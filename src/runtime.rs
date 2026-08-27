//! Owner-only runtime state shared by Termnav subsystems.
//!
//! Relays, navigation directives, and focus leases all create files that can
//! affect terminal input or appearance. This module is the single authority
//! for selecting and validating their runtime directories so one subsystem
//! cannot accidentally weaken another subsystem's filesystem boundary.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Return an owner-only directory below Termnav's per-user runtime root.
pub fn private_subdirectory(parts: &[&OsStr]) -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| Path::new(value).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let uid = unsafe { libc::getuid() };
    let mut path = base.join(format!("termnav-{uid}"));
    ensure_private_directory(&path, uid)?;

    for part in parts {
        // Callers provide fixed names or hashes, never path-shaped input. The
        // check keeps that interface honest if a future caller passes user
        // data directly and would otherwise escape the validated root.
        if Path::new(part).components().count() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "runtime directory component must be one path segment",
            ));
        }
        path.push(part);
        ensure_private_directory(&path, uid)?;
    }
    Ok(path)
}

fn ensure_private_directory(path: &Path, uid: u32) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing unsafe Termnav runtime directory: {}",
                path.display()
            ),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}
