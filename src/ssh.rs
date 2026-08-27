//! Connection-owned SSH relay supervision.
//!
//! Termnav never opens a second authenticated transport. `ssh -G` only resolves
//! configuration, the reverse Unix forward is injected into the one session
//! the user requested, and cleanup addresses an already-resolved local mux
//! socket with a control operation that is structurally unable to reach the
//! network.

use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::process;
use crate::relay::client::new_nonce;
use crate::relay::server;

const SOCKET_WAIT: Duration = Duration::from_secs(1);
const CHILD_POLL: Duration = Duration::from_millis(10);
const SIGNAL_GRACE: Duration = Duration::from_millis(75);

/// Run SSH, enriching only an interactive session Termnav can safely own.
pub fn run(arguments: &[OsString]) -> io::Result<i32> {
    let binary = real_ssh()?;
    let Some(destination) = destination_index(arguments) else {
        return run_plain(&binary, arguments);
    };
    if control_mode(&arguments[..destination]) || !interactive(arguments, destination) {
        return run_plain(&binary, arguments);
    }

    let settings = match effective_config(&binary, arguments) {
        Ok(settings) => settings,
        Err(_) => return run_plain(&binary, arguments),
    };
    if settings
        .get("sessiontype")
        .is_some_and(|value| !value.eq_ignore_ascii_case("default"))
        || settings
            .get("remotecommand")
            .is_some_and(|value| !value.eq_ignore_ascii_case("none"))
        || settings
            .get("exitonforwardfailure")
            .is_some_and(|value| value.eq_ignore_ascii_case("yes"))
        || !resolved_tty(&settings, arguments, destination)
    {
        return run_plain(&binary, arguments);
    }

    server::sweep()?;
    let token = format!("{}{}", new_nonce(), new_nonce());
    let runtime = runtime_directory()?;
    let local_socket = runtime.join(format!("relay-{token}.sock"));
    let remote_socket = format!("/tmp/termnav-relay-{token}.sock");
    let (owner_read, owner_write) = pipe()?;
    let server_socket = local_socket.clone();
    let server_thread = thread::spawn(move || {
        let result = server::serve(&server_socket, Some(owner_read.as_raw_fd()));
        drop(owner_read);
        result
    });
    if !wait_for_socket(&local_socket, &server_thread) {
        drop(owner_write);
        let _ = server_thread.join();
        return run_plain(&binary, arguments);
    }

    let enhanced = enhanced_arguments(arguments, destination, &local_socket, &remote_socket);
    let mut child = Command::new(&binary)
        .args(&enhanced)
        .env("TERMNAV_PARENT_RELAY", &remote_socket)
        .spawn()?;
    let status = wait_for_ssh(&mut child)?;

    // Closing the only writer is a kernel-owned lifetime signal. It wakes the
    // server even when no request is active and avoids a detached process or
    // polling timer. The server unlinks its own socket before the join returns.
    drop(owner_write);
    let _ = server_thread.join();

    if let Some(control_path) = settings.get("controlpath").filter(|value| *value != "none") {
        cancel_forward(
            &binary,
            control_path,
            arguments
                .get(destination)
                .expect("destination index is validated"),
            &remote_socket,
            &local_socket,
        );
    }
    Ok(process::status_code(status))
}

fn run_plain(binary: &Path, arguments: &[OsString]) -> io::Result<i32> {
    Command::new(binary)
        .args(arguments)
        .status()
        .map(process::status_code)
}

pub(crate) fn real_ssh() -> io::Result<PathBuf> {
    if let Some(binary) = env::var_os("TERMNAV_SSH_BINARY") {
        return Ok(PathBuf::from(binary));
    }
    let installed_shim = env::current_exe()
        .ok()
        .and_then(|binary| binary.parent()?.parent().map(Path::to_owned))
        .map(|prefix| prefix.join("share/termnav/shims"));
    let source_shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("share/termnav/shims");
    for directory in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        if same_path(&directory, &source_shim)
            || installed_shim
                .as_ref()
                .is_some_and(|shim| same_path(&directory, shim))
        {
            continue;
        }
        let candidate = directory.join("ssh");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "ssh is not installed",
    ))
}

