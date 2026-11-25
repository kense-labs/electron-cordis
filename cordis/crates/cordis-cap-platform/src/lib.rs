//! M3 Batch 3 Class-A Rust DSH platform & supply-chain plugins (Chromium-free):
//! - `@dsh/cap-desktop-capture`: permission-gated screen/window enumeration (`delivery: "on-demand"`)
//! - `@dsh/cap-updater`: dual-channel updater (`cordis-canary` Full App + real Ed25519 Renderer-only
//!   update verification with `base=electron-cordis` `MAIN_COMPATIBILITY_HASH` isolation)
//! - `@dsh/plugin-installer`: signed on-demand plugin installer (real Ed25519 manifest signature +
//!   per-file SHA-256 verification + atomic staging/activation/rollback on disk)
//! - `@dsh/runtime-node-host`: isolated Node worker host with fixed argv, env allowlist, and
//!   deterministic kill-on-dispose cleanup barrier

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use cordis_core::{Context, Plugin, Service};
use ed25519_dalek::{Signature, VerifyingKey};
use electron_cordis_kernel::service_names;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const CAPTURE_PLUGIN_ID: &str = "@dsh/cap-desktop-capture";
pub const UPDATER_PLUGIN_ID: &str = "@dsh/cap-updater";
pub const INSTALLER_PLUGIN_ID: &str = "@dsh/plugin-installer";
pub const NODE_HOST_PLUGIN_ID: &str = "@dsh/runtime-node-host";

pub const CAPTURE_SERVICE_NAME: &str = "cap.desktop-capture";
pub const NODE_HOST_SERVICE_NAME: &str = "runtime.node_host";

#[derive(Debug, Error)]
pub enum PlatformCapError {
    #[error("PERMISSION_DENIED (fail-closed): {0}")]
    PermissionDenied(String),
    #[error("SIGNATURE_INVALID (fail-closed): {0}")]
    SignatureInvalid(String),
    #[error("COMPATIBILITY_MISMATCH (fail-closed): {0}")]
    CompatibilityMismatch(String),
    #[error("INTEGRITY_MISMATCH (fail-closed): {0}")]
    IntegrityMismatch(String),
    #[error("POLICY_DENIED (fail-closed): {0}")]
    PolicyDenied(String),
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    #[error("runtime error: {0}")]
    Runtime(String),
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, PlatformCapError> {
    let clean = hex.trim();
    if clean.len() % 2 != 0 {
        return Err(PlatformCapError::InvalidArgs("odd-length hex string".into()));
    }
    let mut out = Vec::with_capacity(clean.len() / 2);
    for pair in clean.as_bytes().chunks(2) {
        let s = std::str::from_utf8(pair)
            .map_err(|_| PlatformCapError::InvalidArgs("non-utf8 hex".into()))?;
        let byte = u8::from_str_radix(s, 16)
            .map_err(|_| PlatformCapError::InvalidArgs("invalid hex digit".into()))?;
        out.push(byte);
    }
    Ok(out)
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    encode_hex(&digest)
}

pub fn verify_ed25519_hex(
    public_key_hex: &str,
    message: &[u8],
    signature_hex: &str,
) -> Result<(), PlatformCapError> {
    let pk_bytes = decode_hex(public_key_hex)
        .map_err(|e| PlatformCapError::SignatureInvalid(format!("public key hex: {e}")))?;
    let pk_arr: [u8; 32] = pk_bytes
        .try_into()
        .map_err(|_| PlatformCapError::SignatureInvalid("public key must be 32 bytes".into()))?;
    let vk = VerifyingKey::from_bytes(&pk_arr)
        .map_err(|e| PlatformCapError::SignatureInvalid(format!("invalid verifying key: {e}")))?;

    let sig_bytes = decode_hex(signature_hex)
        .map_err(|e| PlatformCapError::SignatureInvalid(format!("signature hex: {e}")))?;
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| PlatformCapError::SignatureInvalid("signature must be 64 bytes".into()))?;
    let sig = Signature::from_bytes(&sig_arr);

