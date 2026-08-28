//! One tool-search policy shared by local and ControlMaster editor routing.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

const USER_DEFAULTS: &[&str] = &[".local/bin", ".local/share/mise/shims"];
const SYSTEM_DEFAULTS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];

pub(crate) fn fallbacks(home: &Path, configured: Option<&OsStr>) -> Vec<PathBuf> {
    configured.map_or_else(
        || {
            USER_DEFAULTS
                .iter()
                .map(|path| home.join(path))
                .chain(SYSTEM_DEFAULTS.iter().map(PathBuf::from))
                .collect()
        },
        |value| {
            env::split_paths(value)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        },
    )
}

pub(crate) fn remote_assignment() -> String {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("$HOME"));
    let configured = env::var_os("TERMNAV_REMOTE_TOOL_PATH").filter(|value| !value.is_empty());
    remote_assignment_for(&home, configured.as_deref())
}

fn remote_assignment_for(home: &Path, configured: Option<&OsStr>) -> String {
    let rendered = fallbacks(home, configured)
        .iter()
        .map(|path| remote_path(path, home))
        .collect::<Vec<_>>()
        .join(":");
    format!("PATH=\"$PATH\":{rendered}; export PATH")
}

fn remote_path(path: &Path, home: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(home)
        && !relative.as_os_str().is_empty()
    {
        // Rebase home-relative entries on the remote account. This transports
        // a configured policy in the existing SSH command without depending
        // on sshd AcceptEnv or assuming local and remote home paths match.
        return format!(
            "\"$HOME\"/{}",
            crate::shell::quote(&relative.to_string_lossy())
        );
    }
    crate::shell::quote(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::{fallbacks, remote_assignment_for, remote_path};
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    #[test]
    fn defaults_and_remote_rendering_share_one_policy() {
        let home = Path::new("/home/local");
        assert_eq!(
            fallbacks(home, None),
            vec![
                PathBuf::from("/home/local/.local/bin"),
                PathBuf::from("/home/local/.local/share/mise/shims"),
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
            ]
        );
        assert_eq!(
            remote_path(Path::new("/home/local/.local/bin"), home),
            "\"$HOME\"/'.local/bin'"
        );
        assert_eq!(
            fallbacks(home, Some(OsStr::new("/managed/bin:/managed/shims"))),
            vec![
                PathBuf::from("/managed/bin"),
                PathBuf::from("/managed/shims")
            ]
        );
        assert_eq!(
            remote_assignment_for(
                home,
                Some(OsStr::new(
                    "/home/local/.local/bin:/home/local/.local/share/mise/shims"
                )),
            ),
            "PATH=\"$PATH\":\"$HOME\"/'.local/bin':\"$HOME\"/'.local/share/mise/shims'; export PATH"
        );
    }
}
