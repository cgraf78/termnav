use std::path::PathBuf;
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::symlink;

fn tmux(socket: &PathBuf, arguments: &[&str]) -> std::process::Output {
    Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(arguments)
        .env_remove("TMUX")
        .output()
        .expect("run tmux")
}

#[test]
fn claim_dims_the_parent_and_release_restores_its_style() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }
    let socket = std::env::temp_dir().join(format!("termnav-focus-{}.sock", std::process::id()));
    let _ = tmux(&socket, &["kill-server"]);
    assert!(
        tmux(&socket, &["new-session", "-d", "-s", "focus"])
            .status
            .success()
    );
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = tmux(&self.0, &["kill-server"]);
        }
    }
    let _cleanup = Cleanup(socket.clone());
    let pane = String::from_utf8(tmux(&socket, &["display-message", "-p", "#{pane_id}"]).stdout)
        .expect("pane is utf-8")
        .trim()
        .to_owned();
    assert!(
        tmux(
            &socket,
            &["set-option", "-g", "@termnav_inactive_style", "bg=#011627"]
        )
        .status
        .success()
    );

    let token = "aaaaaaaaaaaaaaaaaaaaaaaa";
    let claim = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args([
            "tmux",
            "focus",
            "claim",
            "--parent-tmux",
            socket.to_str().expect("socket is utf-8"),
            "--parent-pane",
            &pane,
            "--token",
            token,
            "--lease-ms",
            "100",
        ])
        .status()
        .expect("claim focus");
    assert!(claim.success());
    let active_style = String::from_utf8(
        tmux(
            &socket,
            &["show-options", "-pqv", "-t", &pane, "window-active-style"],
        )
        .stdout,
    )
    .expect("style is utf-8");
    assert_eq!(active_style.trim(), "bg=#011627");

    let release = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args([
            "tmux",
            "focus",
            "release",
            "--parent-tmux",
            socket.to_str().expect("socket is utf-8"),
            "--parent-pane",
            &pane,
            "--token",
            token,
        ])
        .status()
        .expect("release focus");
    assert!(release.success());
    let shown = tmux(
        &socket,
        &["show-options", "-pqv", "-t", &pane, "@termnav_child_focus"],
    );
    assert!(shown.stdout.is_empty());
}

#[cfg(unix)]
#[test]
fn focus_state_refuses_a_symlinked_runtime_leaf() {
    let root = std::env::temp_dir().join(format!(
        "termnav-focus-runtime-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let owner = root.join(format!("termnav-{}", unsafe { libc::getuid() }));
    let target = root.join("target");
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    std::fs::create_dir_all(&owner).expect("create owner runtime root");
    std::fs::create_dir_all(&target).expect("create symlink target");
    symlink(&target, owner.join("focus")).expect("create unsafe focus symlink");

    let status = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args([
            "tmux",
            "focus",
            "claim",
            "--parent-tmux",
            "/missing/tmux.sock",
            "--parent-pane",
            "%1",
            "--token",
            "aaaaaaaaaaaaaaaaaaaaaaaa",
            "--lease-ms",
            "100",
        ])
        .env("XDG_RUNTIME_DIR", &root)
        .status()
        .expect("run focus command");
    assert!(!status.success());
    assert_eq!(
        std::fs::read_dir(&target)
            .expect("read symlink target")
            .count(),
        0,
        "unsafe symlink target must remain untouched"
    );
}
