use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

mod common;

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ChildCleanup(Child);

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            if let Ok(group) = i32::try_from(self.0.id()) {
                let _ = unsafe { libc::kill(-group, libc::SIGKILL) };
            }
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_for_file(path: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        if let Some(status) = child.try_wait().expect("inspect termnav eza") {
            panic!("termnav eza exited before the fixture blocked: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for fake eza readiness");
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("inspect termnav eza") {
            return status;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("timed out waiting for termnav eza to exit");
}

fn rewritten(bytes: &[u8], host: &str) -> Vec<u8> {
    let pattern = b"\x1b]8;;file:///";
    let replacement = format!("\x1b]8;;file://{host}/").into_bytes();
    let mut output = Vec::new();
    let mut remaining = bytes;
    while let Some(index) = remaining
        .windows(pattern.len())
        .position(|window| window == pattern)
    {
        output.extend_from_slice(&remaining[..index]);
        output.extend_from_slice(&replacement);
        remaining = &remaining[index + pattern.len()..];
    }
    output.extend_from_slice(remaining);
    output
}

#[test]
fn remote_eza_streams_rewritten_binary_output_and_preserves_stderr_and_status() {
    let root = common::temporary_root("eza-stream");
    fs::create_dir_all(&root).expect("create eza fixture root");
    let _cleanup = Cleanup(root.clone());
    let fake_eza = root.join("eza");
    let early_path = root.join("early");
    let early_stderr_path = root.join("early-stderr");
    let payload_path = root.join("payload");
    let stderr_path = root.join("stderr");
    let ready_path = root.join("ready");
    let release_path = root.join("release");

    // The first record must become observable while the child is still blocked.
    // The payloads deliberately contain invalid UTF-8, NULs, and enough data to
    // fill ordinary pipe buffers so the test exercises byte-safe concurrent IO.
    let early = b"before:\x1b]8;;file:///tmp/early\x1b\\early\x1b]8;;\x1b\\\n";
    let mut payload = Vec::with_capacity(256 * 1024);
    while payload.len() < 256 * 1024 {
        payload.extend_from_slice(b"\0\xff:\x1b]8;;file:///tmp/large\x1b\\value\n");
    }
    let early_stderr = vec![0xa5; 96 * 1024];
    let stderr = vec![0x5a; 192 * 1024];
    fs::write(&early_path, early).expect("write early output");
    fs::write(&early_stderr_path, &early_stderr).expect("write early stderr");
    fs::write(&payload_path, &payload).expect("write large stdout payload");
    fs::write(&stderr_path, &stderr).expect("write large stderr payload");
    fs::write(
        &fake_eza,
        r#"#!/bin/sh
if [ "${2-}" = "--version" ] || [ "${1-}" = "--version" ]; then
  exit 0
fi
cat "$TERMNAV_TEST_EZA_EARLY"
cat "$TERMNAV_TEST_EZA_EARLY_STDERR" >&2
: >"$TERMNAV_TEST_EZA_READY"
while [ ! -e "$TERMNAV_TEST_EZA_RELEASE" ]; do
  sleep 0.01
done
cat "$TERMNAV_TEST_EZA_PAYLOAD"
cat "$TERMNAV_TEST_EZA_STDERR" >&2
exit 37
"#,
    )
    .expect("write fake eza");
    fs::set_permissions(&fake_eza, fs::Permissions::from_mode(0o700))
        .expect("make fake eza executable");

    let host = "remote.example";
    let mut child = ChildCleanup(
        Command::new(env!("CARGO_BIN_EXE_termnav"))
            .args(["eza", "/tmp"])
            .env("TERMNAV_EZA_BINARY", &fake_eza)
            .env("TERMNAV_REMOTE_LINK_HOST", host)
            .env("TERMNAV_TEST_EZA_EARLY", &early_path)
            .env("TERMNAV_TEST_EZA_EARLY_STDERR", &early_stderr_path)
            .env("TERMNAV_TEST_EZA_PAYLOAD", &payload_path)
            .env("TERMNAV_TEST_EZA_STDERR", &stderr_path)
            .env("TERMNAV_TEST_EZA_READY", &ready_path)
            .env("TERMNAV_TEST_EZA_RELEASE", &release_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("start termnav eza"),
    );
    let mut stdout = child.0.stdout.take().expect("capture termnav stdout");
    let mut stderr_pipe = child.0.stderr.take().expect("capture termnav stderr");
    let expected_early = rewritten(early, host);
    let early_length = expected_early.len();
    let (early_sender, early_receiver) = mpsc::channel();
    let stdout_reader = thread::spawn(move || {
        let mut output = vec![0; early_length];
        stdout
            .read_exact(&mut output)
            .expect("read streamed eza prefix");
        early_sender
            .send(output.clone())
            .expect("report streamed eza prefix");
        stdout
            .read_to_end(&mut output)
            .expect("read remaining stdout");
        output
    });
    let early_stderr_length = early_stderr.len();
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    let stderr_reader = thread::spawn(move || {
        let mut output = vec![0; early_stderr_length];
        stderr_pipe
            .read_exact(&mut output)
            .expect("read streamed eza stderr");
        stderr_sender
            .send(output.clone())
            .expect("report streamed eza stderr");
        stderr_pipe
            .read_to_end(&mut output)
            .expect("read remaining stderr");
        output
    });

    wait_for_file(&ready_path, &mut child.0);
    let streamed = early_receiver.recv_timeout(Duration::from_secs(1));
    let streamed_stderr = stderr_receiver.recv_timeout(Duration::from_secs(1));
    fs::write(&release_path, b"release\n").expect("release fake eza");
    let status = wait_for_exit(&mut child.0);
    let stdout = stdout_reader.join().expect("join stdout reader");
    let actual_stderr = stderr_reader.join().expect("join stderr reader");

    assert_eq!(
        streamed.expect("rewritten prefix was buffered until child exit"),
        expected_early
    );
    assert_eq!(status.code(), Some(37));
    let mut expected_stdout = rewritten(early, host);
    expected_stdout.extend_from_slice(&rewritten(&payload, host));
    assert_eq!(stdout, expected_stdout);
    assert_eq!(
        streamed_stderr.expect("stderr was buffered until child exit"),
        early_stderr
    );
    let mut expected_stderr = early_stderr;
    expected_stderr.extend_from_slice(&stderr);
    assert_eq!(actual_stderr, expected_stderr);
}

#[test]
fn downstream_write_failure_kills_the_complete_eza_process_group() {
    let root = common::temporary_root("eza-write-failure");
    fs::create_dir_all(&root).expect("create eza failure fixture root");
    let _cleanup = Cleanup(root.clone());
    let fake_eza = root.join("eza");
    let ready_path = root.join("ready");
    let release_path = root.join("release");
    let pids_path = root.join("pids");
    fs::write(
        &fake_eza,
        r#"#!/bin/sh
if [ "${2-}" = "--version" ] || [ "${1-}" = "--version" ]; then
  exit 0
fi
sh -c 'trap "" TERM; exec sleep 60' &
child=$!
printf '%s %s\n' "$$" "$child" >"$TERMNAV_TEST_EZA_PIDS"
: >"$TERMNAV_TEST_EZA_READY"
while [ ! -e "$TERMNAV_TEST_EZA_RELEASE" ]; do sleep 0.01; done
printf '\033]8;;file:///tmp/failure\033\\failure\n'
wait "$child"
"#,
    )
    .expect("write failing-output eza fixture");
    fs::set_permissions(&fake_eza, fs::Permissions::from_mode(0o700))
        .expect("make failing-output eza executable");

    let mut child = ChildCleanup(
        Command::new(env!("CARGO_BIN_EXE_termnav"))
            .args(["eza", "/tmp"])
            .env("TERMNAV_EZA_BINARY", &fake_eza)
            .env("TERMNAV_REMOTE_LINK_HOST", "remote.example")
            .env("TERMNAV_TEST_EZA_PIDS", &pids_path)
            .env("TERMNAV_TEST_EZA_READY", &ready_path)
            .env("TERMNAV_TEST_EZA_RELEASE", &release_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("start termnav with a downstream pipe"),
    );
    wait_for_file(&ready_path, &mut child.0);
    drop(
        child
            .0
            .stdout
            .take()
            .expect("close downstream stdout reader"),
    );
    fs::write(&release_path, b"release\n").expect("release failing-output eza fixture");

    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("inspect failed eza adapter") {
            break status;
        }
        if Instant::now() >= deadline {
            let pids = fs::read_to_string(&pids_path).unwrap_or_default();
            for pid in pids
                .split_whitespace()
                .filter_map(|value| value.parse::<i32>().ok())
            {
                let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            panic!("termnav hung joining stderr after downstream write failure");
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success(), "downstream write failure was hidden");

    let pids = fs::read_to_string(&pids_path).expect("read eza process identities");
    let pids = pids
        .split_whitespace()
        .map(|value| value.parse::<u32>().expect("parse eza process identity"))
        .collect::<Vec<_>>();
    let deadline = Instant::now() + Duration::from_secs(1);
    while pids.iter().copied().any(process_alive) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        pids.iter().copied().all(|pid| !process_alive(pid)),
        "downstream failure left an eza process alive: {pids:?}"
    );
}
