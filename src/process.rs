//! Bounded process execution and portable process metadata queries.
//!
//! Navigation runs in the UI hot path, so Linux reads process state directly
//! from procfs. macOS has no procfs; only that platform pays for the bounded
//! `ps` fallback. Keeping both paths here prevents each feature from inventing
//! subtly different PID-reuse and timeout behavior.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Run a command with captured output and a hard deadline.
pub fn output_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;

    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            // A timeout is an uncertainty boundary. Kill and reap the exact
            // child before returning so a failed UI gesture never accumulates
            // helper processes in the background.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
        }
        thread::sleep(Duration::from_millis(2));
    }
}

/// Return one process environment value through procfs or the macOS `ps` form.
#[must_use]
pub fn environment(pid: u32, name: &str) -> Option<String> {
    if pid == 0 || !valid_env_name(name) {
        return None;
    }

    let path = format!("/proc/{pid}/environ");
    match fs::read(path) {
        Ok(data) => {
            let prefix = format!("{name}=").into_bytes();
            return data
                .split(|byte| *byte == 0)
                .find_map(|entry| entry.strip_prefix(prefix.as_slice()))
                .and_then(|value| String::from_utf8(value.to_vec()).ok());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }

    let mut command = Command::new("ps");
    command.args(["eww", "-p", &pid.to_string(), "-o", "command="]);
    let output = output_timeout(&mut command, Duration::from_secs(2)).ok()?;
    parse_environment(&String::from_utf8(output.stdout).ok()?, name)
}

/// Return a live process's parent PID without assuming procfs exists.
#[must_use]
pub fn parent(pid: u32) -> Option<u32> {
    if pid == 0 {
        return None;
    }

    if let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) {
        return status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|value| value.trim().parse().ok())
        });
    }

    let mut command = Command::new("ps");
    command.args(["-p", &pid.to_string(), "-o", "ppid="]);
    let output = output_timeout(&mut command, Duration::from_secs(2)).ok()?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// Confirm a live process still owns the tty captured with its route identity.
#[must_use]
pub fn tty_matches(pid: u32, expected: &str) -> bool {
    let proc_path = format!("/proc/{pid}/fd/0");
    if let Ok(actual) = fs::read_link(proc_path) {
        return actual.as_os_str() == OsStr::new(expected);
    }

    let mut command = Command::new("ps");
    command.args(["-p", &pid.to_string(), "-o", "tty="]);
    let Ok(output) = output_timeout(&mut command, Duration::from_secs(2)) else {
        return false;
    };
    let Ok(mut actual) = String::from_utf8(output.stdout) else {
        return false;
    };
    actual = actual.trim().to_owned();
    if !actual.is_empty() && !actual.starts_with('/') {
        actual = format!("/dev/{actual}");
    }
    actual == expected
}

/// Extract one variable from the environment suffix printed by `ps eww`.
///
/// Values may contain spaces, so splitting the whole command line on
/// whitespace is incorrect. An environment value ends only where whitespace
/// is followed by another syntactically valid `NAME=` prefix.
#[must_use]
pub fn parse_environment(output: &str, name: &str) -> Option<String> {
    if !valid_env_name(name) {
        return None;
    }
    let bytes = output.as_bytes();
    let needle = format!("{name}=").into_bytes();
    let start = bytes.windows(needle.len()).position(|window| {
        window == needle
            && (window.as_ptr() == bytes.as_ptr() || {
                let index = window.as_ptr() as usize - bytes.as_ptr() as usize;
                bytes[index - 1].is_ascii_whitespace()
            })
    })? + needle.len();

    let mut end = bytes.len();
    let mut index = start;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            let candidate = index + 1;
            if candidate < bytes.len() && env_assignment_at(bytes, candidate) {
                end = index;
                break;
            }
        }
        index += 1;
    }
    String::from_utf8(bytes[start..end].to_vec()).ok()
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn env_assignment_at(bytes: &[u8], start: usize) -> bool {
    let Some(first) = bytes.get(start) else {
        return false;
    };
    if !first.is_ascii_alphabetic() && *first != b'_' {
        return false;
    }
    let mut index = start + 1;
    while let Some(byte) = bytes.get(index) {
        if *byte == b'=' {
            return true;
        }
        if !byte.is_ascii_alphanumeric() && *byte != b'_' {
            return false;
        }
        index += 1;
    }
    false
}

/// Extract a portable integer exit code, treating signal death as failure.
#[must_use]
pub fn status_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::parse_environment;

    #[test]
    fn ps_environment_values_keep_spaces() {
        let output = "ssh host PATH=/usr/local/bin:/usr/bin LABEL=two words TERM=xterm-256color";

        assert_eq!(
            parse_environment(output, "LABEL").as_deref(),
            Some("two words")
        );
        assert_eq!(
            parse_environment(output, "PATH").as_deref(),
            Some("/usr/local/bin:/usr/bin")
        );
    }

    #[test]
    fn environment_name_must_match_a_complete_assignment() {
        let output = "ssh host OTHER_PATH=/wrong PATH=/right";

        assert_eq!(parse_environment(output, "PATH").as_deref(), Some("/right"));
    }
}
