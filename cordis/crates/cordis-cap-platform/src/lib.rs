#![cfg(any(
    feature = "desktop-capture",
    feature = "updater",
    feature = "plugin-installer",
    feature = "runtime-node-host"
))]

//! M3 Batch 3 Class-A Rust DSH platform & supply-chain plugins (Chromium-free):
//! - `@dsh/cap-desktop-capture`: permission-gated screen/window enumeration (`delivery: "on-demand"`)
//! - `@dsh/cap-updater`: dual-channel updater (`cordis-canary` Full App + real Ed25519 Renderer-only
//!   update verification with `base=electron-cordis` `MAIN_COMPATIBILITY_HASH` isolation)
//! - `@dsh/plugin-installer`: signed on-demand plugin installer (real Ed25519 manifest signature +
//!   per-file SHA-256 verification + atomic staging/activation/rollback on disk)
//! - `@dsh/runtime-node-host`: isolated Node worker host with fixed argv, env allowlist, and
//!   deterministic kill-on-dispose cleanup barrier

#[cfg(feature = "runtime-node-host")]
use std::collections::BTreeMap;
use std::convert::Infallible;
#[cfg(feature = "runtime-node-host")]
use std::path::Path;
#[cfg(feature = "runtime-node-host")]
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use cordis_core::{Context, Plugin, Service};
#[cfg(any(feature = "updater", feature = "plugin-installer"))]
use ed25519_dalek::{Signature, VerifyingKey};
#[cfg(feature = "updater")]
use electron_cordis_kernel::service_names;
use serde::{Deserialize, Serialize};
#[cfg(any(feature = "desktop-capture", feature = "updater"))]
use serde_json::{json, Value};
#[cfg(any(feature = "updater", feature = "plugin-installer"))]
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

#[cfg(any(feature = "updater", feature = "plugin-installer"))]
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

#[cfg(any(feature = "updater", feature = "plugin-installer"))]
fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

#[cfg(any(feature = "updater", feature = "plugin-installer"))]
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    encode_hex(&digest)
}

#[cfg(any(feature = "updater", feature = "plugin-installer"))]
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

#[cfg(feature = "desktop-capture")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureSource {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[cfg(feature = "desktop-capture")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesktopCaptureConfig {
    #[serde(default)]
    pub screen_recording_granted: bool,
}

#[cfg(feature = "desktop-capture")]
#[derive(Clone)]
pub struct DesktopCaptureService {
    granted: Arc<Mutex<bool>>,
}

#[cfg(feature = "desktop-capture")]
impl Service for DesktopCaptureService {
    const NAME: &'static str = CAPTURE_SERVICE_NAME;
}

#[cfg(feature = "desktop-capture")]
impl DesktopCaptureService {
    pub fn new(cfg: DesktopCaptureConfig) -> Self {
        Self { granted: Arc::new(Mutex::new(cfg.screen_recording_granted)) }
    }

    pub fn call_compat(&self, method: &str, args: &Value) -> Result<Value, PlatformCapError> {
        match method {
            "setPermission" => {
                let g = args.get("granted").and_then(Value::as_bool).ok_or_else(|| {
                    PlatformCapError::InvalidArgs("missing bool `granted`".into())
                })?;
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
                    .map(|arr| arr.iter().filter_map(Value::as_str).map(String::from).collect())
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

#[cfg(feature = "desktop-capture")]
#[derive(Clone, Default)]
pub struct DesktopCapturePlugin;

#[cfg(feature = "desktop-capture")]
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
        let pub_handle = ctx.provide(svc).map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
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

#[cfg(feature = "updater")]
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

#[cfg(feature = "updater")]
fn default_update_channel() -> String {
    "cordis-canary".to_string()
}

#[cfg(feature = "updater")]
fn default_compat_hash() -> String {
    "base=electron-cordis;abi=electron43;v1".to_string()
}

#[cfg(feature = "updater")]
impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            channel: default_update_channel(),
            main_compatibility_hash: default_compat_hash(),
            trusted_ed25519_pubkey_hex: None,
        }
    }
}

#[cfg(feature = "updater")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RendererUpdateManifest {
    pub version: String,
    pub channel: String,
    pub main_compatibility_hash: String,
    pub bundle_sha256: String,
    pub ed25519_signature_hex: String,
}

