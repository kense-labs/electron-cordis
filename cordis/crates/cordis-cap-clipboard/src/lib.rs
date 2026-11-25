//! `@dsh/cap-clipboard` — Class-A OS capability plugin (Chromium-free Rust DSH plugin).
//!
//! Security invariants:
//! - `availableFormats` returns ONLY MIME type strings (e.g. `["text/plain"]`), NEVER leaking
//!   clipboard body text in its result or metadata.
//! - Provides `ClipboardService` as a Cordis v3 `Service` and registers Plane B DSH tools
//!   (`clipboard.available_formats`, `clipboard.read_text`, `clipboard.write_text`) with
//!   automatic reverse-order cleanup via `ctx.effect_sync` on Fiber disposal.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, RwLock};

use cordis_core::{Context, Plugin, Service};
use electron_cordis_kernel::{RegisteredTool, ToolCatalog};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

pub const PLUGIN_ID: &str = "@dsh/cap-clipboard";
pub const SERVICE_NAME: &str = "cap.clipboard";

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("invalid clipboard arguments: {0}")]
    InvalidArgs(String),
    #[error("unknown clipboard method: {0}")]
    UnknownMethod(String),
    #[error("clipboard plugin lifecycle error: {0}")]
    Runtime(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClipboardMode {
    /// Deterministic isolated in-memory clipboard (default for headless/CI and side-by-side tests).
    #[default]
    Memory,
    /// Uses OS pasteboard commands (`pbcopy`/`pbpaste` on macOS) via fixed argv (no shell).
    OsNative,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClipboardConfig {
    #[serde(default)]
    pub mode: ClipboardMode,
}

/// Thread-safe clipboard capability service.
#[derive(Clone)]
pub struct ClipboardService {
    mode: ClipboardMode,
    buffers: Arc<RwLock<BTreeMap<String, String>>>,
}

impl Service for ClipboardService {
    const NAME: &'static str = SERVICE_NAME;
}

impl Default for ClipboardService {
    fn default() -> Self {
        Self::new(ClipboardMode::Memory)
    }
}

impl ClipboardService {
    pub fn new(mode: ClipboardMode) -> Self {
        Self {
            mode,
            buffers: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Returns ONLY MIME format identifiers currently present in the clipboard.
    /// Security invariant: NEVER returns or embeds clipboard text body.
    pub fn available_formats(&self) -> Vec<String> {
        let guard = self.buffers.read().expect("clipboard lock");
        guard.keys().cloned().collect()
    }

    pub fn write_text(&self, text: String) -> Result<(), ClipboardError> {
        if self.mode == ClipboardMode::OsNative {
            #[cfg(target_os = "macos")]
            {
                use std::io::Write;
                use std::process::{Command, Stdio};
                let mut child = Command::new("/usr/bin/pbcopy")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| ClipboardError::Runtime(format!("pbcopy spawn: {e}")))?;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin
                        .write_all(text.as_bytes())
                        .map_err(|e| ClipboardError::Runtime(format!("pbcopy write: {e}")))?;
                }
                let _ = child.wait();
            }
        }
        let mut guard = self.buffers.write().expect("clipboard lock");
        if text.is_empty() {
            guard.remove("text/plain");
        } else {
            guard.insert("text/plain".to_string(), text);
        }
        Ok(())
    }

    pub fn read_text(&self) -> Result<String, ClipboardError> {
        if self.mode == ClipboardMode::OsNative {
            #[cfg(target_os = "macos")]
            {
                use std::process::Command;
                let out = Command::new("/usr/bin/pbpaste")
                    .output()
                    .map_err(|e| ClipboardError::Runtime(format!("pbpaste exec: {e}")))?;
                if out.status.success() {
                    return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
                }
            }
        }
        let guard = self.buffers.read().expect("clipboard lock");
        Ok(guard.get("text/plain").cloned().unwrap_or_default())
    }

    pub fn clear(&self) {
        let mut guard = self.buffers.write().expect("clipboard lock");
        guard.clear();
    }

    pub fn write_html(&self, html: String) {
        let mut guard = self.buffers.write().expect("clipboard lock");
        if html.is_empty() {
            guard.remove("text/html");
        } else {
            guard.insert("text/html".to_string(), html);
        }
    }

    pub fn read_html(&self) -> String {
        let guard = self.buffers.read().expect("clipboard lock");
        guard.get("text/html").cloned().unwrap_or_default()
    }

    pub fn write_image_base64(&self, png_base64: String) {
        let mut guard = self.buffers.write().expect("clipboard lock");
        if png_base64.is_empty() {
            guard.remove("image/png");
        } else {
            guard.insert("image/png".to_string(), png_base64);
        }
    }

    pub fn read_image_base64(&self) -> String {
        let guard = self.buffers.read().expect("clipboard lock");
        guard.get("image/png").cloned().unwrap_or_default()
    }

    /// JSON-RPC compat dispatch (`cordis.compat.call` for capability `"clipboard"`).
    pub fn call_compat(&self, method: &str, args: &Value) -> Result<Value, ClipboardError> {
        match method {
            "availableFormats" => {
                // Strictly return ONLY format names (no body content).
                Ok(json!(self.available_formats()))
            }
            "readText" => Ok(json!(self.read_text()?)),
            "writeText" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ClipboardError::InvalidArgs("missing string `text`".into()))?;
                self.write_text(text.to_string())?;
                Ok(json!({ "written": true }))
            }
            "readHTML" => Ok(json!(self.read_html())),
            "writeHTML" => {
                let html = args
                    .get("markup")
                    .or_else(|| args.get("html"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| ClipboardError::InvalidArgs("missing string `markup`".into()))?;
                self.write_html(html.to_string());
                Ok(json!({ "written": true }))
            }
            "readImageBase64" => Ok(json!(self.read_image_base64())),
            "writeImageBase64" => {
                let data = args
                    .get("pngBase64")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ClipboardError::InvalidArgs("missing string `pngBase64`".into())
                    })?;
                self.write_image_base64(data.to_string());
                Ok(json!({ "written": true }))
            }
            "clear" => {
                self.clear();
                Ok(json!({ "cleared": true }))
            }
            other => Err(ClipboardError::UnknownMethod(other.to_string())),
        }
    }

    /// Invokes a Plane B projected DSH tool owned by `@dsh/cap-clipboard`.
    pub fn invoke_dsh_tool(&self, tool_name: &str, args: &Value) -> Result<Value, ClipboardError> {
        match tool_name {
            "clipboard.available_formats" => Ok(json!({ "formats": self.available_formats() })),
            "clipboard.read_text" => Ok(json!({ "text": self.read_text()? })),
            "clipboard.write_text" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ClipboardError::InvalidArgs("missing string `text`".into()))?;
                self.write_text(text.to_string())?;
                Ok(json!({ "ok": true, "formats": self.available_formats() }))
            }
            other => Err(ClipboardError::UnknownMethod(other.to_string())),
        }
    }
}

