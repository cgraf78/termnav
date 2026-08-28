//! Discovery of provider-owned runtime assets.

use std::env;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Resolve one installed asset without coupling consumers to an installer.
pub fn resolve(relative: &Path) -> io::Result<PathBuf> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "asset path must be a non-empty relative path without parent traversal",
        ));
    }

    let root = match env::var_os("TERMNAV_ASSET_ROOT") {
        Some(root) => PathBuf::from(root),
        None => {
            let executable = env::current_exe()?;
            // Release layouts keep the binary at ROOT/bin/termnav. Deriving
            // from the running executable makes this contract independent of
            // Shdeps, XDG defaults, and the directory used to launch Termnav.
            executable
                .parent()
                .and_then(Path::parent)
                .map(Path::to_owned)
                .ok_or_else(|| io::Error::other("cannot derive Termnav asset root"))?
        }
    };
    let root = root.canonicalize()?;
    let asset = root.join(relative).canonicalize()?;
    if !asset.starts_with(&root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "asset resolves outside the Termnav provider root",
        ));
    }
    Ok(asset)
}
