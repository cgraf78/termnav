//! Owner-only runtime state shared by Termnav subsystems.
//!
//! Relays, navigation directives, and focus leases all create files that can
//! affect terminal input or appearance. This module is the single authority
//! for selecting and validating their runtime directories so one subsystem
//! cannot accidentally weaken another subsystem's filesystem boundary.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Return an owner-only directory below Termnav's per-user runtime root.
pub fn private_subdirectory(parts: &[&OsStr]) -> io::Result<PathBuf> {
    let base = runtime_base();
    let uid = unsafe { libc::getuid() };
    let mut path = private_root(&base, uid)?;

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
        ensure_private_directory_for(&path, uid)?;
    }
    Ok(path)
}

/// Return an owner-only root that can hold one portable Unix socket leaf.
///
/// macOS reserves only 104 bytes for `sockaddr_un.sun_path`, including the
/// trailing NUL. Its default per-session temporary directory can already be
/// long enough that a random relay name no longer fits. Prefer the normal XDG
/// root, but use a validated `/tmp/termnav-UID` root when length alone would
/// make the socket unusable.
pub fn private_socket_directory(leaf: &OsStr) -> io::Result<PathBuf> {
    if Path::new(leaf).components().count() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runtime socket name must be one path segment",
        ));
    }
    let uid = unsafe { libc::getuid() };
    let mut bases = vec![runtime_base(), std::env::temp_dir(), PathBuf::from("/tmp")];
    bases.dedup();
    for base in bases {
        let root = base.join(format!("termnav-{uid}"));
        // 103 non-NUL bytes fit both Linux and the more restrictive macOS
        // sockaddr_un layout. Count encoded path bytes, not Unicode scalars.
        if root.join(leaf).as_os_str().as_bytes().len() > 103 {
            continue;
        }
        return private_root(&base, uid);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "no portable Termnav Unix socket path is available",
    ))
}

fn runtime_base() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| Path::new(value).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

fn private_root(base: &Path, uid: u32) -> io::Result<PathBuf> {
    let path = base.join(format!("termnav-{uid}"));
    ensure_private_directory_for(&path, uid)?;
    Ok(path)
}

/// Create or validate an owner-only directory at an exact caller-owned path.
///
/// Most state should use [`private_subdirectory`]. Relay listeners also accept
/// an explicit socket path for protocol tests and internal composition, so
/// their immediate parent still needs the same symlink and ownership checks.
pub fn ensure_private_directory(path: &Path) -> io::Result<()> {
    ensure_private_directory_for(path, unsafe { libc::getuid() })
}

fn ensure_private_directory_for(path: &Path, uid: u32) -> io::Result<()> {
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
