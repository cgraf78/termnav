use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use termnav::click::{Input, Target, resolve};

mod common;

fn temporary_root() -> PathBuf {
    common::temporary_root("click")
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn shared_link_route_fixture_matches_the_native_resolver() {
    let fixture = include_str!("../test/fixtures/nvim-link-routes.tsv");
    for line in fixture.lines().filter(|line| !line.starts_with('#')) {
        let fields = line.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 6, "malformed fixture row: {line}");
        let context = if fields[5] == "-" { "" } else { fields[5] };
        assert_eq!(
            resolve(&Input {
                hyperlink: fields[1].to_owned(),
                cwd: fields[3].to_owned(),
                ..Input::default()
            }),
            Some(Target::File {
                target: fields[2].to_owned(),
                source: fields[4].to_owned(),
                context: context.to_owned(),
            }),
            "fixture case {}",
            fields[0]
        );
    }
}

#[test]
fn browser_targets_are_returned_to_the_exact_click_client() {
    let root = temporary_root();
    std::fs::create_dir_all(&root).expect("create test root");
    let _cleanup = Cleanup(root.clone());
    let tty = root.join("client.out");
    std::fs::write(&tty, []).expect("create fake client tty");

    let status = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args([
            "tmux",
            "follow-click",
            "",
            "CVE-2024-12345",
            "",
            "0",
            "/repo",
            tty.to_str().expect("tty path is utf-8"),
        ])
        .status()
        .expect("route browser target");
    assert!(status.success());
    let output = std::fs::read(&tty).expect("read terminal escape");
    assert!(output.starts_with(b"\x1b]1337;SetUserVar=TERMNAV_OPEN_URL="));
}

#[test]
fn file_targets_reuse_nvim_in_the_current_tmux_window() {
    let root = temporary_root();
    // Route every process through this fixture explicitly. Tool fallback order
    // has focused unit coverage; this integration test owns click dispatch and
    // must never reach a hosted runner's real tmux socket.
    let bin = root.join(".local/bin");
    std::fs::create_dir_all(&bin).expect("create fake bin");
    let _cleanup = Cleanup(root.clone());
    let log = root.join("tmux.log");
    let tmux = bin.join("tmux");
    std::fs::write(
        &tmux,
        "#!/bin/sh\nif [ \"$1\" = list-panes ]; then printf '%s\\t%s\\n' '%2' nvim; exit 0; fi\nprintf '%s\\n' \"$*\" >>\"$TERMNAV_CLICK_LOG\"\n",
    )
    .expect("write fake tmux");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755))
        .expect("make fake tmux executable");
    let status = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args([
            "tmux",
            "follow-click",
            "nvim-open://src/main.rs:12:4",
            "",
            "",
            "0",
            "/repo",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", &root)
        .env("TERMNAV_CLICK_LOG", &log)
        .env("TMUX", "/tmp/tmux.sock,1,0")
        .env("TMUX_PANE", "%1")
        .env("XDG_STATE_HOME", root.join("state"))
        .status()
        .expect("route file target");
    assert!(status.success());
    let calls = std::fs::read_to_string(log).expect("read tmux calls");
    assert!(calls.contains("send-keys -t %2 -l"));
    assert!(calls.contains("fnameescape(\"/repo/src/main.rs\")"));
    assert!(calls.contains("cursor(12,4)"));
}
