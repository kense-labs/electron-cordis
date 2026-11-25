use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use super::OsShellError;

const PROTECTED_ROOTS: &[&str] = &[".git", ".codex", ".agents", ".discovery"];

pub(super) fn resolve(path: &Path, root: &Path) -> Result<PathBuf, OsShellError> {
    let io_error = |error| denied(format!("workspace path: {error}"));
    fs::create_dir_all(root).map_err(io_error)?;
    let canonical_root = root.canonicalize().map_err(io_error)?;
    let relative = if path.is_absolute() {
        path.strip_prefix(&canonical_root)
            .or_else(|_| path.strip_prefix(root))
            .map_err(|_| denied("path outside workspace"))?
    } else {
        path
    };
    let relative = relative.to_string_lossy();
    let mut resolved = canonical_root.clone();
    let mut components = Path::new(relative.trim().trim_start_matches("./"))
        .components()
        .peekable();
    while let Some(component) = components.next() {
        let name = match component {
            Component::CurDir => continue,
            Component::Normal(name) => name.to_string_lossy(),
            _ => return Err(denied("path outside workspace")),
        };
        if resolved == canonical_root
            && PROTECTED_ROOTS.iter().any(|entry| {
                if cfg!(windows) {
                    name.eq_ignore_ascii_case(entry)
                } else {
                    name == *entry
                }
            })
        {
            return Err(denied("protected workspace path"));
        }
        #[cfg(windows)]
        if invalid_windows_component(&name) {
            return Err(denied("invalid Windows path component"));
        }

        let next = resolved.join(name.as_ref());
        resolved = match fs::symlink_metadata(&next) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(denied("workspace symlink is forbidden"));
            }
            Ok(meta) => {
                let canonical = next.canonicalize().map_err(io_error)?;
                if !canonical.starts_with(&canonical_root) {
                    return Err(denied("path outside workspace"));
                }
                if components.peek().is_some() && !meta.is_dir() {
                    return Err(denied("workspace path ancestor is not a directory"));
                }
                canonical
            }
            Err(error) if error.kind() == ErrorKind::NotFound => next,
            Err(error) => return Err(io_error(error)),
        };
    }
    Ok(resolved)
}

fn denied(message: impl Into<String>) -> OsShellError {
    OsShellError::PolicyDenied(message.into())
}

#[cfg(windows)]
fn invalid_windows_component(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    name.contains(':')
        || name.ends_with(['.', ' '])
        || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}