fn same_path(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left)
        .ok()
        .zip(fs::canonicalize(right).ok())
        .is_some_and(|(left, right)| left == right)
}

pub(crate) fn effective_config(
    binary: &Path,
    arguments: &[OsString],
) -> io::Result<HashMap<String, String>> {
    let mut command = Command::new(binary);
    command.arg("-G").args(arguments);
    let output = process::output_timeout(&mut command, Duration::from_secs(2))?;
    if !output.status.success() {
        return Err(io::Error::other("ssh -G failed"));
    }
    let mut settings = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((key, value)) = line.split_once(' ') {
            settings.insert(key.to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    Ok(settings)
}

fn interactive(arguments: &[OsString], destination: usize) -> bool {
    let explicit_tty = arguments[..destination]
        .iter()
        .filter_map(|value| value.to_str())
        .any(|value| value.starts_with("-t"));
    let stdin_tty = env::var("TERMNAV_TEST_STDIN_TTY")
        .ok()
        .map_or_else(|| io::stdin().is_terminal(), |value| value == "1");
    stdin_tty || explicit_tty
}

fn resolved_tty(
    settings: &HashMap<String, String>,
    arguments: &[OsString],
    destination: usize,
) -> bool {
    let explicit_tty = arguments[..destination]
        .iter()
        .filter_map(|value| value.to_str())
        .any(|value| value.starts_with("-t"));
    let request = settings
        .get("requesttty")
        .map_or_else(|| "auto".to_owned(), |value| value.to_ascii_lowercase());
    if request == "no" {
        return false;
    }
    if explicit_tty || matches!(request.as_str(), "yes" | "force") {
        return true;
    }
    let has_remote_command = destination + 1 < arguments.len();
    request == "auto" && !has_remote_command && io::stdin().is_terminal()
        || request == "auto"
            && !has_remote_command
            && env::var("TERMNAV_TEST_STDIN_TTY").is_ok_and(|value| value == "1")
}

fn control_mode(arguments: &[OsString]) -> bool {
    arguments
        .iter()
        .filter_map(|value| value.to_str())
        .any(|value| {
            matches!(value, "-G" | "-N" | "-T" | "-W" | "-O" | "-f")
                || value.starts_with("-W")
                || value.starts_with("-O")
                || value.strip_prefix('-').is_some_and(|flags| {
                    !flags.is_empty()
                        && flags.bytes().all(no_value_flag)
                        && flags.bytes().any(|flag| b"GNTf".contains(&flag))
                })
        })
}

fn destination_index(arguments: &[OsString]) -> Option<usize> {
    let mut index = 0;
    while index < arguments.len() {
        let value = arguments[index].to_str()?;
        if value == "--" {
            return (index + 1 < arguments.len()).then_some(index + 1);
        }
        if !value.starts_with('-') || value == "-" {
            return Some(index);
        }
        let bytes = value.as_bytes();
        if bytes.len() == 2 && no_value_flag(bytes[1]) || bytes.len() > 2 && value_option(bytes[1])
        {
            index += 1;
        } else if bytes.len() == 2 && value_option(bytes[1]) {
            index += 2;
        } else {
            return None;
        }
    }
    None
}

fn no_value_flag(value: u8) -> bool {
    b"46ACGKMNTVX Yafgknqstvx".contains(&value) && value != b' '
}

fn value_option(value: u8) -> bool {
    b"BbcDEeFIiJLlmOopQRSWw".contains(&value)
}

fn enhanced_arguments(
    arguments: &[OsString],
    destination: usize,
    local_socket: &Path,
    remote_socket: &str,
) -> Vec<OsString> {
    let option_end = if destination > 0 && arguments[destination - 1] == OsStr::new("--") {
        destination - 1
    } else {
        destination
    };
    let mut enhanced = arguments[..option_end].to_vec();
    enhanced.extend([
        OsString::from("-o"),
        OsString::from("StreamLocalBindUnlink=yes"),
        OsString::from("-o"),
        OsString::from(format!(
            "RemoteForward={remote_socket}:{}",
            local_socket.display()
        )),
        OsString::from("-o"),
        OsString::from("SendEnv=TERMNAV_PARENT_RELAY"),
    ]);
    enhanced.extend_from_slice(&arguments[option_end..]);
    enhanced
}

fn cancel_forward(
    binary: &Path,
    control_path: &str,
    destination: &OsStr,
    remote_socket: &str,
    local_socket: &Path,
) {
    // `-O cancel` is a local mux control request, but defensive command-line
    // overrides make the safety property structural: if the exact socket has
    // vanished, OpenSSH runs `/bin/false` instead of consulting a configured
    // proxy, DNS, network socket, or authentication provider.
    let _ = Command::new(binary)
        .args([
            OsStr::new("-S"),
            OsStr::new(control_path),
            OsStr::new("-o"),
            OsStr::new("ControlMaster=no"),
            OsStr::new("-o"),
            OsStr::new("CanonicalizeHostname=no"),
            OsStr::new("-o"),
            OsStr::new("ProxyJump=none"),
            OsStr::new("-o"),
            OsStr::new("ProxyCommand=/bin/false"),
            OsStr::new("-o"),
            OsStr::new("ClearAllForwardings=yes"),
            OsStr::new("-o"),
            OsStr::new("BatchMode=yes"),
            OsStr::new("-O"),
            OsStr::new("cancel"),
            OsStr::new("-R"),
            OsStr::new(&format!("{remote_socket}:{}", local_socket.display())),
            destination,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn wait_for_socket(socket: &Path, server_thread: &thread::JoinHandle<io::Result<()>>) -> bool {
    let deadline = Instant::now() + SOCKET_WAIT;
    while Instant::now() < deadline {
        if socket.exists() {
            return true;
        }
        if server_thread.is_finished() {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

fn wait_for_ssh(child: &mut Child) -> io::Result<ExitStatus> {
    let mut forwarded = false;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if !forwarded && let Some(signal) = server::shutdown_signal() {
            // A terminal-generated signal normally reaches both processes in
            // the foreground group. Give SSH a brief chance to report that
            // exit before forwarding the same signal for direct `kill` cases.
            let deadline = Instant::now() + SIGNAL_GRACE;
            while Instant::now() < deadline {
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
                thread::sleep(CHILD_POLL);
            }
            let _ = unsafe { libc::kill(child.id() as i32, signal) };
            forwarded = true;
        }
        thread::sleep(CHILD_POLL);
    }
}

fn runtime_directory() -> io::Result<PathBuf> {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| Path::new(value).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    let path = base.join(format!("termnav-{}", unsafe { libc::getuid() }));
    fs::create_dir_all(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `pipe` returns two newly owned descriptors.
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{control_mode, destination_index};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn destination_parser_preserves_option_boundaries() {
        assert_eq!(destination_index(&args(&["-p", "22", "host"])), Some(2));
        assert_eq!(destination_index(&args(&["-p22", "host"])), Some(1));
        assert_eq!(destination_index(&args(&["--", "-host"])), Some(1));
        assert_eq!(destination_index(&args(&["-p"])), None);
    }

    #[test]
    fn sessionless_control_modes_are_never_enhanced() {
        assert!(control_mode(&args(&["-N"])));
        assert!(control_mode(&args(&["-O", "check"])));
        assert!(control_mode(&args(&["-TN"])));
        assert!(!control_mode(&args(&["-t", "-p", "22"])));
    }
}
