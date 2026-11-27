use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

/// Holds an exclusive filesystem lock for the supervisor's private volume.
pub(super) struct PrivateVolumeWriterSeal {
    path: PathBuf,
    remove_on_drop: bool,
    #[cfg(unix)]
    guard: Option<File>,
}

impl PrivateVolumeWriterSeal {
    pub(super) fn acquire(volume_dir: &Path, epoch: u64) -> Result<Self, String> {
        fs::create_dir_all(volume_dir).map_err(|error| format!("create volume dir: {error}"))?;
        let path = volume_dir.join(".writer-seal.lock");
        #[cfg(unix)]
        if let Some(seal) = Self::acquire_unix(&path, epoch)? {
            return Ok(seal);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| Self::unavailable(&path, error))?;
        let seal = Self {
            path,
            remove_on_drop: true,
            #[cfg(unix)]
            guard: None,
        };
        writeln!(file, "pid={} epoch={epoch}", std::process::id())
            .map_err(|error| format!("write volume seal: {error}"))?;
        Ok(seal)
    }

    #[cfg(unix)]
    fn acquire_unix(path: &Path, epoch: u64) -> Result<Option<Self>, String> {
        let guard_path = path.with_file_name(".writer-seal.guard");
        let mut guard = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&guard_path)
            .map_err(|error| Self::unavailable(path, error))?;
        let identity = guard.metadata().map_err(|error| Self::unavailable(path, error))?;
        if !identity.is_file() {
            return Err(Self::unavailable(path, "the guard is not a regular file"));
        }
        // SAFETY: guard owns an open descriptor. Both writer processes keep it open.
        if unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Unsupported {
                return Ok(None);
            }
            return Err(Self::unavailable(path, error));
        }

        // The ephemeral name also excludes supervisors that use create_new.
        // An existing link to the locked inode is ours to recover after a crash.
        if let Err(error) = fs::hard_link(&guard_path, path) {
            if error.kind() == io::ErrorKind::Unsupported
                || error.raw_os_error() == Some(libc::EPERM)
            {
                return Ok(None);
            }
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(Self::unavailable(path, error));
            }
        }
        let marker = fs::symlink_metadata(path).map_err(|error| Self::unavailable(path, error))?;
        if !marker.is_file() || marker.dev() != identity.dev() || marker.ino() != identity.ino() {
            return Err(Self::unavailable(path, "the existing marker belongs to another writer"));
        }
        let write_marker = guard
            .set_len(0)
            .and_then(|()| writeln!(guard, "pid={} epoch={epoch}", std::process::id()));
        if let Err(error) = write_marker {
            let _ = fs::remove_file(path);
            return Err(format!("write volume seal: {error}"));
        }

        Ok(Some(Self { path: path.to_owned(), remove_on_drop: true, guard: Some(guard) }))
    }

    pub(super) fn configure_daemon(&self, command: &mut tokio::process::Command) {
        #[cfg(unix)]
        if let Some(guard) = &self.guard {
            let fd = guard.as_raw_fd();
            command.arg("--writer-seal-fd").arg(fd.to_string());
            // SAFETY: the seal owns fd until spawn returns. The callback only uses fcntl.
            // The daemon reapplies close-on-exec at startup.
            unsafe {
                command.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(not(unix))]
        let _ = command;
    }

    pub(super) fn preserve_on_failure(&mut self) {
        #[cfg(unix)]
        if self.guard.is_some() {
            self.remove_on_drop = false;
        }
    }

    pub(super) fn release(mut self) {
        self.remove_on_drop = true;
    }

    fn unavailable(path: &Path, error: impl std::fmt::Display) -> String {
        format!("PRIVATE_VOLUME_DUAL_WRITE_DENIED: seal `{}` unavailable ({error})", path.display())
    }

    pub(super) fn lock_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateVolumeWriterSeal {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn recovers_its_own_marker_after_the_previous_lock_releases() {
        let volume =
            std::env::temp_dir().join(format!("cordis-seal-recovery-{}", std::process::id()));
        fs::create_dir(&volume).expect("create test volume");
        let mut first = PrivateVolumeWriterSeal::acquire(&volume, 1).expect("first seal");
        let marker = first.lock_path().to_owned();
        assert!(PrivateVolumeWriterSeal::acquire(&volume, 2).is_err());
        if first.guard.is_some() {
            first.preserve_on_failure();
            drop(first);
            assert!(marker.is_file());
            let second = PrivateVolumeWriterSeal::acquire(&volume, 2).expect("recover seal");
            assert!(fs::read_to_string(&marker).unwrap().contains("epoch=2"));
            second.release();
        } else {
            drop(first);
        }
        assert!(!marker.exists());

        fs::write(&marker, "legacy marker").expect("create legacy marker");
        assert!(PrivateVolumeWriterSeal::acquire(&volume, 3).is_err());
        assert_eq!(fs::read_to_string(&marker).unwrap(), "legacy marker");
        fs::remove_dir_all(volume).expect("remove test volume");
    }
}