pub fn dsh_tool_specs() -> Vec<RegisteredTool> {
    vec![
        RegisteredTool {
            name: "clipboard.available_formats".into(),
            version: "1".into(),
            origin_plugin: PLUGIN_ID.into(),
            description: "Inspect available clipboard MIME formats without exposing clipboard body text."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            requires_approval: false,
        },
        RegisteredTool {
            name: "clipboard.read_text".into(),
            version: "1".into(),
            origin_plugin: PLUGIN_ID.into(),
            description: "Read plain text from the host clipboard.".into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            requires_approval: true,
        },
        RegisteredTool {
            name: "clipboard.write_text".into(),
            version: "1".into(),
            origin_plugin: PLUGIN_ID.into(),
            description: "Write plain text to the host clipboard.".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
                "additionalProperties": false
            }),
            requires_approval: true,
        },
    ]
}

/// Cordis v3 plugin wiring `ClipboardService` into `HostRealm` and registering Plane B tools.
#[derive(Clone)]
pub struct ClipboardPlugin {
    catalog: Option<ToolCatalog>,
}

impl ClipboardPlugin {
    pub fn new(catalog: Option<ToolCatalog>) -> Self {
        Self { catalog }
    }
}

impl Plugin for ClipboardPlugin {
    type Config = ClipboardConfig;
    type Input = ClipboardConfig;
    type PrepareError = Infallible;
    type ApplyError = ClipboardError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(ClipboardService::new(input.mode));
        let publication = ctx
            .provide(svc)
            .map_err(|e| ClipboardError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = publication.remove();
        })
        .map_err(|e| ClipboardError::Runtime(e.to_string()))?;

        if let Some(catalog) = self.catalog.clone() {
            for tool in dsh_tool_specs() {
                catalog.register(tool);
            }
            ctx.effect_sync(move || {
                catalog.unregister_by_plugin(PLUGIN_ID);
            })
            .map_err(|e| ClipboardError::Runtime(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use cordis_core::PreparedPlugin;
    use electron_cordis_kernel::RealmTopology;

    use super::*;

    #[tokio::test]
    async fn available_formats_never_leaks_clipboard_body_and_disposes_cleanly() {
        let root = Context::new();
        let topo = RealmTopology::new(root).expect("topology");
        let catalog = ToolCatalog::default();

        let handle = topo
            .host
            .spawn(PreparedPlugin::from_input(
                ClipboardPlugin::new(Some(catalog.clone())),
                ClipboardConfig::default(),
            ))
            .await
            .expect("spawn clipboard plugin");

        let svc = topo
            .host
            .try_service::<ClipboardService>()
            .expect("clipboard service");

        let secret = "SUPER_SECRET_CLIPBOARD_TOKEN_98765";
        svc.call_compat("writeText", &json!({ "text": secret }))
            .expect("writeText");

        let formats = svc
            .call_compat("availableFormats", &json!({}))
            .expect("availableFormats");
        let serialized = serde_json::to_string(&formats).unwrap();
        assert_eq!(formats, json!(["text/plain"]));
        assert!(
            !serialized.contains(secret),
            "availableFormats MUST NOT leak clipboard body text"
        );

        let read = svc.call_compat("readText", &json!({})).expect("readText");
        assert_eq!(read, json!(secret));

        assert_eq!(catalog.list().len(), 3);

        handle.dispose().await.expect("dispose");
        assert!(topo.host.try_service::<ClipboardService>().is_err());
        assert!(catalog.list().is_empty(), "tools must unregister on dispose");
    }
}
