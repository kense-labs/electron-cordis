use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};

use flate2::read::MultiGzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use super::{
    sanitize_plugin_dir_name, store_error, validate_relative_file_path, InstalledPluginState,
    PluginInstallerService, PluginStore,
};
use crate::{encode_hex, verify_ed25519_hex, PlatformCapError};

const BUFFER_BYTES: usize = 64 * 1024;

/// Ed25519-signed descriptor for a native plugin delivered as a gzip-compressed tar archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedPluginArchive {
    pub plugin_id: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub archive_bytes: u64,
    pub archive_sha256: String,
    pub unpacked_file_bytes: u64,
    pub entries: u64,
    pub ed25519_signature_hex: String,
}

impl SignedPluginArchive {
    /// Serialize this tuple as compact UTF-8 JSON to sign the archive in any language.
    pub fn canonical_signing_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&(
            "plugin-archive-v1",
            &self.plugin_id,
            &self.version,
            &self.os,
            &self.arch,
            self.archive_bytes,
            &self.archive_sha256,
            self.unpacked_file_bytes,
            self.entries,
        ))
        .expect("archive descriptor contains JSON primitives")
    }

    fn verify(&self, trusted_key: Option<&str>) -> Result<(String, String), PlatformCapError> {
        let key = trusted_key.ok_or_else(|| {
            PlatformCapError::SignatureInvalid(
                "no trusted Ed25519 public key configured for plugin installer".into(),
            )
        })?;
        verify_ed25519_hex(key, &self.canonical_signing_bytes(), &self.ed25519_signature_hex)?;
        if (self.os.as_str(), self.arch.as_str()) != (std::env::consts::OS, std::env::consts::ARCH)
        {
            return Err(PlatformCapError::CompatibilityMismatch(
                "plugin archive targets another OS or architecture".into(),
            ));
        }
        if self.entries == 0 || self.archive_bytes == 0 {
            return Err(PlatformCapError::InvalidArgs(
                "plugin archive must contain at least one entry".into(),
            ));
        }
        if self.archive_sha256.len() != 64
            || !self
                .archive_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(PlatformCapError::InvalidArgs(
                "expected a lowercase archive SHA-256".into(),
            ));
        }
        Ok((sanitize_plugin_dir_name(&self.plugin_id)?, sanitize_plugin_dir_name(&self.version)?))
    }
}

fn archive_mismatch(message: impl Into<String>) -> PlatformCapError {
    PlatformCapError::IntegrityMismatch(message.into())
}

