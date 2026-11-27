use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use cordis_core::{Context, Plugin, Service};
use electron_cordis_kernel::service_names;
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tempfile::{NamedTempFile, TempDir};

use crate::{sha256_hex, verify_ed25519_hex, PlatformCapError};

mod archive;
pub use archive::SignedPluginArchive;

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
        let mut sorted: Vec<(&str, &str)> =
            self.files.iter().map(|f| (f.relative_path.as_str(), f.sha256.as_str())).collect();
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
}

impl Service for PluginInstallerService {
    const NAME: &'static str = service_names::PLUGIN_INSTALLER;
}

fn sanitize_plugin_dir_name(id: &str) -> Result<String, PlatformCapError> {
    if id.trim().is_empty()
        || matches!(id, "." | ".installed-v1.json" | ".install.lock")
        || id.contains(['\0', '\\', ':'])
        || id.contains("..")
    {
        return Err(PlatformCapError::PolicyDenied(format!(
            "invalid plugin identifier or version `{id}`"
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
        return Err(PlatformCapError::PolicyDenied(format!("absolute file path forbidden: {rel}")));
    }
    for c in p.components() {
        if matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)) {
            return Err(PlatformCapError::PolicyDenied(format!("path traversal forbidden: {rel}")));
        }
    }
    Ok(p)
}

const INSTALLED_INDEX: &str = ".installed-v1.json";

#[derive(Serialize, Deserialize)]
struct PluginVersions {
    active: String,
    previous: Option<String>,
}

struct PluginStore<'a> {
    root: &'a Path,
    _lock: File,
    installed: BTreeMap<String, PluginVersions>,
}

fn store_error(error: impl std::fmt::Display) -> PlatformCapError {
    PlatformCapError::Runtime(format!("access plugin store: {error}"))
}

impl<'a> PluginStore<'a> {
    fn open(root: &'a Path) -> Result<Self, PlatformCapError> {
        fs::create_dir_all(root).map_err(store_error)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".install.lock"))
            .map_err(store_error)?;
        lock.lock_exclusive().map_err(store_error)?;
        let installed = match File::open(root.join(INSTALLED_INDEX)) {
            Ok(index) => serde_json::from_reader(index).map_err(store_error)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(store_error(error)),
        };
        Ok(Self { root, _lock: lock, installed })
    }

    fn save(&self) -> Result<(), PlatformCapError> {
        let mut staged = NamedTempFile::new_in(self.root).map_err(store_error)?;
        serde_json::to_writer(staged.as_file_mut(), &self.installed).map_err(store_error)?;
        staged.as_file().sync_all().map_err(store_error)?;
        staged.persist(self.root.join(INSTALLED_INDEX)).map_err(store_error)?;
        Ok(())
    }

    fn state(
        &self,
        id: &str,
        versions: &PluginVersions,
    ) -> Result<InstalledPluginState, PlatformCapError> {
        let active_path = self
            .root
            .join(sanitize_plugin_dir_name(id)?)
            .join(sanitize_plugin_dir_name(&versions.active)?);
        Ok(InstalledPluginState {
            plugin_id: id.to_owned(),
            active_version: versions.active.clone(),
            previous_version: versions.previous.clone(),
            active_path,
        })
    }

    fn activate(
        &mut self,
        id: &str,
        version: &str,
    ) -> Result<InstalledPluginState, PlatformCapError> {
        let current = self.installed.get(id);
        if let Some(current) = current {
            if current.active == version {
                return self.state(id, current);
            }
        }
        let versions = PluginVersions {
            active: version.to_owned(),
            previous: current.map(|current| current.active.clone()),
        };
        let state = self.state(id, &versions)?;
        self.installed.insert(id.to_owned(), versions);
        self.save()?;
        Ok(state)
    }
}

fn matches_installed_files(
    directory: &Path,
    expected: &BTreeMap<PathBuf, &[u8]>,
) -> io::Result<bool> {
    let mut folders = vec![directory.to_owned()];
    let mut matched_files = 0;
    let mut buffer = [0; 64 * 1024];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(folder)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                folders.push(entry.path());
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(directory).expect("path is inside plugin directory");
            let Some(contents) = expected.get(relative) else {
                return Ok(false);
            };
            if !kind.is_file() || entry.metadata()?.len() != contents.len() as u64 {
                return Ok(false);
            }
            let mut file = File::open(path)?;
            for chunk in contents.chunks(buffer.len()) {
                file.read_exact(&mut buffer[..chunk.len()])?;
                if chunk != &buffer[..chunk.len()] {
                    return Ok(false);
                }
            }
            matched_files += 1;
        }
    }
    Ok(matched_files == expected.len())
}

fn stage_version(
    directory: &Path,
    files: BTreeMap<PathBuf, &[u8]>,
) -> Result<(), PlatformCapError> {
    let plugin_root = directory.parent().expect("plugin versions have a parent");
    fs::create_dir_all(plugin_root).map_err(store_error)?;
    let stage = TempDir::new_in(plugin_root).map_err(store_error)?;
    for (relative, contents) in files {
        let file = stage.path().join(relative);
        fs::create_dir_all(file.parent().expect("staged files have a parent"))
            .map_err(store_error)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(file)
            .and_then(|mut file| file.write_all(contents))
            .map_err(store_error)?;
    }
    fs::rename(stage.path(), directory).map_err(store_error)?;
    Ok(())
}

