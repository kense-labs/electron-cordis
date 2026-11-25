use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Holds an exclusive filesystem lock for the supervisor's private volume.
pub(super) struct PrivateVolumeWriterSeal {
    path: PathBuf,
}

impl PrivateVolumeWriterSeal {
    pub(super) fn acquire(volume_dir: &Path, epoch: u64) -> Result<Self, String> {
        fs::create_dir_all(volume_dir).map_err(|error| format!("create volume dir: {error}"))?;
        let path = volume_dir.join(".writer-seal.lock");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                format!(
                    "PRIVATE_VOLUME_DUAL_WRITE_DENIED: seal `{}` unavailable ({error})",
                    path.display()
                )
            })?;
        let seal = Self { path };
        let metadata = writeln!(file, "pid={} epoch={epoch}", std::process::id());
        drop(file);
        metadata.map_err(|error| format!("write volume seal: {error}"))?;
        Ok(seal)
    }

    pub(super) fn lock_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateVolumeWriterSeal {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
