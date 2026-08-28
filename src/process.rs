//! Bounded process execution and portable process metadata queries.
//!
//! Navigation runs in the UI hot path, so Linux reads process state directly
//! from procfs. macOS has no procfs; only that platform pays for the bounded
//! `ps` fallback. Keeping both paths here prevents each feature from inventing
//! subtly different PID-reuse and timeout behavior.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Create a pipe whose descriptors cannot leak through an executed child.
///
/// The relay's ownership pipe is a lifetime capability: an inherited writer
/// would keep the listener alive after its actual owner died. Signal wakeups
/// additionally need nonblocking descriptors because a signal handler must
/// never wait for pipe capacity.
pub fn pipe(nonblocking: bool) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `pipe` returns two newly owned descriptors.
    let owned = unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    };
    for descriptor in [&owned.0, &owned.1] {
        let raw = descriptor.as_raw_fd();
        let descriptor_flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        if descriptor_flags == -1
            || unsafe { libc::fcntl(raw, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        if nonblocking {
            set_nonblocking(descriptor)?;
        }
    }
    Ok(owned)
}

/// Run a command with structurally bounded captured output and a hard deadline.
///
/// Navigation's tmux and helper queries return a few compact records, so they
/// stay on this allocation-light path. The macOS `ps eww` fallback can emit an
/// entire large process environment and uses the draining variant below.
pub fn output_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    match wait_for_child(&mut child, timeout)? {
        Some(_) => child.wait_with_output(),
        None => Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out")),
    }
}

/// Run a child to completion behind a hard deadline without capturing output.
///
/// UI-triggered helpers must never inherit an unbounded wait from an external
/// program. On every timeout or wait failure this function kills and reaps the
/// exact child before returning, so callers cannot leak detached work while
/// reporting a failed gesture.
pub(crate) fn status_timeout(command: &mut Command, timeout: Duration) -> io::Result<ExitStatus> {
    command.process_group(0);
    let mut child = command.spawn()?;
    match wait_for_child(&mut child, timeout)? {
        Some(status) => Ok(status),
        None => Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out")),
    }
}

/// Capture a subprocess behind both a hard deadline and a combined byte cap.
///
/// Extension hooks are user-controlled code. A time limit alone is not enough:
/// a child can produce output faster than the timeout and exhaust the parent.
/// Draining both pipes while the child runs also avoids the wait-before-read
/// deadlock inherent in `Command::output` for pipe-sized output.
pub(crate) fn output_timeout_limited(
    command: &mut Command,
    timeout: Duration,
    max_bytes: usize,
) -> io::Result<Output> {
    output_timeout_draining_limited(command, timeout, max_bytes)
}

fn output_timeout_draining(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    output_timeout_draining_limited(command, timeout, usize::MAX)
}

fn output_timeout_draining_limited(
    command: &mut Command,
    timeout: Duration,
    max_bytes: usize,
) -> io::Result<Output> {
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let Some(stdout) = child.stdout.take() else {
        kill_child_group(&mut child);
        let _ = child.wait();
        return Err(io::Error::other("child stdout pipe is unavailable"));
    };
    let Some(stderr) = child.stderr.take() else {
        kill_child_group(&mut child);
        let _ = child.wait();
        return Err(io::Error::other("child stderr pipe is unavailable"));
    };
    let result = (|| {
        set_nonblocking(&stdout)?;
        set_nonblocking(&stderr)?;

        // A child can fill either pipe before it exits. Drain both descriptors
        // in the same bounded polling loop that watches the child, avoiding
        // both wait-then-read deadlock and per-query reader threads on macOS's
        // hot process-inspection path.
        let deadline = Instant::now() + timeout;
        let mut stdout = Some(stdout);
        let mut stderr = Some(stderr);
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let mut status = None;
        loop {
            if let Some(pipe) = stdout.as_mut()
                && !drain_available(
                    pipe,
                    &mut stdout_bytes,
                    max_bytes.saturating_sub(stderr_bytes.len()),
                )?
            {
                stdout = None;
            }
            if let Some(pipe) = stderr.as_mut()
                && !drain_available(
                    pipe,
                    &mut stderr_bytes,
                    max_bytes.saturating_sub(stdout_bytes.len()),
                )?
            {
                stderr = None;
            }
            if status.is_none() {
                status = child.try_wait()?;
            }
            if let Some(status) = status
                && stdout.is_none()
                && stderr.is_none()
            {
                return Ok(Output {
                    status,
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                });
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
            }
            thread::sleep(Duration::from_millis(2));
        }
    })();
    if result.is_err() {
        // Every setup, read, wait, and timeout failure owns the same exact
        // process group. Kill the full group and reap its leader here so future
        // changes cannot accidentally add an early-return path that leaks a
        // helper descendant.
        kill_child_group(&mut child);
        let _ = child.wait();
    }
    result
}

