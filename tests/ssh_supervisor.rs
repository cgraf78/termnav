use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

fn fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("termnav-ssh-{}-{name}", std::process::id()));
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
    (root, bin, log)
}

#[test]
fn enhanced_login_uses_one_session_and_local_only_cleanup() {
    let (root, binary, log) = fixture("enhanced");
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["ssh", "duo.example.invalid"])
        .env("TERMNAV_SSH_BINARY", &binary)
        .env("TERMNAV_TEST_SSH_LOG", &log)
        .env("TERMNAV_TEST_STDIN_TTY", "1")
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
    let (root, binary, log) = fixture("plain");
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["ssh", "plain.example.invalid"])
        .env("TERMNAV_SSH_BINARY", &binary)
        .env("TERMNAV_TEST_SSH_LOG", &log)
        .env("TERMNAV_TEST_STDIN_TTY", "0")
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .output()
        .expect("run plain SSH");

    assert!(output.status.success());
    let log = fs::read_to_string(log).expect("read ssh log");
    assert_eq!(log, "SESSION\n<plain.example.invalid>\n");
}