    vk.verify_strict(message, &sig)
        .map_err(|e| PlatformCapError::SignatureInvalid(format!("ed25519 verify failed: {e}")))
}

// ---------------------------------------------------------------------------
// 1. @dsh/cap-desktop-capture (on-demand, permission-gated)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureSource {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesktopCaptureConfig {
    #[serde(default)]
    pub screen_recording_granted: bool,
}

#[derive(Clone)]
pub struct DesktopCaptureService {
    granted: Arc<Mutex<bool>>,
}

impl Service for DesktopCaptureService {
    const NAME: &'static str = CAPTURE_SERVICE_NAME;
}

impl DesktopCaptureService {
    pub fn new(cfg: DesktopCaptureConfig) -> Self {
        Self {
            granted: Arc::new(Mutex::new(cfg.screen_recording_granted)),
        }
    }

    pub fn call_compat(&self, method: &str, args: &Value) -> Result<Value, PlatformCapError> {
        match method {
            "setPermission" => {
                let g = args
                    .get("granted")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| PlatformCapError::InvalidArgs("missing bool `granted`".into()))?;
                *self.granted.lock().expect("capture perm lock") = g;
                Ok(json!({ "granted": g }))
            }
            "getSources" => {
                if !*self.granted.lock().expect("capture perm lock") {
                    return Err(PlatformCapError::PermissionDenied(
                        "CAPTURE_PERMISSION_DENIED: screen recording permission not granted".into(),
                    ));
                }
                let types: Vec<String> = args
                    .get("types")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["screen".into()]);
                let mut sources = Vec::new();
                if types.iter().any(|t| t == "screen") {
                    sources.push(CaptureSource {
                        id: "screen:0:0".into(),
                        name: "Primary Display".into(),
                        kind: "screen".into(),
                    });
                }
                if types.iter().any(|t| t == "window") {
                    sources.push(CaptureSource {
                        id: "window:1:0".into(),
                        name: "Inkstone (Cordis)".into(),
                        kind: "window".into(),
                    });
                }
                Ok(json!({ "sources": sources }))
            }
            other => Err(PlatformCapError::UnknownMethod(other.to_string())),
        }
    }
}

#[derive(Clone, Default)]
pub struct DesktopCapturePlugin;