fn verify_bytes(
    mut source: impl Read,
    mut output: impl Write,
    manifest: &SignedPluginArchive,
) -> Result<(), PlatformCapError> {
    let mut hash = Sha256::new();
    let mut buffer = [0; BUFFER_BYTES];
    let mut remaining = manifest.archive_bytes;
    loop {
        let read = source.read(&mut buffer).map_err(store_error)?;
        if read == 0 {
            break;
        }
        remaining = remaining
            .checked_sub(read as u64)
            .ok_or_else(|| archive_mismatch("archive grew while being read"))?;
        hash.update(&buffer[..read]);
        output.write_all(&buffer[..read]).map_err(store_error)?;
    }
    if remaining != 0 || encode_hex(&hash.finalize()) != manifest.archive_sha256 {
        return Err(archive_mismatch("archive hash differs from its signed descriptor"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_archive(source: &File, metadata: &fs::Metadata, destination: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::macos::fs::MetadataExt;
    use std::os::unix::ffi::OsStrExt;

    // Cloning preserves flags and mode. A fresh copy avoids inheriting flags that
    // can prevent deletion or permissions that can prevent reopening the clone.
    if metadata.st_flags() != 0 || metadata.st_mode() & u32::from(libc::S_IRUSR) == 0 {
        return Ok(false);
    }
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: The source FD stays open and destination is a NUL-terminated path
    // inside the private staging directory. On failure no clone is created.
    let result = unsafe {
        libc::fclonefileat(source.as_raw_fd(), libc::AT_FDCWD, destination.as_ptr(), 0)
    };
    Ok(result == 0)
}

fn verified_archive(
    source: &Path,
    destination: &Path,
    manifest: &SignedPluginArchive,
) -> Result<File, PlatformCapError> {
    if !source.is_absolute()
        || !fs::symlink_metadata(source)
            .map_err(|error| store_error(format_args!("inspect source archive: {error}")))?
            .is_file()
    {
        return Err(PlatformCapError::InvalidArgs(
            "expected an absolute path to a regular archive file".into(),
        ));
    }
    let source = File::open(source)
        .map_err(|error| store_error(format_args!("open source archive: {error}")))?;
    let metadata = source
        .metadata()
        .map_err(|error| store_error(format_args!("stat open source archive: {error}")))?;
    if !metadata.is_file() {
        return Err(PlatformCapError::InvalidArgs(
            "expected an open regular archive file".into(),
        ));
    }
    if metadata.len() != manifest.archive_bytes {
        return Err(archive_mismatch("archive size differs from its signed descriptor"));
    }

    #[cfg(target_os = "macos")]
    if clone_archive(&source, &metadata, destination).map_err(store_error)? {
        let mut snapshot = File::open(destination).map_err(store_error)?;
        verify_bytes(&mut snapshot, io::sink(), manifest)?;
        snapshot.rewind().map_err(store_error)?;
        return Ok(snapshot);
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut snapshot = options
        .open(destination)
        .map_err(|error| store_error(format_args!("create archive snapshot: {error}")))?;
    verify_bytes(source, &mut snapshot, manifest)?;
    snapshot.rewind().map_err(store_error)?;
    Ok(snapshot)
}

fn relative_entry(path: &Path) -> Result<PathBuf, PlatformCapError> {
    let name = path.to_str().ok_or_else(|| {
        PlatformCapError::PolicyDenied("archive entry is not a UTF-8 path".into())
    })?;
    if name.contains(['\\', ':']) {
        return Err(PlatformCapError::PolicyDenied(format!(
            "invalid portable archive path `{name}`"
        )));
    }
    let path = validate_relative_file_path(name)?;
    let normalized: PathBuf = path.components().filter(|part| *part != Component::CurDir).collect();
    if normalized.as_os_str().is_empty() {
        return Err(PlatformCapError::PolicyDenied(
            "archive entries must be beneath the package root".into(),
        ));
    }
    Ok(normalized)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o755))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

fn unpack_archive(
    snapshot: &mut File,
    destination: &Path,
    manifest: &SignedPluginArchive,
) -> Result<(), PlatformCapError> {
    let mut archive = tar::Archive::new(MultiGzDecoder::new(snapshot));
    let mut entries = BTreeSet::new();
    let mut directories = Vec::new();
    let mut remaining_bytes = manifest.unpacked_file_bytes;
    for entry in archive.entries().map_err(store_error)? {
        let mut entry = entry.map_err(store_error)?;
        let relative = relative_entry(&entry.path().map_err(store_error)?)?;
        if !entries.insert(relative.clone()) || entries.len() as u64 > manifest.entries {
            return Err(archive_mismatch("duplicate entry or too many archive entries"));
        }
        let path = destination.join(relative);
        let mode = entry.header().mode().map_err(store_error)?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(&path).map_err(store_error)?;
            directories.push((path, mode));
        } else if kind.is_file() {
            remaining_bytes = remaining_bytes
                .checked_sub(entry.size())
                .ok_or_else(|| archive_mismatch("archive exceeds its signed unpacked size"))?;
            fs::create_dir_all(path.parent().expect("archive file has a parent"))
                .map_err(store_error)?;
            let mut output =
                OpenOptions::new().write(true).create_new(true).open(&path).map_err(store_error)?;
            if io::copy(&mut entry, &mut output).map_err(store_error)? != entry.size() {
                return Err(archive_mismatch("archive file ended early"));
            }
            set_mode(&path, mode).map_err(store_error)?;
        } else {
            return Err(PlatformCapError::PolicyDenied(
                "native plugin archives contain only directories and regular files".into(),
            ));
        }
    }
    if remaining_bytes != 0 || entries.len() as u64 != manifest.entries {
        return Err(archive_mismatch(
            "unpacked size or entry count differs from the signed descriptor",
        ));
    }
    // Drain the tar end padding and check the gzip footer before making the files active.
    io::copy(&mut archive.into_inner(), &mut io::sink()).map_err(store_error)?;
    directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, mode) in directories {
        set_mode(&path, mode).map_err(store_error)?;
    }
    Ok(())
}

fn same_permissions(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (_left.permissions().mode() & 0o777) == (_right.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    true
}

fn same_files(left: &Path, right: &Path) -> io::Result<bool> {
    let mut folders = vec![(left.to_owned(), right.to_owned())];
    let (mut a, mut b) = ([0; BUFFER_BYTES], [0; BUFFER_BYTES]);
    while let Some((staged, installed)) = folders.pop() {
        let mut count = 0;
        for entry in fs::read_dir(staged)? {
            let entry = entry?;
            count += 1;
            let target = installed.join(entry.file_name());
            let old = match fs::symlink_metadata(&target) {
                Ok(meta) => meta,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            let new = entry.metadata()?;
            if new.file_type() != old.file_type() || !same_permissions(&new, &old) {
                return Ok(false);
            }
            if new.is_dir() {
                folders.push((entry.path(), target));
            } else {
                if !new.is_file() || new.len() != old.len() {
                    return Ok(false);
                }
                let mut left = File::open(entry.path())?;
                let mut right = File::open(target)?;
                let mut remaining = new.len();
                while remaining > 0 {
                    let bytes = remaining.min(BUFFER_BYTES as u64) as usize;
                    left.read_exact(&mut a[..bytes])?;
                    right.read_exact(&mut b[..bytes])?;
                    if a[..bytes] != b[..bytes] {
                        return Ok(false);
                    }
                    remaining -= bytes as u64;
                }
            }
        }
        if fs::read_dir(installed)?.try_fold(0, |total, item| item.map(|_| total + 1))? != count {
            return Ok(false);
        }
    }
    Ok(true)
}

impl PluginInstallerService {
    /// Check the signed metadata before the host requests the potentially large archive.
    pub fn verify_archive_manifest(
        &self,
        manifest: &SignedPluginArchive,
    ) -> Result<(), PlatformCapError> {
        let key =
            self.config.lock().expect("installer cfg lock").trusted_ed25519_pubkey_hex.clone();
        manifest.verify(key.as_deref()).map(|_| ())
    }

    /// Install a signed archive downloaded by the host without putting its bytes on the RPC channel.
    pub fn install_signed_archive(
        &self,
        manifest: &SignedPluginArchive,
        source: &Path,
    ) -> Result<InstalledPluginState, PlatformCapError> {
        let cfg = self.config.lock().expect("installer cfg lock").clone();
        let (id, version) = manifest.verify(cfg.trusted_ed25519_pubkey_hex.as_deref())?;
        fs::create_dir_all(&cfg.store_root)
            .map_err(|error| store_error(format_args!("create archive store: {error}")))?;
        let stage = TempDir::new_in(&cfg.store_root).map_err(|error| {
            store_error(format_args!("create archive staging directory: {error}"))
        })?;
        let snapshot_path = stage.path().join("archive.tar.gz");
        let mut snapshot = verified_archive(source, &snapshot_path, manifest)?;
        let package = stage.path().join("package");
        fs::create_dir(&package).map_err(store_error)?;
        unpack_archive(&mut snapshot, &package, manifest)?;
        drop(snapshot);
        // Release the copied blocks before waiting for the store lock. TempDir
        // also cleans the snapshot if it could not be removed here.
        let _ = fs::remove_file(snapshot_path);

        let mut store = PluginStore::open(&cfg.store_root)?;
        let root = store.root.join(id);
        fs::create_dir_all(&root).map_err(store_error)?;
        let destination = root.join(version);
        match fs::symlink_metadata(&destination) {
            Ok(metadata) => {
                if !metadata.is_dir() || !same_files(&package, &destination).map_err(store_error)? {
                    return Err(archive_mismatch(
                        "installed plugin version already has different contents",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::rename(&package, &destination).map_err(|error| {
                    store_error(format_args!("publish archive version: {error}"))
                })?;
            }
            Err(error) => return Err(store_error(error)),
        }
        store.activate(&manifest.plugin_id, &manifest.version)
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use flate2::{write::GzEncoder, Compression};

    use super::*;
    use crate::{sha256_hex, PluginInstallerConfig};

    fn archive_fixture(path: &Path, data: &[u8], key: &SigningKey) -> SignedPluginArchive {
        let encoder = GzEncoder::new(File::create(path).unwrap(), Compression::fast());
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o755);
        header.set_size(data.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, "bin/runtime", data).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let bytes = fs::read(path).unwrap();
        let mut manifest = SignedPluginArchive {
            plugin_id: "@dsh/archive-test".into(),
            version: "1.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            archive_bytes: bytes.len() as u64,
            archive_sha256: sha256_hex(&bytes),
            unpacked_file_bytes: data.len() as u64,
            entries: 1,
            ed25519_signature_hex: String::new(),
        };
        manifest.ed25519_signature_hex =
            encode_hex(&key.sign(&manifest.canonical_signing_bytes()).to_bytes());
        manifest
    }

    #[test]
    fn archive_install_in_unicode_paths_preserves_binary_and_refuses_replacement() {
        let scratch = TempDir::new().unwrap();
        let key = SigningKey::from_bytes(&[19; 32]);
        let config = PluginInstallerConfig {
            store_root: scratch.path().join("插件 store"),
            trusted_ed25519_pubkey_hex: Some(encode_hex(key.verifying_key().as_bytes())),
        };
        let config_json = serde_json::to_string(&config).unwrap();
        let service = PluginInstallerService::new(serde_json::from_str(&config_json).unwrap());
        let archive = scratch.path().join("更新 archive.tar.gz");
        let payload = b"\x00\xf0\xf1\xff two-byte length";
        let manifest = archive_fixture(&archive, payload, &key);
        let installed = service.install_signed_archive(&manifest, &archive).unwrap();
        let executable = installed.active_path.join("bin/runtime");
        assert_eq!(fs::read(&executable).unwrap(), payload);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(executable.metadata().unwrap().permissions().mode() & 0o777, 0o755);
        }
        let retry = service.install_signed_archive(&manifest, &archive).unwrap();
        assert_eq!(retry.active_path, installed.active_path);
        assert_eq!(retry.previous_version, None);

        let conflict = archive_fixture(&archive, b"\x00another native binary", &key);
        assert!(matches!(
            service.install_signed_archive(&conflict, &archive),
            Err(PlatformCapError::IntegrityMismatch(_))
        ));
        assert_eq!(fs::read(&executable).unwrap(), payload);
    }

    #[test]
    fn verified_snapshot_is_independent_of_the_source_file() {
        let scratch = TempDir::new().unwrap();
        let key = SigningKey::from_bytes(&[23; 32]);
        let source = scratch.path().join("source.tar.gz");
        let destination = scratch.path().join("snapshot.tar.gz");
        let payload = b"verified executable data";
        let manifest = archive_fixture(&source, payload, &key);
        let mut snapshot = verified_archive(&source, &destination, &manifest).unwrap();
        fs::write(&source, vec![0; manifest.archive_bytes as usize]).unwrap();
        assert!(matches!(
            verified_archive(&source, &scratch.path().join("corrupt.tar.gz"), &manifest),
            Err(PlatformCapError::IntegrityMismatch(_))
        ));

        let package = scratch.path().join("payload");
        fs::create_dir(&package).unwrap();
        unpack_archive(&mut snapshot, &package, &manifest).unwrap();
        assert_eq!(fs::read(package.join("bin/runtime")).unwrap(), payload);
    }

    #[test]
    fn archive_install_limits_staging_to_the_signed_size() {
        let scratch = TempDir::new().unwrap();
        let key = SigningKey::from_bytes(&[29; 32]);
        let root = scratch.path().join("store");
        let service = PluginInstallerService::new(PluginInstallerConfig {
            store_root: root.clone(),
            trusted_ed25519_pubkey_hex: Some(encode_hex(key.verifying_key().as_bytes())),
        });
        let archive = scratch.path().join("native.tar.gz");
        let mut manifest = archive_fixture(&archive, b"\xf1\xff", &key);
        manifest.unpacked_file_bytes = 1;
        manifest.ed25519_signature_hex =
            encode_hex(&key.sign(&manifest.canonical_signing_bytes()).to_bytes());
        assert!(matches!(
            service.install_signed_archive(&manifest, &archive),
            Err(PlatformCapError::IntegrityMismatch(_))
        ));
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    }
}