#[cfg(feature = "updater")]
impl RendererUpdateManifest {
    pub fn canonical_signing_bytes(&self) -> Vec<u8> {
        format!(
            "renderer-update-v1|{}|{}|{}|{}",
            self.version, self.channel, self.main_compatibility_hash, self.bundle_sha256
        )
        .into_bytes()
    }
}

#[cfg(feature = "updater")]
#[derive(Clone)]
pub struct UpdaterService {
    config: Arc<Mutex<UpdaterConfig>>,
}

#[cfg(feature = "updater")]
impl Service for UpdaterService {
    const NAME: &'static str = service_names::UPDATER;
}

#[cfg(feature = "updater")]
impl UpdaterService {
    pub fn new(config: UpdaterConfig) -> Self {
        Self { config: Arc::new(Mutex::new(config)) }
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
        if !manifest.main_compatibility_hash.contains("base=electron-cordis")
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
                let hex = args.get("pubkeyHex").and_then(Value::as_str).ok_or_else(|| {
                    PlatformCapError::InvalidArgs("missing string `pubkeyHex`".into())
                })?;
                let mut cfg = self.config.lock().expect("updater lock");
                cfg.trusted_ed25519_pubkey_hex = Some(hex.to_string());
                Ok(json!({ "configured": true }))
            }
            "verifyRendererUpdate" => {
                let manifest: RendererUpdateManifest =
                    serde_json::from_value(args.get("manifest").cloned().unwrap_or(Value::Null))
                        .map_err(|e| PlatformCapError::InvalidArgs(format!("manifest: {e}")))?;
                let payload = args.get("bundleText").and_then(Value::as_str).ok_or_else(|| {
                    PlatformCapError::InvalidArgs("missing string `bundleText`".into())
                })?;
                self.verify_renderer_update(&manifest, payload.as_bytes())
            }
            other => Err(PlatformCapError::UnknownMethod(other.to_string())),
        }
    }
}

#[cfg(feature = "updater")]
#[derive(Clone, Default)]
pub struct UpdaterPlugin;

#[cfg(feature = "updater")]
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
        let pub_handle = ctx.provide(svc).map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = pub_handle.remove();
        })
        .map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        Ok(())
    }
}

#[cfg(feature = "plugin-installer")]
mod plugin_installer;
#[cfg(feature = "plugin-installer")]
pub use plugin_installer::*;

// ---------------------------------------------------------------------------
// 4. @dsh/runtime-node-host (Isolated Node worker host with S3 kill-on-dispose)
// ---------------------------------------------------------------------------

#[cfg(feature = "runtime-node-host")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHostConfig {
    #[serde(default = "default_node_bin")]
    pub node_binary: String,
}

#[cfg(feature = "runtime-node-host")]
fn default_node_bin() -> String {
    "node".to_string()
}

#[cfg(feature = "runtime-node-host")]
impl Default for NodeHostConfig {
    fn default() -> Self {
        Self { node_binary: default_node_bin() }
    }
}

#[cfg(feature = "runtime-node-host")]
#[derive(Clone)]
pub struct RuntimeNodeHostService {
    node_binary: String,
    workers: Arc<Mutex<BTreeMap<String, Child>>>,
}

#[cfg(feature = "runtime-node-host")]
impl Service for RuntimeNodeHostService {
    const NAME: &'static str = NODE_HOST_SERVICE_NAME;
}

#[cfg(feature = "runtime-node-host")]
impl RuntimeNodeHostService {
    pub fn new(cfg: NodeHostConfig) -> Self {
        Self { node_binary: cfg.node_binary, workers: Arc::new(Mutex::new(BTreeMap::new())) }
    }