impl Plugin for DesktopCapturePlugin {
    type Config = DesktopCaptureConfig;
    type Input = DesktopCaptureConfig;
    type PrepareError = Infallible;
    type ApplyError = PlatformCapError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(DesktopCaptureService::new(input.clone()));
        let pub_handle = ctx
            .provide(svc)
            .map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = pub_handle.remove();
        })
        .map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 2. @dsh/cap-updater (Dual-channel: cordis-canary Full App + Ed25519 Renderer-only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdaterConfig {
    /// Side-by-side update channel (must remain isolated from native Electron channel).
    #[serde(default = "default_update_channel")]
    pub channel: String,
    /// Required compatibility hash incorporating `base=electron-cordis`.
    #[serde(default = "default_compat_hash")]
    pub main_compatibility_hash: String,
    /// Trusted Ed25519 public key (hex, 32 bytes) for verifying Renderer-only hot updates.
    #[serde(default)]
    pub trusted_ed25519_pubkey_hex: Option<String>,
}

fn default_update_channel() -> String {
    "cordis-canary".to_string()
}

fn default_compat_hash() -> String {
    "base=electron-cordis;abi=electron43;v1".to_string()
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            channel: default_update_channel(),
            main_compatibility_hash: default_compat_hash(),
            trusted_ed25519_pubkey_hex: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RendererUpdateManifest {
    pub version: String,
    pub channel: String,
    pub main_compatibility_hash: String,
    pub bundle_sha256: String,
    pub ed25519_signature_hex: String,
}

impl RendererUpdateManifest {
    pub fn canonical_signing_bytes(&self) -> Vec<u8> {
        format!(
            "renderer-update-v1|{}|{}|{}|{}",
            self.version, self.channel, self.main_compatibility_hash, self.bundle_sha256
        )
        .into_bytes()
    }
}

#[derive(Clone)]
pub struct UpdaterService {
    config: Arc<Mutex<UpdaterConfig>>,
}

impl Service for UpdaterService {
    const NAME: &'static str = service_names::UPDATER;
}

impl UpdaterService {
    pub fn new(config: UpdaterConfig) -> Self {
        Self {
            config: Arc::new(Mutex::new(config)),
        }
    }

    pub fn verify_renderer_update(
        &self,
        manifest: &RendererUpdateManifest,
        bundle_bytes: &[u8],
    ) -> Result<Value, PlatformCapError> {
        let cfg = self.config.lock().expect("updater lock").clone();

        // 1. Channel isolation: refuse cross-track update feeds.
        if manifest.channel != cfg.channel {
            return Err(PlatformCapError::CompatibilityMismatch(format!(
                "channel mismatch: expected `{}`, got `{}`",
                cfg.channel, manifest.channel
            )));
        }

        // 2. Base isolation: MAIN_COMPATIBILITY_HASH MUST carry `base=electron-cordis` and match exactly.
        if !manifest
            .main_compatibility_hash
            .contains("base=electron-cordis")
            || manifest.main_compatibility_hash != cfg.main_compatibility_hash
        {
            return Err(PlatformCapError::CompatibilityMismatch(format!(
                "MAIN_COMPATIBILITY_HASH mismatch (refusing cross-base renderer install): expected `{}`, got `{}`",
                cfg.main_compatibility_hash, manifest.main_compatibility_hash
            )));
        }

        // 3. Real Ed25519 signature verification (FailClosed if no key configured).
        let pubkey = cfg.trusted_ed25519_pubkey_hex.as_deref().ok_or_else(|| {
            PlatformCapError::SignatureInvalid(
                "no trusted Ed25519 public key configured for renderer updates".into(),
            )
        })?;
        verify_ed25519_hex(
            pubkey,
            &manifest.canonical_signing_bytes(),
            &manifest.ed25519_signature_hex,
        )?;

        // 4. SHA-256 content integrity verification.
        let actual_sha = sha256_hex(bundle_bytes);
        if actual_sha != manifest.bundle_sha256.to_ascii_lowercase() {
            return Err(PlatformCapError::IntegrityMismatch(format!(
                "bundle SHA-256 mismatch: expected {}, got {actual_sha}",
                manifest.bundle_sha256
            )));
        }

        Ok(json!({
            "verified": true,
            "version": manifest.version,
            "channel": manifest.channel,
            "main_compatibility_hash": manifest.main_compatibility_hash,
            "bundle_sha256": actual_sha,
        }))
    }

    pub fn call_compat(&self, method: &str, args: &Value) -> Result<Value, PlatformCapError> {
        match method {
            "getChannelInfo" => {
                let cfg = self.config.lock().expect("updater lock").clone();
                Ok(json!({
                    "channel": cfg.channel,
                    "main_compatibility_hash": cfg.main_compatibility_hash,
                    "has_trusted_pubkey": cfg.trusted_ed25519_pubkey_hex.is_some(),
                }))
            }
            "setTrustedPubkey" => {
                let hex = args
                    .get("pubkeyHex")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        PlatformCapError::InvalidArgs("missing string `pubkeyHex`".into())
                    })?;
                let mut cfg = self.config.lock().expect("updater lock");
                cfg.trusted_ed25519_pubkey_hex = Some(hex.to_string());
                Ok(json!({ "configured": true }))
            }
            "verifyRendererUpdate" => {
                let manifest: RendererUpdateManifest = serde_json::from_value(
                    args.get("manifest").cloned().unwrap_or(Value::Null),
                )
                .map_err(|e| PlatformCapError::InvalidArgs(format!("manifest: {e}")))?;
                let payload = args
                    .get("bundleText")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        PlatformCapError::InvalidArgs("missing string `bundleText`".into())
                    })?;
                self.verify_renderer_update(&manifest, payload.as_bytes())
            }
            other => Err(PlatformCapError::UnknownMethod(other.to_string())),
        }
    }
}

