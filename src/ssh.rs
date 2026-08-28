//! Connection-owned SSH relay supervision.
//!
//! Termnav never opens a second authenticated transport. `ssh -G` only resolves
//! configuration, the reverse Unix forward is injected into the one session
//! the user requested, and cleanup addresses an already-resolved local mux
//! socket with a control operation that is structurally unable to reach the
//! network.

use std::collections::HashMap;
use std::env;
use std::ffi::CStr;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use crate::process;
use crate::relay::client::new_nonce;
use crate::relay::server;

const SOCKET_WAIT: Duration = Duration::from_secs(1);
const CHILD_POLL: Duration = Duration::from_millis(10);
const SIGNAL_GRACE: Duration = Duration::from_millis(75);
const SIGNAL_KILL_GRACE: Duration = Duration::from_secs(1);
const SSH_SHIM_ACTIVE_ENV: &str = "TERMNAV_SSH_SHIM_ACTIVE";
const SSH_SHIM_DIR_ENV: &str = "TERMNAV_SSH_SHIM_DIR";
const SSH_SHIM_MARKER: &[u8] = b"# termnav-ssh-shim-v1";
const LEGACY_SSH_SHIM_MARKER: &[u8] = b"# Keep SSH interposition inherited by child processes";
const SSH_SHIM_ORIGINAL_PATH_ENV: &str = "TERMNAV_SSH_ORIGINAL_PATH";

/// Run SSH, enriching only an interactive session Termnav can safely own.
pub fn run(arguments: &[OsString]) -> io::Result<i32> {
    let binary = real_ssh()?;
    let Some(destination) = destination_index(arguments) else {
        return run_plain(&binary, arguments);
    };
    let stdin_tty = io::stdin().is_terminal();
    if control_mode(&arguments[..destination]) || !interactive(arguments, destination, stdin_tty) {
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
        || !resolved_tty(&settings, arguments, destination, stdin_tty)
    {
        return run_plain(&binary, arguments);
    }

    if server::sweep().is_err() {
        return run_plain(&binary, arguments);
    }
    let token = format!("{}{}", new_nonce(), new_nonce());
    let socket_name = OsString::from(format!("relay-{token}.sock"));
    let runtime = match crate::runtime::private_socket_directory(&socket_name) {
        Ok(runtime) => runtime,
        Err(_) => return run_plain(&binary, arguments),
    };
    let local_socket = runtime.join(socket_name);
    let remote_socket = format!("/tmp/termnav-relay-{token}.sock");
    let (owner_read, owner_write) = match process::pipe(false) {
        Ok(pipe) => pipe,
        Err(_) => return run_plain(&binary, arguments),
    };
    let direct_terminal = if env::var_os("TMUX").is_none() && env::var_os("TMUX_PANE").is_none() {
        terminal_identity()
    } else {
        None
    };
    let server_socket = local_socket.clone();
    let (ready_send, ready_receive) = sync_channel(1);
    let server_thread = match thread::Builder::new()
        .name("termnav-relay".to_owned())
        .spawn(move || {
            let result = server::serve_ready(
                &server_socket,
                Some(owner_read.as_raw_fd()),
                direct_terminal,
                ready_send,
            );
            drop(owner_read);
            result
        }) {
        Ok(thread) => thread,
        Err(_) => {
            drop(owner_write);
            return run_plain(&binary, arguments);
        }
    };
    if !wait_for_socket(&ready_receive, &server_thread) {
        drop(owner_write);
        let _ = server_thread.join();
        return run_plain(&binary, arguments);
    }

    let enhanced = enhanced_arguments(arguments, destination, &local_socket, &remote_socket);
    let mut child = match ssh_command(&binary)
        .args(&enhanced)
        .env("TERMNAV_PARENT_RELAY", &remote_socket)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            drop(owner_write);
            let _ = server_thread.join();
            return Err(error);
        }
    };
    let status = match wait_for_ssh(&mut child) {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            drop(owner_write);
            let _ = server_thread.join();
            return Err(error);
        }
    };

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
    ssh_command(binary)
        .args(arguments)
        .status()
        .map(process::status_code)
}

pub(crate) fn real_ssh() -> io::Result<PathBuf> {
    let invoking_shim = env::var_os(SSH_SHIM_DIR_ENV).map(PathBuf::from);
    for directory in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        if is_termnav_shim_dir(&directory)
            || invoking_shim
                .as_ref()
                .is_some_and(|shim| same_path(&directory, shim))
        {
            continue;
        }
        let candidate = directory.join("ssh");
        if candidate.is_file() && !is_termnav_shim(&candidate) {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "ssh is not installed",
    ))
}

fn ssh_command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    // These variables describe only the shim-to-Termnav handoff. Never leak
    // them into OpenSSH or commands launched by the remote login: a later SSH
    // from that environment must begin a fresh, independent resolution.
    if let Some(path) = env::var_os(SSH_SHIM_ORIGINAL_PATH_ENV) {
        command.env("PATH", path);
    }
    command
        .env_remove(SSH_SHIM_ACTIVE_ENV)
        .env_remove(SSH_SHIM_DIR_ENV)
        .env_remove(SSH_SHIM_ORIGINAL_PATH_ENV);
    command
}

fn is_termnav_shim_dir(directory: &Path) -> bool {
    // Every source, staged, and installed Termnav shim lives below this
    // provider-owned suffix. Inspect the canonical spelling as well so a PATH
    // symlink cannot disguise another shim as a real SSH client. This catches
    // all coexisting release copies without coupling resolution to whichever
    // binary happened to start the current process.
    let canonical = fs::canonicalize(directory).unwrap_or_else(|_| directory.to_owned());
    canonical.ends_with(Path::new("share/termnav/shims"))
}

