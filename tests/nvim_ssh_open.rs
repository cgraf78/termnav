use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("termnav-nvim-ssh-{}-{name}", std::process::id()));
    fs::create_dir_all(&path).expect("create fixture directory");
    path
}

#[test]
fn remote_open_uses_one_exact_mux_attempt_without_a_check_race() {
    let root = directory("arguments");
    let binary = root.join("ssh");
    let log = root.join("ssh.log");
    fs::write(
        &binary,
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == -G ]]; then
  printf '%s\n' 'controlpath /tmp/TermNav-Exact-Master'
  exit 0
fi
printf '<%s>\n' "$@" >"$TERMNAV_TEST_SSH_LOG"
"#,
    )
    .expect("write fake ssh");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("chmod fake ssh");

    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["nvim", "ssh-open", "duo-host", "/tmp/example.txt:4"])
        .env("TERMNAV_SSH_BINARY", &binary)
        .env("TERMNAV_TEST_SSH_LOG", &log)
        .env("TERMNAV_SSH_CONTROL_HOSTS", "duo-host")
        .output()
        .expect("run remote opener");

    assert!(output.status.success());
    let log = fs::read_to_string(log).expect("read ssh log");
    assert!(log.contains("</tmp/TermNav-Exact-Master>"));
    assert!(log.contains("<ProxyCommand=/bin/false>"));
    assert!(log.contains("<ProxyJump=none>"));
    assert!(log.contains("<CanonicalizeHostname=no>"));
    assert!(log.contains("<KbdInteractiveAuthentication=no>"));
    assert!(!log.contains("<check>"));
    assert_eq!(log.lines().filter(|line| *line == "<duo-host>").count(), 1);
}

#[test]
fn vanished_master_cannot_reach_the_configured_proxy_or_authentication() {
    let root = directory("race");
    let wrapper = root.join("ssh");
    let config = root.join("ssh_config");
    let sentinel = root.join("network-attempted");
    fs::write(
        &config,
        format!(
            "Host race.invalid\n  ControlPath {}/missing-master\n  ProxyCommand /bin/sh -c 'touch {}'\n",
            root.display(),
            sentinel.display()
        ),
    )
    .expect("write ssh config");
    fs::write(
        &wrapper,
        format!(
            "#!/usr/bin/env bash\nexec /usr/bin/ssh -F '{}' \"$@\"\n",
            config.display()
        ),
    )
    .expect("write ssh wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("chmod wrapper");

    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["nvim", "ssh-open", "race.invalid", "/tmp/example.txt"])
        .env("TERMNAV_SSH_BINARY", &wrapper)
        .env("TERMNAV_SSH_CONTROL_HOSTS", "race.invalid")
        .output()
        .expect("run race fixture");

    assert_eq!(output.status.code(), Some(12));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        !sentinel.exists(),
        "the configured ProxyCommand would be an unauthorized network path"
    );
}
