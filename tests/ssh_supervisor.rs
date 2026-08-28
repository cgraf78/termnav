use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> (PathBuf, PathBuf, PathBuf, Cleanup) {
    // Rust tests run concurrently. Some macOS filesystems expose timestamps
    // too coarsely for time alone to distinguish two fixtures created in the
    // same process, so use a process-local monotonic identity instead.
    let nonce = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    // Keep XDG_RUNTIME_DIR short enough that these tests exercise their stated
    // branch on macOS instead of taking the socket-length fallback first.
    let root = PathBuf::from("/tmp").join(format!("tn-{}-{nonce:x}", std::process::id()));
    let bin = root.join("ssh");
    let log = root.join("ssh.log");
    fs::create_dir_all(&root).expect("create fixture");
    fs::write(
        &bin,
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == -G ]]; then
  printf 'CONFIG\n' >>"$TERMNAV_TEST_SSH_LOG"
  printf '%s\n' 'requesttty auto' 'sessiontype default' \
    'remotecommand none' 'exitonforwardfailure no' \
    'controlpath /tmp/TermNav-Control-Exact'
  exit 0
fi
kind=SESSION
for argument in "$@"; do
  [[ $argument == cancel ]] && kind=CANCEL
done
printf '%s\n' "$kind" >>"$TERMNAV_TEST_SSH_LOG"
printf '<%s>\n' "$@" >>"$TERMNAV_TEST_SSH_LOG"
"#,
    )
    .expect("write fake ssh");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod fake ssh");
    let cleanup = Cleanup(root.clone());
    (root, bin, log, cleanup)
}

#[test]
fn enhanced_login_uses_one_session_and_local_only_cleanup() {
    let (root, _binary, log, _cleanup) = fixture();
    fs::create_dir(root.join("runtime")).expect("create XDG runtime root");
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["ssh", "-t", "duo.example.invalid"])
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .env("TERMNAV_TEST_SSH_LOG", &log)
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .output()
        .expect("run SSH supervisor");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(log).expect("read ssh log");
    assert_eq!(log.lines().filter(|line| *line == "SESSION").count(), 1);
    assert_eq!(log.lines().filter(|line| *line == "CONFIG").count(), 1);
    assert_eq!(log.lines().filter(|line| *line == "CANCEL").count(), 1);
    assert!(log.contains("<RemoteForward="));
    assert!(log.contains("<SendEnv=TERMNAV_PARENT_RELAY>"));
    assert!(log.contains("</tmp/TermNav-Control-Exact>"));
    assert!(log.contains("<ProxyCommand=/bin/false>"));
}

#[test]
fn noninteractive_ssh_is_exactly_one_plain_invocation() {
    let (root, _binary, log, _cleanup) = fixture();
    fs::create_dir(root.join("runtime")).expect("create XDG runtime root");
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["ssh", "plain.example.invalid"])
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .env("TERMNAV_TEST_SSH_LOG", &log)
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .output()
        .expect("run plain SSH");

    assert!(output.status.success());
    let log = fs::read_to_string(log).expect("read ssh log");
    assert_eq!(log, "SESSION\n<plain.example.invalid>\n");
}

#[test]
fn path_lookup_skips_a_non_executable_ssh_shadow() {
    let (root, _binary, log, _cleanup) = fixture();
    let shadow = root.join("shadow");
    fs::create_dir(&shadow).expect("create shadow directory");
    fs::write(shadow.join("ssh"), "not executable\n").expect("write shadow");

    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["ssh", "plain.example.invalid"])
        .env(
            "PATH",
            format!("{}:{}:/usr/bin:/bin", shadow.display(), root.display()),
        )
        .env("TERMNAV_TEST_SSH_LOG", &log)
        .output()
        .expect("run plain SSH through executable fallback");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(log).expect("read ssh log"),
        "SESSION\n<plain.example.invalid>\n"
    );
}

#[test]
fn failed_session_spawn_joins_and_removes_the_connection_relay() {
    let (root, binary, log, _cleanup) = fixture();
    fs::create_dir(root.join("runtime")).expect("create XDG runtime root");
    fs::write(
        &binary,
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == -G ]]; then
  rm -f -- "$0"
  printf '%s\n' 'requesttty auto' 'sessiontype default' \
    'remotecommand none' 'exitonforwardfailure no'
  exit 0
fi
exit 99
"#,
    )
    .expect("replace fake ssh");
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["ssh", "-t", "gone.example.invalid"])
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .env("TERMNAV_TEST_SSH_LOG", &log)
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .output()
        .expect("run SSH supervisor");

    assert!(!output.status.success());
    let relay_root = root
        .join("runtime")
        .join(format!("termnav-{}", unsafe { libc::getuid() }));
    let sockets = fs::read_dir(relay_root)
        .expect("read relay root")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("relay-"))
        .collect::<Vec<_>>();
    assert!(sockets.is_empty(), "orphaned relay paths: {sockets:?}");
}

#[test]
fn long_xdg_runtime_falls_back_to_a_portable_socket_path() {
    let (root, _binary, log, _cleanup) = fixture();
    let runtime = root.join("x".repeat(120));
    fs::create_dir(&runtime).expect("create long XDG runtime root");
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["ssh", "-t", "mac-path.example.invalid"])
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .env("TERMNAV_TEST_SSH_LOG", &log)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .expect("run SSH supervisor");

    assert!(output.status.success());
    let arguments = fs::read_to_string(log).expect("read ssh log");
    let forward = arguments
        .lines()
        .find_map(|line| line.strip_prefix("<RemoteForward="))
        .and_then(|line| line.strip_suffix('>'))
        .expect("find remote forward");
    let (_, local_socket) = forward.split_once(':').expect("split remote forward");
    assert!(
        !PathBuf::from(local_socket).starts_with(&runtime),
        "forward did not use the short portable runtime: {arguments}"
    );
    assert!(
        local_socket.len() <= 103,
        "portable socket exceeded sockaddr_un: {local_socket}"
    );
    assert!(
        PathBuf::from(local_socket).parent().is_some_and(
            |parent| parent.ends_with(format!("termnav-{}", unsafe { libc::getuid() }))
        ),
        "portable socket escaped the private Termnav root: {local_socket}"
    );
}

#[test]
fn unsafe_runtime_state_falls_back_to_plain_ssh() {
    let (root, _binary, log, _cleanup) = fixture();
    let runtime = root.join("runtime");
    let outside = root.join("outside");
    fs::create_dir(&runtime).expect("create XDG runtime root");
    fs::create_dir(&outside).expect("create symlink target");
    std::os::unix::fs::symlink(
        &outside,
        runtime.join(format!("termnav-{}", unsafe { libc::getuid() })),
    )
    .expect("create unsafe runtime symlink");

    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["ssh", "-t", "fallback.example.invalid"])
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .env("TERMNAV_TEST_SSH_LOG", &log)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .expect("run SSH supervisor");

    assert!(output.status.success());
    let arguments = fs::read_to_string(log).expect("read ssh log");
    assert_eq!(
        arguments, "CONFIG\nSESSION\n<-t>\n<fallback.example.invalid>\n",
        "relay setup failure must preserve one config probe and one SSH session"
    );
}