fn is_termnav_shim(candidate: &Path) -> bool {
    // The directory convention covers every managed installation. The marker
    // additionally protects copied or relocated shims. The descriptive header
    // is shared by both earlier single-binary releases, so mixed-version fleets
    // remain bounded without parsing shell syntax or comparing whole files.
    let mut prefix = [0_u8; 1024];
    File::open(candidate)
        .and_then(|mut file| file.read(&mut prefix))
        .is_ok_and(|length| {
            let prefix = &prefix[..length];
            [SSH_SHIM_MARKER, LEGACY_SSH_SHIM_MARKER]
                .into_iter()
                .any(|marker| prefix.windows(marker.len()).any(|window| window == marker))
        })
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
    let mut command = ssh_command(binary);
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

fn interactive(arguments: &[OsString], destination: usize, stdin_tty: bool) -> bool {
    let explicit_tty = arguments[..destination]
        .iter()
        .filter_map(|value| value.to_str())
        .any(|value| value.starts_with("-t"));
    stdin_tty || explicit_tty
}

fn resolved_tty(
    settings: &HashMap<String, String>,
    arguments: &[OsString],
    destination: usize,
    stdin_tty: bool,
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
    request == "auto" && !has_remote_command && stdin_tty
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
        index += option_width(value)?;
    }
    None
}

fn option_width(value: &str) -> Option<usize> {
    let flags = value.as_bytes().strip_prefix(b"-")?;
    for (index, flag) in flags.iter().copied().enumerate() {
        if no_value_flag(flag) {
            continue;
        }
        if value_option(flag) {
            // OpenSSH permits both `-p 22` and attached values such as
            // `-p22`. In a cluster like `-vp22`, every byte after the first
            // value-taking flag belongs to that option rather than naming
            // more flags.
            return Some(if index + 1 < flags.len() { 1 } else { 2 });
        }
        return None;
    }
    Some(1)
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
    let _ = ssh_command(binary)
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

fn wait_for_socket(
    ready: &Receiver<()>,
    server_thread: &thread::JoinHandle<io::Result<()>>,
) -> bool {
    ready.recv_timeout(SOCKET_WAIT).is_ok() && !server_thread.is_finished()
}

fn wait_for_ssh(child: &mut Child) -> io::Result<ExitStatus> {
    let mut kill_deadline = None;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if let Some(deadline) = kill_deadline
            && Instant::now() >= deadline
        {
            // A configured wrapper or ProxyCommand may trap the forwarded
            // signal. Bound that cooperation window so the supervising
            // process, its owner pipe, and its relay socket cannot leak
            // indefinitely after the user has terminated the connection.
            let _ = child.kill();
            return child.wait();
        }
        if kill_deadline.is_none()
            && let Some(signal) = server::shutdown_signal()
        {
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
            kill_deadline = Some(Instant::now() + SIGNAL_KILL_GRACE);
        }
        thread::sleep(CHILD_POLL);
    }
}

fn terminal_identity() -> Option<server::DirectTerminal> {
    // Prefer stdout/stderr because redirected stdin may be non-terminal or
    // read-only even when SSH was explicitly allocated a usable terminal.
    for descriptor in [1, 2, 0] {
        let Some(tty) = tty_name(descriptor) else {
            continue;
        };
        let status_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        if status_flags == -1 || status_flags & libc::O_ACCMODE == libc::O_RDONLY {
            continue;
        }
        let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate == -1 {
            continue;
        }
        // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned by this
        // process. DirectTerminal keeps it alive for exactly the SSH relay.
        let output = unsafe { File::from_raw_fd(duplicate) };
        return Some(server::DirectTerminal::captured(
            std::process::id(),
            tty,
            env::var("TERM").unwrap_or_default(),
            output,
        ));
    }
    None
}

fn tty_name(descriptor: i32) -> Option<String> {
    let mut buffer = vec![0_u8; 1024];
    let result = unsafe { libc::ttyname_r(descriptor, buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let value = unsafe { CStr::from_ptr(buffer.as_ptr().cast()) };
    value.to_str().ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{control_mode, destination_index, interactive, resolved_tty};
    use std::collections::HashMap;
    use std::ffi::OsString;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn destination_parser_preserves_option_boundaries() {
        assert_eq!(destination_index(&args(&["-p", "22", "host"])), Some(2));
        assert_eq!(destination_index(&args(&["-p22", "host"])), Some(1));
        assert_eq!(destination_index(&args(&["-vvv", "host"])), Some(1));
        assert_eq!(destination_index(&args(&["-tt", "host"])), Some(1));
        assert_eq!(destination_index(&args(&["-vp22", "host"])), Some(1));
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

    #[test]
    fn tty_classification_uses_real_input_or_explicit_flags() {
        let settings = HashMap::from([("requesttty".to_owned(), "auto".to_owned())]);

        assert!(interactive(&args(&["host"]), 0, true));
        assert!(!interactive(&args(&["host"]), 0, false));
        assert!(interactive(&args(&["-t", "host"]), 1, false));
        assert!(resolved_tty(&settings, &args(&["host"]), 0, true));
        assert!(!resolved_tty(
            &settings,
            &args(&["host", "command"]),
            0,
            true
        ));
    }
}
