use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(unix)]
use std::os::unix::fs::symlink;

mod common;

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture_path(bin: &Path) -> OsString {
    let ambient = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(bin.to_owned()).chain(std::env::split_paths(&ambient)))
        .expect("assemble focus fixture PATH")
}

/// Keep the server, its scheduled jobs, and both CLI calls in one private environment.
struct FocusServer {
    root: PathBuf,
    bin: PathBuf,
    path: OsString,
}

impl FocusServer {
    fn new() -> Self {
        let root = common::socket_root();
        let bin = common::temporary_root("focus-bin");
        fs::create_dir(&bin).expect("create executable fixture directory");
        // This case must observe manual release regardless of scheduling. The
        // tmux-focus suite tests real expiry after a publisher crashes.
        common::write_executable(
            &bin.join("termnav"),
            &format!("#!{}\nexit 0\n", common::fixture_shell()),
        );
        let path = fixture_path(&bin);
        Self { root, bin, path }
    }

    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("TMPDIR", &self.root)
            .env("PATH", &self.path)
            .env("SHELL", common::fixture_shell())
            .env("TERM", "xterm-256color");
        command
    }

    fn socket(&self) -> PathBuf {
        self.root.join("s")
    }

    fn focus(&self, action: &str, pane: &str, options: &[&str]) {
        let output = self
            .command(Path::new(env!("CARGO_BIN_EXE_termnav")))
            .args(["tmux", "focus", action, "--parent-tmux"])
            .arg(self.socket())
            .args(["--parent-pane", pane, "--token", "aaaaaaaaaaaaaaaaaaaaaaaa"])
            .args(options)
            .output()
            .expect("run focus command");
        assert!(
            output.status.success(),
            "focus {action}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn tmux(&self, arguments: &[&str]) -> std::io::Result<Output> {
        self.command(Path::new("tmux"))
            .arg("-S")
            .arg(self.socket())
            .args(["-f", "/dev/null"])
            .args(arguments)
            .output()
    }

    fn query(&self, arguments: &[&str]) -> String {
        let output = self.tmux(arguments).expect("run private tmux");
        assert!(
            output.status.success(),
            "tmux {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 tmux response")
            .trim()
            .to_owned()
    }
}

impl Drop for FocusServer {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.bin);
    }
}

#[test]
fn claim_dims_the_parent_and_release_restores_its_style() {
    if Command::new("tmux").arg("-V").output().is_err() {
        // CI sets TERMNAV_REQUIRE_TMUX wherever it runs this test.
        assert!(
            std::env::var_os("TERMNAV_REQUIRE_TMUX").is_none_or(|value| value != "1"),
            "TERMNAV_REQUIRE_TMUX=1, but tmux does not run"
        );
        return;
    }
    let server = FocusServer::new();
    server.query(&["new-session", "-d", "-s", "focus", "exec cat"]);
    let pane = server.query(&["display-message", "-p", "#{pane_id}"]);
    server.query(&["set-option", "-g", "@termnav_inactive_style", "bg=#011627"]);
    let style_query = ["show-options", "-pqv", "-t", &pane, "window-active-style"];
    let original_style = server.query(&style_query);

    server.focus("claim", &pane, &["--lease-ms", "100"]);
    assert_eq!(server.query(&style_query), "bg=#011627");

    server.focus("release", &pane, &[]);
    assert!(
        server
            .query(&["show-options", "-pqv", "-t", &pane, "@termnav_child_focus"])
            .is_empty()
    );
    assert_eq!(server.query(&style_query), original_style);
}

#[test]
fn focus_fixture_leaves_caller_configuration_helpers_and_runtime_untouched() {
    let caller_root = common::temporary_root("focus-caller");
    fs::create_dir(&caller_root).expect("create synthetic caller root");
    let caller = Cleanup(caller_root);
    let runtime = caller.0.join("runtime");
    let long_tmp = caller.0.join("x".repeat(108));
    let bin = caller.0.join("bin");
    for directory in [&runtime, &long_tmp, &bin] {
        fs::create_dir_all(directory).expect("create synthetic caller directory");
    }
    fs::write(
        caller.0.join(".tmux.conf"),
        "run-shell 'touch \"$HOME/config-loaded\"'\n",
    )
    .expect("seed caller tmux config");
    common::write_executable(
        &bin.join("termnav"),
        &format!(
            "#!{}\ntouch \"${{0%/*}}/../helper-ran\"\n",
            common::fixture_shell()
        ),
    );
    // Exercise the real test entry point in a subprocess: changing HOME or
    // TMPDIR inside this test process would race Rust's other test threads.
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "claim_dims_the_parent_and_release_restores_its_style",
        ])
        .env("HOME", &caller.0)
        .env("XDG_CONFIG_HOME", &caller.0)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("TMPDIR", &long_tmp)
        .env("PATH", fixture_path(&bin))
        .output()
        .expect("run focus fixture under caller environment");
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!caller.0.join("config-loaded").exists());
    assert!(!caller.0.join("helper-ran").exists());
    assert_eq!(fs::read_dir(runtime).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn focus_state_refuses_a_symlinked_runtime_leaf() {
    let root = common::socket_root();
    let owner = root.join(format!("termnav-{}", unsafe { libc::getuid() }));
    let target = root.join("target");
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