    /// Spawns a bounded isolated Node worker (fixed binary, explicit argv, cleared env + PATH only).
    pub fn spawn_worker(
        &self,
        worker_id: &str,
        script_args: &[String],
        cwd: &Path,
    ) -> Result<u32, PlatformCapError> {
        if worker_id.trim().is_empty() || script_args.is_empty() {
            return Err(PlatformCapError::InvalidArgs("worker_id and script_args required".into()));
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
        self.workers.lock().expect("workers lock").insert(worker_id.to_string(), child);
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

#[cfg(feature = "runtime-node-host")]
#[derive(Clone, Default)]
pub struct RuntimeNodeHostPlugin;

#[cfg(feature = "runtime-node-host")]
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
        let pub_handle = ctx.provide(svc).map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = pub_handle.remove();
            svc_for_kill.kill_all_workers();
        })
        .map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        Ok(())
    }
}

#[cfg(all(
    test,
    feature = "desktop-capture",
    feature = "updater",
    feature = "plugin-installer",
    feature = "runtime-node-host"
))]
mod tests {
    use std::fs;

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
                    trusted_ed25519_pubkey_hex: Some(pk_hex.clone()),
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

        let restarted = PluginInstallerService::new(PluginInstallerConfig {
            store_root: store_root.clone(),
            trusted_ed25519_pubkey_hex: Some(pk_hex),
        });
        assert_eq!(restarted.try_list_installed().unwrap()[0].active_version, "1.1.0");
        let same = sign_pkg("1.1.0", "export const v = 2;", "index.mjs");
        std::thread::scope(|scope| {
            let first = scope.spawn(|| installer.install_signed(&same).unwrap());
            let second = scope.spawn(|| restarted.install_signed(&same).unwrap());
            for retry in [first.join().unwrap(), second.join().unwrap()] {
                assert_eq!(retry.active_path, v2.active_path);
                assert_eq!(retry.previous_version.as_deref(), Some("1.0.0"));
            }
        });
        let conflict = sign_pkg("1.1.0", "export const v = 99;", "index.mjs");
        assert!(matches!(
            restarted.install_signed(&conflict),
            Err(PlatformCapError::IntegrityMismatch(_))
        ));
        assert_eq!(
            fs::read_to_string(v2.active_path.join("index.mjs")).unwrap(),
            "export const v = 2;"
        );

        let mut incomplete = sign_pkg("1.2.0", "first file", "assets");
        incomplete.files.extend(sign_pkg("1.2.0", "another file", "assets/index.mjs").files);
        incomplete.ed25519_signature_hex =
            encode_hex(&sk.sign(&incomplete.canonical_signing_bytes()).to_bytes());
        assert!(restarted.install_signed(&incomplete).is_err());
        let versions_on_disk = fs::read_dir(v2.active_path.parent().unwrap()).unwrap().count();
        assert_eq!(versions_on_disk, 2);
        assert_eq!(restarted.try_list_installed().unwrap()[0].active_version, "1.1.0");

        // Path traversal fails closed even if signed!
        let evil = sign_pkg("1.2.0", "evil", "../escape.mjs");
        assert!(matches!(installer.install_signed(&evil), Err(PlatformCapError::PolicyDenied(_))));

        // Rollback atomically switches back to v1.0.0
        let rb = restarted.rollback("@dsh/plugin-science-pack").unwrap();
        assert_eq!(rb.active_version, "1.0.0");
        assert_eq!(installer.try_list_installed().unwrap()[0].active_version, "1.0.0");
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
                DesktopCaptureConfig { screen_recording_granted: false },
            ))
            .await
            .unwrap();
        let cap = topo.host.try_service::<DesktopCaptureService>().unwrap();
        assert!(matches!(
            cap.call_compat("getSources", &json!({ "types": ["screen"] })),
            Err(PlatformCapError::PermissionDenied(_))
        ));
        cap.call_compat("setPermission", &json!({ "granted": true })).unwrap();
        let res = cap.call_compat("getSources", &json!({ "types": ["screen", "window"] })).unwrap();
        assert_eq!(res["sources"].as_array().unwrap().len(), 2);
        h_cap.dispose().await.unwrap();

        // NodeHost spawns real isolated node worker and kills it on Fiber disposal!
        let h_node = topo
            .core
            .spawn(PreparedPlugin::from_input(RuntimeNodeHostPlugin, NodeHostConfig::default()))
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
