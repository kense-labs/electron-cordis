//! `@dsh/cap-safe-storage` — Class-A OS secret storage plugin (Chromium-free Rust DSH plugin).
//!
//! Security invariants:
//! - Published EXCLUSIVELY into `HostRealm` as `host.safe_storage` (`service_names::SAFE_STORAGE`).
//! - Structurally unreachable from `UiRealm`, `CoreRealm`, or `UntrustedPluginRealm` (zero fallback).
//! - Explicit `caller_realm` check rejects any non-`"host"` compat caller (FailClosed).
//! - Authenticated encryption envelope: `v1 || nonce(16) || tag(32) || ciphertext`, using
//!   random per-message nonce (`getrandom`), SHA-256 keystream, and HMAC-SHA256 authentication tag
//!   verified in constant time before returning plaintext.

use std::convert::Infallible;
use std::sync::Arc;

use cordis_core::{Context, Plugin, Service};
use electron_cordis_kernel::service_names;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const PLUGIN_ID: &str = "@dsh/cap-safe-storage";
const ENVELOPE_VERSION: u8 = 1;
const NONCE_LEN: usize = 16;
const TAG_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum SafeStorageError {
    #[error("SAFE_STORAGE_REALM_DENIED (fail-closed): caller realm `{0}` is not HostRealm")]
    RealmDenied(String),
    #[error("invalid safeStorage arguments: {0}")]
    InvalidArgs(String),
    #[error("unknown safeStorage method: {0}")]
    UnknownMethod(String),
    #[error("decryption failed (corrupted or tampered ciphertext)")]
    DecryptFailed,
    #[error("entropy generation failed: {0}")]
    Entropy(String),
    #[error("safeStorage lifecycle error: {0}")]
    Runtime(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SafeStorageBackendMode {
    /// Ephemeral/seeded process memory key (default for deterministic CI and headless tests).
    #[default]
    Memory,
    /// OS Keychain master key (`cn.internai.inkstone.cordis.safe-storage`, fixed argv `/usr/bin/security`, zero shell).
    OsKeychain,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SafeStorageConfig {
    #[serde(default)]
    pub mode: SafeStorageBackendMode,
    /// Isolated keychain service name (side-by-side canary isolation from native Inkstone).
    #[serde(default = "default_keychain_service")]
    pub keychain_service: String,
    /// Optional deterministic key seed for testing; when None, fresh 256-bit OS entropy is used.
    #[serde(default)]
    pub key_seed_hex: Option<String>,
}

fn default_keychain_service() -> String {
    "cn.internai.inkstone.cordis.safe-storage".to_string()
}

/// HostRealm-exclusive authenticated secret encryption service.
#[derive(Clone)]
pub struct SafeStorageService {
    enc_key: [u8; 32],
    mac_key: [u8; 32],
    backend_label: String,
}

impl Service for SafeStorageService {
    const NAME: &'static str = service_names::SAFE_STORAGE;
}

impl SafeStorageService {
    pub fn from_config(cfg: &SafeStorageConfig) -> Result<Self, SafeStorageError> {
        if cfg.mode == SafeStorageBackendMode::OsKeychain {
            return Self::from_os_keychain(&cfg.keychain_service);
        }
        let master = match &cfg.key_seed_hex {
            Some(seed) => {
                let mut h = Sha256::new();
                h.update(b"electron-cordis-safe-storage-seed:");
                h.update(seed.as_bytes());
                let digest: [u8; 32] = h.finalize().into();
                digest
            }
            None => {
                let mut bytes = [0u8; 32];
                getrandom::fill(&mut bytes)
                    .map_err(|e| SafeStorageError::Entropy(e.to_string()))?;
                bytes
            }
        };
        Ok(Self::from_master_key_with_label(master, "memory"))
    }

    #[cfg(target_os = "macos")]
    fn from_os_keychain(service: &str) -> Result<Self, SafeStorageError> {
        use std::process::Command;
        if service.is_empty() || service.contains('\0') {
            return Err(SafeStorageError::InvalidArgs("invalid keychain service".into()));
        }
        // Fixed argv `/usr/bin/security` (zero shell concatenation).
        let read = Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", service, "-a", "master-key", "-w"])
            .output()
            .map_err(|e| SafeStorageError::Runtime(format!("keychain lookup failed: {e}")))?;
        let raw_secret = if read.status.success() {
            String::from_utf8_lossy(&read.stdout).trim().to_string()
        } else {
            let mut fresh = [0u8; 32];
            getrandom::fill(&mut fresh).map_err(|e| SafeStorageError::Entropy(e.to_string()))?;
            let encoded = base64_encode(&fresh);
            let write = Command::new("/usr/bin/security")
                .args([
                    "add-generic-password",
                    "-U",
                    "-s",
                    service,
                    "-a",
                    "master-key",
                    "-w",
                    &encoded,
                ])
                .output()
                .map_err(|e| SafeStorageError::Runtime(format!("keychain store failed: {e}")))?;
            if !write.status.success() {
                return Err(SafeStorageError::Runtime(
                    "macOS Keychain refused to store master key (fail-closed)".into(),
                ));
            }
            encoded
        };
        let master: [u8; 32] = Sha256::new()
            .chain_update(b"cordis-keychain-master:")
            .chain_update(raw_secret.as_bytes())
            .finalize()
            .into();
        Ok(Self::from_master_key_with_label(master, "macos-keychain"))
    }

    #[cfg(not(target_os = "macos"))]
    fn from_os_keychain(_service: &str) -> Result<Self, SafeStorageError> {
        Err(SafeStorageError::Runtime(
            "OS keychain backend unavailable on this target (fail-closed)".into(),
        ))
    }

    pub fn from_master_key(master: [u8; 32]) -> Self {
        Self::from_master_key_with_label(master, "memory")
    }

    fn from_master_key_with_label(master: [u8; 32], label: &str) -> Self {
        let enc_key: [u8; 32] = Sha256::new()
            .chain_update(b"cordis-safe-storage-enc-v1:")
            .chain_update(master)
            .finalize()
            .into();
        let mac_key: [u8; 32] = Sha256::new()
            .chain_update(b"cordis-safe-storage-mac-v1:")
            .chain_update(master)
            .finalize()
            .into();
        Self {
            enc_key,
            mac_key,
            backend_label: label.to_string(),
        }
    }

    pub fn selected_backend(&self) -> &str {
        &self.backend_label
    }

    pub fn is_encryption_available(&self) -> bool {
        true
    }

    pub fn encrypt_string(&self, plaintext: &str) -> Result<String, SafeStorageError> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|e| SafeStorageError::Entropy(e.to_string()))?;

        let plain = plaintext.as_bytes();
        let cipher = xor_keystream(&self.enc_key, &nonce, plain);
        let tag = compute_tag(&self.mac_key, ENVELOPE_VERSION, &nonce, &cipher);

        let mut blob = Vec::with_capacity(1 + NONCE_LEN + TAG_LEN + cipher.len());
        blob.push(ENVELOPE_VERSION);
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&tag);
        blob.extend_from_slice(&cipher);
        Ok(base64_encode(&blob))
    }

    pub fn decrypt_string(&self, cipher_base64: &str) -> Result<String, SafeStorageError> {
        let blob = base64_decode(cipher_base64).ok_or(SafeStorageError::DecryptFailed)?;
        if blob.len() < 1 + NONCE_LEN + TAG_LEN || blob[0] != ENVELOPE_VERSION {
            return Err(SafeStorageError::DecryptFailed);
        }
        let nonce: [u8; NONCE_LEN] = blob[1..1 + NONCE_LEN]
            .try_into()
            .map_err(|_| SafeStorageError::DecryptFailed)?;
        let provided_tag = &blob[1 + NONCE_LEN..1 + NONCE_LEN + TAG_LEN];
        let cipher = &blob[1 + NONCE_LEN + TAG_LEN..];

        let expected_tag = compute_tag(&self.mac_key, ENVELOPE_VERSION, &nonce, cipher);
        if !ct_eq(provided_tag, &expected_tag) {
            return Err(SafeStorageError::DecryptFailed);
        }

        let plain = xor_keystream(&self.enc_key, &nonce, cipher);
        String::from_utf8(plain).map_err(|_| SafeStorageError::DecryptFailed)
    }

    /// JSON-RPC compat dispatch enforcing `caller_realm == "host"` (HostRealm exclusive).
    pub fn call_compat(
        &self,
        caller_realm: &str,
        method: &str,
        args: &Value,
    ) -> Result<Value, SafeStorageError> {
        if caller_realm != "host" {
            return Err(SafeStorageError::RealmDenied(caller_realm.to_string()));
        }
        match method {
            "isEncryptionAvailable" => Ok(json!(self.is_encryption_available())),
            "encryptString" => {
                let plain = args
                    .get("plainText")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        SafeStorageError::InvalidArgs("missing string `plainText`".into())
                    })?;
                let cipher = self.encrypt_string(plain)?;
                Ok(json!(cipher))
            }
            "decryptString" => {
                let cipher = args
                    .get("cipherBase64")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        SafeStorageError::InvalidArgs("missing string `cipherBase64`".into())
                    })?;
                let plain = self.decrypt_string(cipher)?;
                Ok(json!(plain))
            }
            other => Err(SafeStorageError::UnknownMethod(other.to_string())),
        }
    }
}