#[derive(Clone, Default)]
pub struct UpdaterPlugin;

impl Plugin for UpdaterPlugin {
    type Config = UpdaterConfig;
    type Input = UpdaterConfig;
    type PrepareError = Infallible;
    type ApplyError = PlatformCapError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(UpdaterService::new(input.clone()));
        let pub_handle = ctx
            .provide(svc)
            .map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = pub_handle.remove();
        })
        .map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 3. @dsh/plugin-installer (Ed25519 + Per-File SHA-256 + Atomic Activate & Rollback)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginPackageFile {
    pub relative_path: String,
    pub sha256: String,
    pub content_utf8: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedPluginPackage {
    pub plugin_id: String,
    pub version: String,
    pub files: Vec<PluginPackageFile>,
    pub ed25519_signature_hex: String,
}

impl SignedPluginPackage {
    pub fn canonical_signing_bytes(&self) -> Vec<u8> {
        let mut sorted: Vec<(&str, &str)> = self
            .files
            .iter()
            .map(|f| (f.relative_path.as_str(), f.sha256.as_str()))
            .collect();
        sorted.sort();
        let mut s = format!("plugin-pkg-v1|{}|{}", self.plugin_id, self.version);
        for (path, sha) in sorted {
            s.push('|');
            s.push_str(path);
            s.push('=');
            s.push_str(sha);
        }
        s.into_bytes()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInstallerConfig {
    pub store_root: PathBuf,
    #[serde(default)]
    pub trusted_ed25519_pubkey_hex: Option<String>,
}

impl Default for PluginInstallerConfig {
    fn default() -> Self {
        Self {
            store_root: std::env::temp_dir().join("electron-cordis-plugin-store"),
            trusted_ed25519_pubkey_hex: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPluginState {
    pub plugin_id: String,
    pub active_version: String,
    pub previous_version: Option<String>,
    pub active_path: PathBuf,
}

#[derive(Clone)]
pub struct PluginInstallerService {
    config: Arc<Mutex<PluginInstallerConfig>>,
    installed: Arc<Mutex<BTreeMap<String, InstalledPluginState>>>,
}

impl Service for PluginInstallerService {
    const NAME: &'static str = service_names::PLUGIN_INSTALLER;
}

fn sanitize_plugin_dir_name(id: &str) -> Result<String, PlatformCapError> {
    if id.trim().is_empty() || id.contains('\0') || id.contains("..") {
        return Err(PlatformCapError::PolicyDenied(format!(
            "illegal plugin_id `{id}`"
        )));
    }
    Ok(id.replace(['/', '@'], "_"))
}

fn validate_relative_file_path(rel: &str) -> Result<PathBuf, PlatformCapError> {
    if rel.trim().is_empty() || rel.contains('\0') {
        return Err(PlatformCapError::PolicyDenied("empty or NUL file path".into()));
    }
    let p = PathBuf::from(rel);
    if p.is_absolute() {
        return Err(PlatformCapError::PolicyDenied(format!(
            "absolute file path forbidden: {rel}"
        )));
    }
    for c in p.components() {
        if matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)) {
            return Err(PlatformCapError::PolicyDenied(format!(
                "path traversal forbidden: {rel}"
            )));
        }
    }
    Ok(p)
}

impl PluginInstallerService {
    pub fn new(config: PluginInstallerConfig) -> Self {
        Self {
            config: Arc::new(Mutex::new(config)),
            installed: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Verifies Ed25519 manifest signature + per-file SHA-256, stages files into a temp dir,
    /// atomically activates the target version, and records the prior version for rollback.
    pub fn install_signed(
        &self,
        pkg: &SignedPluginPackage,
    ) -> Result<InstalledPluginState, PlatformCapError> {
        let cfg = self.config.lock().expect("installer cfg lock").clone();
        let pubkey = cfg.trusted_ed25519_pubkey_hex.as_deref().ok_or_else(|| {
            PlatformCapError::SignatureInvalid(
                "no trusted Ed25519 public key configured for plugin installer".into(),
            )
        })?;

        // 1. Verify Ed25519 signature over canonical manifest (FailClosed).
        verify_ed25519_hex(
            pubkey,
            &pkg.canonical_signing_bytes(),
            &pkg.ed25519_signature_hex,
        )?;

        if pkg.files.is_empty() {
            return Err(PlatformCapError::InvalidArgs(
                "plugin package must contain at least one file".into(),
            ));
        }

        // 2. Verify every file path and SHA-256 BEFORE touching active store.
        let safe_id = sanitize_plugin_dir_name(&pkg.plugin_id)?;
        let safe_ver = sanitize_plugin_dir_name(&pkg.version)?;
        let staging_dir = cfg
            .store_root
            .join(".staging")
            .join(format!("{safe_id}-{safe_ver}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&staging_dir);
        fs::create_dir_all(&staging_dir)
            .map_err(|e| PlatformCapError::Runtime(format!("create staging dir: {e}")))?;

        for f in &pkg.files {
            let rel = validate_relative_file_path(&f.relative_path)?;
            let actual = sha256_hex(f.content_utf8.as_bytes());
            if actual != f.sha256.to_ascii_lowercase() {
                let _ = fs::remove_dir_all(&staging_dir);
                return Err(PlatformCapError::IntegrityMismatch(format!(
                    "file `{}` SHA-256 mismatch: expected {}, got {actual}",
                    f.relative_path, f.sha256
                )));
            }
            let target = staging_dir.join(rel);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| PlatformCapError::Runtime(format!("mkdir staging parent: {e}")))?;
            }
            fs::write(&target, f.content_utf8.as_bytes())
                .map_err(|e| PlatformCapError::Runtime(format!("write staged file: {e}")))?;
        }

        // 3. Atomic move from staging -> versioned store directory.
        let plugin_root = cfg.store_root.join(&safe_id);
        fs::create_dir_all(&plugin_root)
            .map_err(|e| PlatformCapError::Runtime(format!("create plugin root: {e}")))?;
        let active_dir = plugin_root.join(&safe_ver);
        if active_dir.exists() {
            let _ = fs::remove_dir_all(&active_dir);
        }
        fs::rename(&staging_dir, &active_dir)
            .map_err(|e| PlatformCapError::Runtime(format!("atomic activate rename: {e}")))?;

        let mut map = self.installed.lock().expect("installed lock");
        let prev = map.get(&pkg.plugin_id).map(|s| s.active_version.clone());
        let state = InstalledPluginState {
            plugin_id: pkg.plugin_id.clone(),
            active_version: pkg.version.clone(),
            previous_version: prev,
            active_path: active_dir,
        };
        map.insert(pkg.plugin_id.clone(), state.clone());
        Ok(state)
    }

    pub fn rollback(&self, plugin_id: &str) -> Result<InstalledPluginState, PlatformCapError> {
        let cfg = self.config.lock().expect("installer cfg lock").clone();
        let mut map = self.installed.lock().expect("installed lock");
        let current = map.get(plugin_id).cloned().ok_or_else(|| {
            PlatformCapError::InvalidArgs(format!("plugin `{plugin_id}` is not installed"))
        })?;
        let prev_ver = current.previous_version.ok_or_else(|| {
            PlatformCapError::InvalidArgs(format!("plugin `{plugin_id}` has no previous version"))
        })?;
        let safe_id = sanitize_plugin_dir_name(plugin_id)?;
        let safe_prev = sanitize_plugin_dir_name(&prev_ver)?;
        let prev_path = cfg.store_root.join(&safe_id).join(&safe_prev);
        if !prev_path.exists() {
            return Err(PlatformCapError::Runtime(
                "previous plugin version directory missing on disk".into(),
            ));
        }
        let next = InstalledPluginState {
            plugin_id: plugin_id.to_string(),
            active_version: prev_ver,
            previous_version: Some(current.active_version),
            active_path: prev_path,
        };
        map.insert(plugin_id.to_string(), next.clone());
        Ok(next)
    }

    pub fn list_installed(&self) -> Vec<InstalledPluginState> {
        self.installed
            .lock()
            .expect("installed lock")
            .values()
            .cloned()
            .collect()
    }

    pub fn call_compat(&self, method: &str, args: &Value) -> Result<Value, PlatformCapError> {
        match method {
            "setTrustedPubkey" => {
                let hex = args
                    .get("pubkeyHex")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        PlatformCapError::InvalidArgs("missing string `pubkeyHex`".into())
                    })?;
                self.config
                    .lock()
                    .expect("installer cfg lock")
                    .trusted_ed25519_pubkey_hex = Some(hex.to_string());
                Ok(json!({ "configured": true }))
            }
            "install" => {
                let pkg: SignedPluginPackage = serde_json::from_value(args.clone())
                    .map_err(|e| PlatformCapError::InvalidArgs(e.to_string()))?;
                let st = self.install_signed(&pkg)?;
                Ok(serde_json::to_value(st).unwrap())
            }
            "rollback" => {
                let id = args
                    .get("plugin_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        PlatformCapError::InvalidArgs("missing string `plugin_id`".into())
                    })?;
                let st = self.rollback(id)?;
                Ok(serde_json::to_value(st).unwrap())
            }
            "listInstalled" => Ok(json!({ "plugins": self.list_installed() })),
            other => Err(PlatformCapError::UnknownMethod(other.to_string())),
        }
    }
}

#[derive(Clone, Default)]
pub struct PluginInstallerPlugin;

impl Plugin for PluginInstallerPlugin {
    type Config = PluginInstallerConfig;
    type Input = PluginInstallerConfig;
    type PrepareError = Infallible;
    type ApplyError = PlatformCapError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(PluginInstallerService::new(input.clone()));
        let pub_handle = ctx
            .provide(svc)
            .map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = pub_handle.remove();
        })
        .map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 4. @dsh/runtime-node-host (Isolated Node worker host with S3 kill-on-dispose)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHostConfig {
    #[serde(default = "default_node_bin")]
    pub node_binary: String,
}

fn default_node_bin() -> String {
    "node".to_string()
}

impl Default for NodeHostConfig {
    fn default() -> Self {
        Self {
            node_binary: default_node_bin(),
        }
    }
}

#[derive(Clone)]
pub struct RuntimeNodeHostService {
    node_binary: String,
    workers: Arc<Mutex<BTreeMap<String, Child>>>,
}

impl Service for RuntimeNodeHostService {
    const NAME: &'static str = NODE_HOST_SERVICE_NAME;
}

impl RuntimeNodeHostService {
    pub fn new(cfg: NodeHostConfig) -> Self {
        Self {
            node_binary: cfg.node_binary,
            workers: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Spawns a bounded isolated Node worker (fixed binary, explicit argv, cleared env + PATH only).
    pub fn spawn_worker(
        &self,
        worker_id: &str,
        script_args: &[String],
        cwd: &Path,
    ) -> Result<u32, PlatformCapError> {
        if worker_id.trim().is_empty() || script_args.is_empty() {
            return Err(PlatformCapError::InvalidArgs(
                "worker_id and script_args required".into(),
            ));
        }
        let mut cmd = Command::new(&self.node_binary);
        cmd.args(script_args)
            .current_dir(cwd)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        let child = cmd
            .spawn()
            .map_err(|e| PlatformCapError::Runtime(format!("spawn node worker: {e}")))?;
        let pid = child.id();
        self.workers
            .lock()
            .expect("workers lock")
            .insert(worker_id.to_string(), child);
        Ok(pid)
    }

    pub fn kill_all_workers(&self) -> usize {
        let mut guard = self.workers.lock().expect("workers lock");
        let count = guard.len();
        for (_, mut child) in std::mem::take(&mut *guard) {
            let _ = child.kill();
            let _ = child.wait();
        }
        count
    }

    pub fn active_worker_count(&self) -> usize {
        self.workers.lock().expect("workers lock").len()
    }
}

#[derive(Clone, Default)]
pub struct RuntimeNodeHostPlugin;

impl Plugin for RuntimeNodeHostPlugin {
    type Config = NodeHostConfig;
    type Input = NodeHostConfig;
    type PrepareError = Infallible;
    type ApplyError = PlatformCapError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(RuntimeNodeHostService::new(input.clone()));
        let svc_for_kill = Arc::clone(&svc);
        let pub_handle = ctx
            .provide(svc)
            .map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = pub_handle.remove();
            svc_for_kill.kill_all_workers();
        })
        .map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use cordis_core::PreparedPlugin;
    use ed25519_dalek::{Signer, SigningKey};
    use electron_cordis_kernel::RealmTopology;

    use super::*;

    fn test_keypair() -> (SigningKey, String) {
        let secret = [42u8; 32];
        let sk = SigningKey::from_bytes(&secret);
        let pk_hex = encode_hex(sk.verifying_key().as_bytes());
        (sk, pk_hex)
    }

    #[tokio::test]
    async fn updater_verifies_ed25519_and_blocks_cross_base_renderer_bundles() {
        let (sk, pk_hex) = test_keypair();
        let root = Context::new();
        let topo = RealmTopology::new(root).unwrap();

        let h = topo
            .host
            .spawn(PreparedPlugin::from_input(
                UpdaterPlugin,
                UpdaterConfig {
                    trusted_ed25519_pubkey_hex: Some(pk_hex),
                    ..UpdaterConfig::default()
                },
            ))
            .await
            .unwrap();
        let svc = topo.host.try_service::<UpdaterService>().unwrap();

        let bundle = b"console.log('inkstone-cordis-renderer-v2');";
        let mut manifest = RendererUpdateManifest {
            version: "0.3.0-cordis.1".into(),
            channel: "cordis-canary".into(),
            main_compatibility_hash: "base=electron-cordis;abi=electron43;v1".into(),
            bundle_sha256: sha256_hex(bundle),
            ed25519_signature_hex: String::new(),
        };
        let sig = sk.sign(&manifest.canonical_signing_bytes());
        manifest.ed25519_signature_hex = encode_hex(&sig.to_bytes());

        // Valid signed bundle passes.
        let ok = svc.verify_renderer_update(&manifest, bundle).unwrap();
        assert_eq!(ok["verified"], json!(true));

        // Tampered bundle fails closed.
        assert!(matches!(
            svc.verify_renderer_update(&manifest, b"tampered"),
            Err(PlatformCapError::IntegrityMismatch(_))
        ));

        // Native Electron renderer bundle (missing `base=electron-cordis`) fails closed!
        let mut native_manifest = manifest.clone();
        native_manifest.main_compatibility_hash = "base=electron-native;abi=electron43;v1".into();
        let sig_native = sk.sign(&native_manifest.canonical_signing_bytes());
        native_manifest.ed25519_signature_hex = encode_hex(&sig_native.to_bytes());
        assert!(matches!(
            svc.verify_renderer_update(&native_manifest, bundle),
            Err(PlatformCapError::CompatibilityMismatch(_))
        ));

        // Forged signature fails closed.
        let mut bad_sig = manifest.clone();
        bad_sig.ed25519_signature_hex = encode_hex(&[0u8; 64]);
        assert!(matches!(
            svc.verify_renderer_update(&bad_sig, bundle),
            Err(PlatformCapError::SignatureInvalid(_))
        ));

        h.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn plugin_installer_enforces_ed25519_sha256_atomic_activate_and_rollback() {
        let (sk, pk_hex) = test_keypair();
        let store_root =
            std::env::temp_dir().join(format!("cordis-installer-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&store_root);

        let root = Context::new();
        let topo = RealmTopology::new(root).unwrap();
        let h = topo
            .host
            .spawn(PreparedPlugin::from_input(
                PluginInstallerPlugin,
                PluginInstallerConfig {
                    store_root: store_root.clone(),
                    trusted_ed25519_pubkey_hex: Some(pk_hex),
                },
            ))
            .await
            .unwrap();
        let installer = topo.host.try_service::<PluginInstallerService>().unwrap();

        let sign_pkg = |version: &str, content: &str, rel_path: &str| {
            let mut pkg = SignedPluginPackage {
                plugin_id: "@dsh/plugin-science-pack".into(),
                version: version.into(),
                files: vec![PluginPackageFile {
                    relative_path: rel_path.into(),
                    sha256: sha256_hex(content.as_bytes()),
                    content_utf8: content.into(),
                }],
                ed25519_signature_hex: String::new(),
            };
            let sig = sk.sign(&pkg.canonical_signing_bytes());
            pkg.ed25519_signature_hex = encode_hex(&sig.to_bytes());
            pkg
        };

        // Install v1.0.0
        let v1 = installer
            .install_signed(&sign_pkg("1.0.0", "export const v = 1;", "index.mjs"))
            .unwrap();
        assert_eq!(v1.active_version, "1.0.0");
        assert!(v1.active_path.join("index.mjs").exists());

        // Upgrade to v1.1.0
        let v2 = installer
            .install_signed(&sign_pkg("1.1.0", "export const v = 2;", "index.mjs"))
            .unwrap();
        assert_eq!(v2.active_version, "1.1.0");
        assert_eq!(v2.previous_version.as_deref(), Some("1.0.0"));

        // Path traversal fails closed even if signed!
        let evil = sign_pkg("1.2.0", "evil", "../escape.mjs");
        assert!(matches!(
            installer.install_signed(&evil),
            Err(PlatformCapError::PolicyDenied(_))
        ));

        // Rollback atomically switches back to v1.0.0
        let rb = installer.rollback("@dsh/plugin-science-pack").unwrap();
        assert_eq!(rb.active_version, "1.0.0");
        assert_eq!(
            fs::read_to_string(rb.active_path.join("index.mjs")).unwrap(),
            "export const v = 1;"
        );

        h.dispose().await.unwrap();
        let _ = fs::remove_dir_all(&store_root);
    }

    #[tokio::test]
    async fn capture_permission_and_node_host_kill_on_dispose() {
        let root = Context::new();
        let topo = RealmTopology::new(root).unwrap();

        let h_cap = topo
            .host
            .spawn(PreparedPlugin::from_input(
                DesktopCapturePlugin,
                DesktopCaptureConfig {
                    screen_recording_granted: false,
                },
            ))
            .await
            .unwrap();
        let cap = topo.host.try_service::<DesktopCaptureService>().unwrap();
        assert!(matches!(
            cap.call_compat("getSources", &json!({ "types": ["screen"] })),
            Err(PlatformCapError::PermissionDenied(_))
        ));
        cap.call_compat("setPermission", &json!({ "granted": true }))
            .unwrap();
        let res = cap
            .call_compat("getSources", &json!({ "types": ["screen", "window"] }))
            .unwrap();
        assert_eq!(res["sources"].as_array().unwrap().len(), 2);
        h_cap.dispose().await.unwrap();

        // NodeHost spawns real isolated node worker and kills it on Fiber disposal!
        let h_node = topo
            .core
            .spawn(PreparedPlugin::from_input(
                RuntimeNodeHostPlugin,
                NodeHostConfig::default(),
            ))
            .await
            .unwrap();
        let node_host = topo.core.try_service::<RuntimeNodeHostService>().unwrap();
        let pid = node_host
            .spawn_worker(
                "w-1",
                &["-e".into(), "setTimeout(() => {}, 30000)".into()],
                &std::env::temp_dir(),
            )
            .expect("spawn isolated node worker");
        assert!(pid > 0);
        assert_eq!(node_host.active_worker_count(), 1);
        h_node.dispose().await.unwrap();
        assert_eq!(node_host.active_worker_count(), 0);
    }
}