impl PluginInstallerService {
    pub fn new(config: PluginInstallerConfig) -> Self {
        Self { config: Arc::new(Mutex::new(config)) }
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
        verify_ed25519_hex(pubkey, &pkg.canonical_signing_bytes(), &pkg.ed25519_signature_hex)?;

        if pkg.files.is_empty() {
            return Err(PlatformCapError::InvalidArgs(
                "plugin package must contain at least one file".into(),
            ));
        }

        // Validate the complete payload before updating the store.
        let safe_id = sanitize_plugin_dir_name(&pkg.plugin_id)?;
        let safe_ver = sanitize_plugin_dir_name(&pkg.version)?;
        let mut files = BTreeMap::new();
        for f in &pkg.files {
            let rel = validate_relative_file_path(&f.relative_path)?;
            let actual = sha256_hex(f.content_utf8.as_bytes());
            if actual != f.sha256.to_ascii_lowercase() {
                return Err(PlatformCapError::IntegrityMismatch(format!(
                    "file `{}` SHA-256 mismatch: expected {}, got {actual}",
                    f.relative_path, f.sha256
                )));
            }
            if files.insert(rel, f.content_utf8.as_bytes()).is_some() {
                return Err(PlatformCapError::InvalidArgs(format!(
                    "duplicate plugin file: {}",
                    f.relative_path
                )));
            }
        }

        let mut store = PluginStore::open(&cfg.store_root)?;
        let active_dir = store.root.join(safe_id).join(safe_ver);
        match fs::symlink_metadata(&active_dir) {
            Ok(metadata) => {
                if !metadata.is_dir()
                    || !matches_installed_files(&active_dir, &files).map_err(store_error)?
                {
                    return Err(PlatformCapError::IntegrityMismatch(format!(
                        "installed plugin `{}` version `{}` already has different contents",
                        pkg.plugin_id, pkg.version,
                    )));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                stage_version(&active_dir, files)?
            }
            Err(error) => return Err(store_error(error)),
        }
        store.activate(&pkg.plugin_id, &pkg.version)
    }

    pub fn rollback(&self, plugin_id: &str) -> Result<InstalledPluginState, PlatformCapError> {
        let root = self.config.lock().expect("installer cfg lock").store_root.clone();
        let mut store = PluginStore::open(&root)?;
        let current = store.installed.get(plugin_id).ok_or_else(|| {
            PlatformCapError::InvalidArgs(format!("plugin `{plugin_id}` is not installed"))
        })?;
        let previous = current.previous.clone().ok_or_else(|| {
            PlatformCapError::InvalidArgs(format!("plugin `{plugin_id}` has no previous version"))
        })?;
        let versions = PluginVersions { active: previous, previous: Some(current.active.clone()) };
        let state = store.state(plugin_id, &versions)?;
        if !fs::symlink_metadata(&state.active_path).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(PlatformCapError::Runtime(
                "previous plugin version directory missing on disk".into(),
            ));
        }
        store.installed.insert(plugin_id.to_owned(), versions);
        store.save()?;
        Ok(state)
    }

    pub fn list_installed(&self) -> Vec<InstalledPluginState> {
        self.try_list_installed().unwrap_or_default()
    }

    pub fn try_list_installed(&self) -> Result<Vec<InstalledPluginState>, PlatformCapError> {
        let root = self.config.lock().expect("installer cfg lock").store_root.clone();
        let store = PluginStore::open(&root)?;
        store.installed.iter().map(|(id, versions)| store.state(id, versions)).collect()
    }

    pub fn call_compat(&self, method: &str, args: &Value) -> Result<Value, PlatformCapError> {
        match method {
            "setTrustedPubkey" => {
                let hex = args.get("pubkeyHex").and_then(Value::as_str).ok_or_else(|| {
                    PlatformCapError::InvalidArgs("missing string `pubkeyHex`".into())
                })?;
                self.config.lock().expect("installer cfg lock").trusted_ed25519_pubkey_hex =
                    Some(hex.to_string());
                Ok(json!({ "configured": true }))
            }
            "install" => {
                let pkg = SignedPluginPackage::deserialize(args)
                    .map_err(|e| PlatformCapError::InvalidArgs(e.to_string()))?;
                let st = self.install_signed(&pkg)?;
                Ok(serde_json::to_value(st).unwrap())
            }
            "verifyArchiveManifest" => {
                let manifest = SignedPluginArchive::deserialize(args)
                    .map_err(|e| PlatformCapError::InvalidArgs(e.to_string()))?;
                self.verify_archive_manifest(&manifest)?;
                Ok(json!({ "verified": true }))
            }
            "installArchive" => {
                let manifest = SignedPluginArchive::deserialize(&args["manifest"])
                    .map_err(|e| PlatformCapError::InvalidArgs(e.to_string()))?;
                let path = args["archivePath"].as_str().ok_or_else(|| {
                    PlatformCapError::InvalidArgs("missing string `archivePath`".into())
                })?;
                Ok(serde_json::to_value(self.install_signed_archive(&manifest, Path::new(path))?)
                    .unwrap())
            }
            "rollback" => {
                let id = args.get("plugin_id").and_then(Value::as_str).ok_or_else(|| {
                    PlatformCapError::InvalidArgs("missing string `plugin_id`".into())
                })?;
                let st = self.rollback(id)?;
                Ok(serde_json::to_value(st).unwrap())
            }
            "listInstalled" => Ok(json!({ "plugins": self.try_list_installed()? })),
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
        let pub_handle = ctx.provide(svc).map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = pub_handle.remove();
        })
        .map_err(|e| PlatformCapError::Runtime(e.to_string()))?;
        Ok(())
    }
}