fn xor_keystream(enc_key: &[u8; 32], nonce: &[u8; NONCE_LEN], input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut counter: u64 = 0;
    for chunk in input.chunks(32) {
        let block: [u8; 32] = Sha256::new()
            .chain_update(enc_key)
            .chain_update(nonce)
            .chain_update(counter.to_le_bytes())
            .finalize()
            .into();
        for (i, b) in chunk.iter().enumerate() {
            out.push(b ^ block[i]);
        }
        counter = counter.wrapping_add(1);
    }
    out
}

fn hmac_sha256(key: &[u8; 32], msg_parts: &[&[u8]]) -> [u8; 32] {
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..32 {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    for part in msg_parts {
        inner.update(part);
    }
    let inner_hash: [u8; 32] = inner.finalize().into();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    outer.finalize().into()
}

fn compute_tag(mac_key: &[u8; 32], version: u8, nonce: &[u8; NONCE_LEN], cipher: &[u8]) -> [u8; 32] {
    hmac_sha256(mac_key, &[&[version], nonce, cipher])
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

const B64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_TABLE[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_TABLE[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_TABLE[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn b64_val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    for chunk in bytes.chunks(4) {
        let a = b64_val(chunk[0])?;
        let b = b64_val(chunk[1])?;
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            let c = b64_val(chunk[2])?;
            out.push((b << 4) | (c >> 2));
            if chunk[3] != b'=' {
                let d = b64_val(chunk[3])?;
                out.push((c << 6) | d);
            }
        } else if chunk[3] != b'=' {
            return None;
        }
    }
    Some(out)
}

/// Cordis v3 plugin wiring `SafeStorageService` exclusively into `HostRealm`.
#[derive(Clone, Default)]
pub struct SafeStoragePlugin;

impl Plugin for SafeStoragePlugin {
    type Config = SafeStorageConfig;
    type Input = SafeStorageConfig;
    type PrepareError = Infallible;
    type ApplyError = SafeStorageError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(SafeStorageService::from_config(input)?);
        let publication = ctx
            .provide(svc)
            .map_err(|e| SafeStorageError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = publication.remove();
        })
        .map_err(|e| SafeStorageError::Runtime(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use cordis_core::PreparedPlugin;
    use electron_cordis_kernel::RealmTopology;

    use super::*;

    #[tokio::test]
    async fn safe_storage_is_host_realm_exclusive_and_rejects_tampering() {
        let root = Context::new();
        let topo = RealmTopology::new(root).expect("topology");

        let handle = topo
            .host
            .spawn(PreparedPlugin::from_input(
                SafeStoragePlugin,
                SafeStorageConfig::default(),
            ))
            .await
            .expect("spawn safeStorage in host realm");

        // HostRealm resolves it; UI, Core, and UntrustedPluginRealm cannot.
        let svc = topo
            .host
            .try_service::<SafeStorageService>()
            .expect("host sees safeStorage");
        assert!(topo.ui.try_service::<SafeStorageService>().is_err());
        assert!(topo.core.try_service::<SafeStorageService>().is_err());
        assert!(topo.untrusted_plugin.try_service::<SafeStorageService>().is_err());

        // HostRealm compat round-trip succeeds with random nonces.
        let c1 = svc
            .call_compat("host", "encryptString", &json!({ "plainText": "inkstone-jwt-123" }))
            .expect("encrypt 1");
        let c2 = svc
            .call_compat("host", "encryptString", &json!({ "plainText": "inkstone-jwt-123" }))
            .expect("encrypt 2");
        assert_ne!(c1, c2, "random nonce must produce distinct ciphertexts");

        let plain = svc
            .call_compat("host", "decryptString", &json!({ "cipherBase64": c1 }))
            .expect("decrypt");
        assert_eq!(plain, json!("inkstone-jwt-123"));

        // Tampering fails closed.
        let mut tampered = c1.as_str().unwrap().to_string();
        let last = if tampered.ends_with('A') { 'B' } else { 'A' };
        tampered.pop();
        tampered.push(last);
        assert!(matches!(
            svc.call_compat("host", "decryptString", &json!({ "cipherBase64": tampered })),
            Err(SafeStorageError::DecryptFailed)
        ));

        // Non-HostRealm caller fails closed.
        assert!(matches!(
            svc.call_compat("untrusted-plugin", "isEncryptionAvailable", &json!({})),
            Err(SafeStorageError::RealmDenied(_))
        ));
        assert!(matches!(
            svc.call_compat("ui", "encryptString", &json!({ "plainText": "x" })),
            Err(SafeStorageError::RealmDenied(_))
        ));

        handle.dispose().await.expect("dispose");
        assert!(topo.host.try_service::<SafeStorageService>().is_err());
    }
}
