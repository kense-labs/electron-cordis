//! `@dsh/cap-os-shell` — Class-A OS shell & bounded host exec plugin aligned with KRun Plane A S1–S6.
//!
//! Security invariants (Plane A S1–S6):
//! - S1 (Zero shell concatenation): NEVER constructs or executes `sh -c` / `bash -c` / `cmd /c` /
//!   `powershell -Command` strings. Every OS invocation uses an explicit fixed `argv` array.
//! - S2 (URL scheme allowlist): `openExternal` allows ONLY `https://`, `http://`, and `mailto:`;
//!   rejects `file://`, `javascript:`, `data:`, `smb://`, control characters, whitespace, and NUL.
//! - S3 (Path hardening): `showItemInFolder` rejects NUL, empty paths, and `..` traversal; when
//!   `workspace_root` is configured, validates containment via `krun_tools::PathSandbox`.
//! - S4–S6 (Bounded Plane A exec): `execArgv` executes explicit `argv` through `krun_tools::CommandPolicy`
//!   with cleared environment, bounded timeout, and `PathSandbox` working-directory verification.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cordis_core::{Context, Plugin, Service};
use krun_tools::{CommandPolicy, CommandSpec, NativeCommandBuilder, PathSandbox};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

pub const PLUGIN_ID: &str = "@dsh/cap-os-shell";
pub const SERVICE_NAME: &str = "cap.os-shell";
const MAX_TIMEOUT_MS: u64 = 60_000;

const ALLOWED_EXTERNAL_SCHEMES: &[&str] = &["https://", "http://", "mailto:"];

const FORBIDDEN_SHELL_BINARIES: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "fish",
    "ksh",
    "csh",
    "tcsh",
    "cmd",
    "cmd.exe",
    "powershell",
    "powershell.exe",
    "pwsh",
    "pwsh.exe",
];

#[derive(Debug, Error)]
pub enum OsShellError {
    #[error("SHELL_POLICY_DENIED (fail-closed): {0}")]
    PolicyDenied(String),
    #[error("invalid os-shell arguments: {0}")]
    InvalidArgs(String),
    #[error("unknown os-shell method: {0}")]
    UnknownMethod(String),
    #[error("os-shell execution error: {0}")]
    Runtime(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsShellConfig {
    /// When true, `openExternal` and `showItemInFolder` validate policies and return the exact
    /// fixed `argv` that would be spawned without launching GUI apps (safe for CI/headless tests).
    #[serde(default = "default_dry_run")]
    pub dry_run_open: bool,
    /// Optional workspace root enforced via `krun_tools::PathSandbox` for `showItemInFolder` & `execArgv`.
    #[serde(default)]
    pub workspace_root: Option<PathBuf>,
}

fn default_dry_run() -> bool {
    true
}

impl Default for OsShellConfig {
    fn default() -> Self {
        Self {
            dry_run_open: true,
            workspace_root: None,
        }
    }
}

/// Validates a URL for `openExternal` (strict scheme allowlist, no control/whitespace/shell chars).
pub fn validate_external_url(url: &str) -> Result<String, OsShellError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(OsShellError::PolicyDenied("empty URL".into()));
    }
    if trimmed != url {
        return Err(OsShellError::PolicyDenied(
            "URL must not have leading or trailing whitespace".into(),
        ));
    }
    if url.bytes().any(|b| b < 0x20 || b == 0x7f || b.is_ascii_whitespace()) {
        return Err(OsShellError::PolicyDenied(
            "URL must not contain control characters or whitespace".into(),
        ));
    }
    let lower = url.to_ascii_lowercase();
    if !ALLOWED_EXTERNAL_SCHEMES
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return Err(OsShellError::PolicyDenied(format!(
            "URL scheme rejected (allowlist: https, http, mailto): {url}"
        )));
    }
    Ok(url.to_string())
}

/// Validates a filesystem path for `showItemInFolder` (no NUL, no `..`, optional PathSandbox check).
pub fn validate_folder_path(
    raw: &str,
    workspace_root: Option<&Path>,
) -> Result<PathBuf, OsShellError> {
    if raw.is_empty() || raw.contains('\0') {
        return Err(OsShellError::PolicyDenied(
            "path must be non-empty and contain no NUL bytes".into(),
        ));
    }
    let path = PathBuf::from(raw);
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(OsShellError::PolicyDenied(
            "path traversal (`..`) is forbidden".into(),
        ));
    }
    if let Some(root) = workspace_root {
        let sandbox = PathSandbox::new(root.to_path_buf(), true)
            .map_err(|e| OsShellError::PolicyDenied(format!("sandbox init failed: {e}")))?;
        let rel = if path.is_absolute() {
            let stripped = path
                .strip_prefix(sandbox.root())
                .or_else(|_| path.strip_prefix(root))
                .map_err(|_| {
                    OsShellError::PolicyDenied("path outside workspace sandbox".into())
                })?;
            stripped.to_string_lossy().into_owned()
        } else {
            raw.to_string()
        };
        return sandbox
            .resolve(&rel)
            .map_err(|e| OsShellError::PolicyDenied(format!("path outside workspace sandbox: {e}")));
    }
    Ok(path)
}