fn set_nonblocking(descriptor: &impl AsRawFd) -> io::Result<()> {
    let raw = descriptor.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_available(
    pipe: &mut impl Read,
    output: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<bool> {
    const MAX_BYTES_PER_TURN: usize = 64 * 1024;

    let mut buffer = [0_u8; 8192];
    let mut drained = 0;
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(false),
            Ok(count) => {
                if count > max_bytes.saturating_sub(output.len()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "command output exceeded limit",
                    ));
                }
                output.extend_from_slice(&buffer[..count]);
                drained += count;
                // A continuously writing or replaced `ps` must not monopolize
                // this loop, starve the other descriptor, or evade the hard
                // process deadline. Normal `ps` records usually fit in one
                // turn; oversized environments make bounded forward progress.
                if drained >= MAX_BYTES_PER_TURN {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn wait_for_child(
    child: &mut std::process::Child,
    timeout: Duration,
) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {}
            Err(error) => {
                kill_child_group(child);
                let _ = child.wait();
                return Err(error);
            }
        }
        if Instant::now() >= deadline {
            // A timeout is an uncertainty boundary. Kill and reap the exact
            // child before returning so a failed UI gesture never accumulates
            // helper processes in the background.
            kill_child_group(child);
            child.wait()?;
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn kill_child_group(child: &mut std::process::Child) {
    // Every bounded command starts a fresh process group. Killing that owned
    // group closes the common shell-wrapper leak where the direct child exits
    // but a grandchild retains pipes, sockets, or remote work. Keep Child::kill
    // as a defensive fallback if the platform rejects the group operation.
    if let Ok(group) = i32::try_from(child.id()) {
        let _ = unsafe { libc::kill(-group, libc::SIGKILL) };
    }
    let _ = child.kill();
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

    environment_from_ps(pid, name)
}

fn environment_from_ps(pid: u32, name: &str) -> Option<String> {
    let mut command = Command::new("ps");
    command.args(["eww", "-p", &pid.to_string(), "-o", "command="]);
    let output = output_timeout_draining(&mut command, Duration::from_secs(2)).ok()?;
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
    let output = output_timeout_draining(&mut command, Duration::from_secs(2)).ok()?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// Return a process parent and selected environment values in one snapshot.
///
/// Parent traversal is part of every boundary navigation gesture. On macOS,
/// querying two variables and the parent independently would spawn three `ps`
/// processes per ancestry level; one snapshot keeps that fallback bounded to
/// a single subprocess while Linux continues using cheap procfs reads.
#[must_use]
pub fn parent_environment(pid: u32, names: &[&str]) -> Option<(u32, Vec<Option<String>>)> {
    if pid == 0 || names.iter().any(|name| !valid_env_name(name)) {
        return None;
    }

    if let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) {
        let parent = status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|value| value.trim().parse().ok())
        })?;
        let environment = fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        let values = names
            .iter()
            .map(|name| environment_bytes(&environment, name))
            .collect();
        return Some((parent, values));
    }

    let mut command = Command::new("ps");
    command.args([
        "eww",
        "-p",
        &pid.to_string(),
        "-o",
        "ppid=",
        "-o",
        "command=",
    ]);
    let output = output_timeout_draining(&mut command, Duration::from_secs(2)).ok()?;
    parse_parent_environment(&String::from_utf8(output.stdout).ok()?, names)
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
    let Ok(output) = output_timeout_draining(&mut command, Duration::from_secs(2)) else {
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
    // `ps` terminates its record with a newline. When the requested variable
    // is the final environment entry, there is no following `NAME=` boundary
    // to stop the parser, so leaving the record terminator attached corrupts
    // path-valued metadata such as TERMNAV_PARENT_RELAY on macOS.
    let output = output.strip_suffix('\n').unwrap_or(output);
    let output = output.strip_suffix('\r').unwrap_or(output);
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

fn environment_bytes(environment: &[u8], name: &str) -> Option<String> {
    let prefix = format!("{name}=").into_bytes();
    environment
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(prefix.as_slice()))
        .and_then(|value| String::from_utf8(value.to_vec()).ok())
}

fn parse_parent_environment(output: &str, names: &[&str]) -> Option<(u32, Vec<Option<String>>)> {
    let output = output.trim_start();
    let boundary = output.find(char::is_whitespace)?;
    let parent = output[..boundary].parse().ok()?;
    let environment = output[boundary..].trim_start();
    Some((
        parent,
        names
            .iter()
            .map(|name| parse_environment(environment, name))
            .collect(),
    ))
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
    use std::os::fd::AsRawFd;
    use std::process::Command;
    #[cfg(target_os = "macos")]
    use std::thread;
    use std::time::Duration;
    #[cfg(target_os = "macos")]
    use std::time::Instant;

    #[cfg(target_os = "macos")]
    use super::environment_from_ps;
    use super::{output_timeout_draining, parse_environment, parse_parent_environment, pipe};

    #[test]
    fn bounded_output_drains_pipes_before_the_child_exits() {
        // Pipe buffers are much smaller than this payload on supported hosts.
        // The child can exit only if stdout and stderr are consumed while it is
        // running, which guards the macOS `ps eww` environment fallback.
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "awk 'BEGIN { for (i = 0; i < 8192; i++) print \"0123456789abcdef\"; for (i = 0; i < 8192; i++) print \"fedcba9876543210\" > \"/dev/stderr\" }'",
        ]);
        let output = output_timeout_draining(&mut command, Duration::from_secs(3))
            .expect("capture output without filling either pipe");

        assert!(output.status.success());
        assert!(output.stdout.len() > 128 * 1024);
        assert!(output.stderr.len() > 128 * 1024);
    }

    #[test]
    fn continuous_output_cannot_evade_the_drain_deadline() {
        // Nonblocking reads must not turn output capture into an unbounded
        // wait when a child keeps the descriptor perpetually readable.
        let mut command = Command::new("sh");
        command.args(["-c", "while :; do printf 0123456789abcdef; done"]);
        let started = std::time::Instant::now();
        let error = output_timeout_draining(&mut command, Duration::from_millis(20))
            .expect_err("long-running child should time out");

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ps_fallback_reads_a_value_after_a_pipe_sized_environment() {
        // GitHub's macOS workers have a large inherited environment. Exercise
        // the actual fallback command with enough unrelated data to fill a pipe
        // so exact-client relay discovery cannot regress to a timeout there.
        let mut command = Command::new("sleep");
        command.arg("5").env("A_TERMNAV_PS_VALUE", "expected");
        // Linux limits each individual environment string before the aggregate
        // ARG_MAX limit, so use several realistic entries to exceed pipe size.
        for index in 0..12 {
            command.env(
                format!("Z_TERMNAV_PS_PADDING_{index}"),
                "x".repeat(12 * 1024),
            );
        }
        let mut child = command
            .spawn()
            .expect("start process with a large environment");
        let value = environment_from_ps(child.id(), "A_TERMNAV_PS_VALUE");
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(value.as_deref(), Some("expected"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ps_fallback_reads_a_final_path_value_without_the_record_newline() {
        // Use `env -i` so the target is provably the final environment field
        // printed by macOS ps. Nested relays commonly add this variable last,
        // and a newline in the parsed value makes the Unix socket unreachable.
        let mut child = Command::new("/usr/bin/env")
            .args([
                "-i",
                "TERM=xterm-256color",
                "TERMNAV_PARENT_RELAY=/tmp/termnav-parent.sock",
                "/bin/sleep",
                "5",
            ])
            .spawn()
            .expect("start process with a final relay environment value");
        // `Command::spawn` returns after fork but does not guarantee that the
        // child has completed env's exec into sleep. Querying during that
        // window finds the assignment in env's argv and legitimately includes
        // `/bin/sleep 5` in the apparent value. Wait for the observable value
        // produced by the final process instead of guessing at exec timing.
        let deadline = Instant::now() + Duration::from_secs(2);
        let value = loop {
            let value = environment_from_ps(child.id(), "TERMNAV_PARENT_RELAY");
            if value.as_deref() == Some("/tmp/termnav-parent.sock") || Instant::now() >= deadline {
                break value;
            }
            thread::sleep(Duration::from_millis(5));
        };
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(value.as_deref(), Some("/tmp/termnav-parent.sock"));
    }

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
    fn ps_environment_excludes_the_record_terminator_from_the_final_value() {
        let output = "tmux TERM=xterm-256color TERMNAV_PARENT_RELAY=/tmp/relay.sock\n";

        assert_eq!(
            parse_environment(output, "TERMNAV_PARENT_RELAY").as_deref(),
            Some("/tmp/relay.sock")
        );
    }

    #[test]
    fn environment_name_must_match_a_complete_assignment() {
        let output = "ssh host OTHER_PATH=/wrong PATH=/right";

        assert_eq!(parse_environment(output, "PATH").as_deref(), Some("/right"));
    }

    #[test]
    fn parent_and_environment_share_one_ps_record() {
        let (parent, values) = parse_parent_environment(
            "  42 ssh host TMUX=/tmp/tmux,1,0 TMUX_PANE=%7 LABEL=two words TERM=xterm",
            &["TMUX", "TMUX_PANE", "LABEL"],
        )
        .expect("parse process snapshot");

        assert_eq!(parent, 42);
        assert_eq!(values[0].as_deref(), Some("/tmp/tmux,1,0"));
        assert_eq!(values[1].as_deref(), Some("%7"));
        assert_eq!(values[2].as_deref(), Some("two words"));
    }

    #[test]
    fn relay_pipes_are_close_on_exec_and_wake_pipes_are_nonblocking() {
        for nonblocking in [false, true] {
            let (read, write) = pipe(nonblocking).expect("create relay pipe");
            for descriptor in [&read, &write] {
                let raw = descriptor.as_raw_fd();
                let descriptor_flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
                let status_flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
                assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
                assert_eq!(status_flags & libc::O_NONBLOCK != 0, nonblocking);
            }
        }
    }
}