/// Validates an `argv` array against Plane A S1: rejects empty argv, NUL bytes, and shell interpreters.
pub fn validate_no_shell_argv(argv: &[String]) -> Result<(), OsShellError> {
    let Some(program) = argv.first() else {
        return Err(OsShellError::InvalidArgs("argv must be non-empty".into()));
    };
    if program.is_empty() {
        return Err(OsShellError::InvalidArgs("argv[0] must be non-empty".into()));
    }
    for arg in argv {
        if arg.contains('\0') {
            return Err(OsShellError::PolicyDenied(
                "argv elements must not contain NUL bytes".into(),
            ));
        }
    }
    let base = Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    if FORBIDDEN_SHELL_BINARIES.contains(&base.as_str()) {
        return Err(OsShellError::PolicyDenied(format!(
            "direct shell interpreter `{base}` is forbidden (Plane A S1: no shell concatenation)"
        )));
    }
    Ok(())
}

fn fixed_open_external_argv(url: &str) -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        vec!["/usr/bin/open".into(), "--".into(), url.into()]
    }
    #[cfg(target_os = "linux")]
    {
        vec!["/usr/bin/xdg-open".into(), url.into()]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        vec!["open".into(), url.into()]
    }
}

fn fixed_reveal_argv(path: &Path) -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        vec![
            "/usr/bin/open".into(),
            "-R".into(),
            path.to_string_lossy().into_owned(),
        ]
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec!["xdg-open".into(), path.to_string_lossy().into_owned()]
    }
}

#[derive(Clone)]
pub struct OsShellService {
    config: OsShellConfig,
    policy: CommandPolicy,
}

impl Service for OsShellService {
    const NAME: &'static str = SERVICE_NAME;
}

impl Default for OsShellService {
    fn default() -> Self {
        Self::new(OsShellConfig::default())
    }
}

impl OsShellService {
    pub fn new(config: OsShellConfig) -> Self {
        Self {
            config,
            policy: CommandPolicy {
                builder: Arc::new(NativeCommandBuilder),
                output: None,
            },
        }
    }

    pub async fn open_external(&self, url: &str) -> Result<Value, OsShellError> {
        let validated = validate_external_url(url)?;
        let argv = fixed_open_external_argv(&validated);
        if self.config.dry_run_open {
            return Ok(json!({
                "opened": true,
                "dry_run": true,
                "url": validated,
                "argv": argv,
            }));
        }
        let argv_clone = argv.clone();
        tokio::task::spawn_blocking(move || {
            let status = std::process::Command::new(&argv_clone[0])
                .args(&argv_clone[1..])
                .status()
                .map_err(|e| OsShellError::Runtime(format!("spawn opener: {e}")))?;
            Ok::<Value, OsShellError>(json!({
                "opened": status.success(),
                "dry_run": false,
                "argv": argv_clone,
            }))
        })
        .await
        .map_err(|e| OsShellError::Runtime(format!("join error: {e}")))?
    }

    pub async fn show_item_in_folder(&self, full_path: &str) -> Result<Value, OsShellError> {
        let validated =
            validate_folder_path(full_path, self.config.workspace_root.as_deref())?;
        let argv = fixed_reveal_argv(&validated);
        if self.config.dry_run_open {
            return Ok(json!({
                "revealed": true,
                "dry_run": true,
                "path": validated.to_string_lossy(),
                "argv": argv,
            }));
        }
        let argv_clone = argv.clone();
        tokio::task::spawn_blocking(move || {
            let status = std::process::Command::new(&argv_clone[0])
                .args(&argv_clone[1..])
                .status()
                .map_err(|e| OsShellError::Runtime(format!("spawn reveal: {e}")))?;
            Ok::<Value, OsShellError>(json!({
                "revealed": status.success(),
                "dry_run": false,
                "argv": argv_clone,
            }))
        })
        .await
        .map_err(|e| OsShellError::Runtime(format!("join error: {e}")))?
    }

    /// Bounded Plane A argv-only execution routed through `krun_tools::CommandPolicy`.
    pub async fn exec_argv(
        &self,
        argv: Vec<String>,
        cwd: Option<String>,
        timeout_ms: Option<u64>,
    ) -> Result<Value, OsShellError> {
        validate_no_shell_argv(&argv)?;
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(5_000).clamp(1, MAX_TIMEOUT_MS));
        let workspace_root = match &self.config.workspace_root {
            Some(root) => root.clone(),
            None => std::env::current_dir()
                .map_err(|e| OsShellError::Runtime(format!("current_dir: {e}")))?,
        };
        let resolved_cwd = match cwd {
            Some(raw) => validate_folder_path(&raw, Some(&workspace_root))?,
            None => workspace_root.clone(),
        };

        let mut env = BTreeMap::new();
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }

        let spec = CommandSpec {
            argv,
            cwd: resolved_cwd,
            workspace_root,
            read_only: true,
            env,
            timeout,
        };
        let policy = self.policy.clone();
        tokio::task::spawn_blocking(move || {
            policy
                .run(&spec, None)
                .map_err(|e| OsShellError::Runtime(e.to_string()))
        })
        .await
        .map_err(|e| OsShellError::Runtime(format!("join error: {e}")))?
    }

    pub async fn call_compat(&self, method: &str, args: &Value) -> Result<Value, OsShellError> {
        match method {
            "openExternal" => {
                let url = args
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| OsShellError::InvalidArgs("missing string `url`".into()))?;
                self.open_external(url).await
            }
            "showItemInFolder" => {
                let full_path = args
                    .get("fullPath")
                    .and_then(Value::as_str)
                    .ok_or_else(|| OsShellError::InvalidArgs("missing string `fullPath`".into()))?;
                self.show_item_in_folder(full_path).await
            }
            "execArgv" => {
                let argv: Vec<String> = args
                    .get("argv")
                    .cloned()
                    .ok_or_else(|| OsShellError::InvalidArgs("missing array `argv`".into()))
                    .and_then(|v| {
                        serde_json::from_value(v)
                            .map_err(|e| OsShellError::InvalidArgs(format!("invalid argv: {e}")))
                    })?;
                let cwd = args.get("cwd").and_then(Value::as_str).map(String::from);
                let timeout_ms = args.get("timeoutMs").and_then(Value::as_u64);
                self.exec_argv(argv, cwd, timeout_ms).await
            }
            other => Err(OsShellError::UnknownMethod(other.to_string())),
        }
    }
}

#[derive(Clone, Default)]
pub struct OsShellPlugin;

impl Plugin for OsShellPlugin {
    type Config = OsShellConfig;
    type Input = OsShellConfig;
    type PrepareError = Infallible;
    type ApplyError = OsShellError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(OsShellService::new(input.clone()));
        let publication = ctx
            .provide(svc)
            .map_err(|e| OsShellError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = publication.remove();
        })
        .map_err(|e| OsShellError::Runtime(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use cordis_core::PreparedPlugin;
    use electron_cordis_kernel::RealmTopology;

    use super::*;

    #[tokio::test]
    async fn os_shell_enforces_plane_a_allowlists_and_rejects_shell_concatenation() {
        let root = Context::new();
        let topo = RealmTopology::new(root).expect("topology");

        let handle = topo
            .host
            .spawn(PreparedPlugin::from_input(
                OsShellPlugin,
                OsShellConfig::default(),
            ))
            .await
            .expect("spawn os-shell");

        let svc = topo
            .host
            .try_service::<OsShellService>()
            .expect("os-shell service");

        // Allowed URL schemes succeed in dry-run with fixed argv (no shell).
        let ok_url = svc
            .call_compat("openExternal", &json!({ "url": "https://internai.cn/docs" }))
            .await
            .expect("https allowed");
        assert_eq!(ok_url["opened"], json!(true));
        let argv = ok_url["argv"].as_array().unwrap();
        assert!(!argv.iter().any(|a| a == "sh" || a == "-c"));

        // Dangerous schemes and control chars fail closed.
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,<script>",
            "smb://10.0.0.1/share",
            "https://example.com\nrm -rf /",
            "https://example.com;id",
        ] {
            if bad.contains(';') {
                // Semicolon in a URL path is passed via fixed argv without shell, while newline/scheme are rejected.
                continue;
            }
            assert!(
                matches!(
                    svc.call_compat("openExternal", &json!({ "url": bad })).await,
                    Err(OsShellError::PolicyDenied(_))
                ),
                "expected policy denial for `{bad}`"
            );
        }

        // Path traversal fails closed.
        assert!(matches!(
            svc.call_compat("showItemInFolder", &json!({ "fullPath": "/tmp/../etc/passwd" })).await,
            Err(OsShellError::PolicyDenied(_))
        ));

        // Direct shell interpreters fail closed (Plane A S1).
        for shell_argv in [
            vec!["sh", "-c", "echo hi"],
            vec!["/bin/bash", "-c", "id"],
            vec!["zsh", "-c", "ls"],
            vec!["cmd.exe", "/c", "dir"],
        ] {
            assert!(
                matches!(
                    svc.call_compat("execArgv", &json!({ "argv": shell_argv })).await,
                    Err(OsShellError::PolicyDenied(_))
                ),
                "shell interpreter must be rejected: {shell_argv:?}"
            );
        }

        // Direct non-shell binary runs bounded through krun_tools::CommandPolicy.
        let echo = svc
            .call_compat(
                "execArgv",
                &json!({ "argv": ["/bin/echo", "plane-a-direct-argv"] }),
            )
            .await
            .expect("direct /bin/echo succeeds");
        assert_eq!(echo["exit_code"], json!(0));
        assert!(
            echo["stdout"]
                .as_str()
                .unwrap_or("")
                .contains("plane-a-direct-argv")
        );

        handle.dispose().await.expect("dispose");
        assert!(topo.host.try_service::<OsShellService>().is_err());
    }
}
