use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::{Duration, Instant};

mod common;

const URL: &str = "https://example.com/path?a=1&b=two three";
// base64 of URL, computed independently of the implementation under test.
const ENCODED: &str = "aHR0cHM6Ly9leGFtcGxlLmNvbS9wYXRoP2E9MSZiPXR3byB0aHJlZQ==";

struct Fixture {
    root: PathBuf,
    tty: PathBuf,
    log: PathBuf,
}

impl Fixture {
    /// Create a fake tmux whose client query prints `reply` (or fails).
    fn new(reply: Option<&str>) -> Self {
        let root = common::temporary_root("open-url");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("create fake bin");
        let tty = root.join("client.out");
        std::fs::write(&tty, []).expect("create fake client tty");
        let log = root.join("tmux.log");
        let body = match reply {
            Some(reply) => format!("printf '{reply}\\n' \"$TERMNAV_TEST_TTY\""),
            None => "exit 1".to_owned(),
        };
        let script = bin.join("tmux");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"$TERMNAV_TEST_LOG\"\n{body}\n"),
        )
        .expect("write fake tmux");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make fake tmux executable");
        Self { root, tty, log }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_termnav"))
            .args(arguments)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("TMUX", "/tmp/termnav-open-url,1,0")
            .env("TMUX_PANE", "%1")
            .env("TERMNAV_TEST_TTY", &self.tty)
            .env("TERMNAV_TEST_LOG", &self.log)
            .output()
            .expect("run termnav open-url")
    }

    fn written(&self) -> Vec<u8> {
        std::fs::read(&self.tty).expect("read fake client tty")
    }

    fn tmux_calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn raw() -> Vec<u8> {
    format!("\x1b]1337;SetUserVar=TERMNAV_OPEN_URL={ENCODED}\x07").into_bytes()
}

#[test]
fn tmux_requests_go_raw_to_the_attached_client_tty() {
    let fixture = Fixture::new(Some("@1\\n0\\t1\\t@1\\t\\t%s\\txterm-256color"));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), raw());
    assert!(output.stdout.is_empty());
    assert!(
        fixture
            .tmux_calls()
            .contains("display-message -p -t %1 #{window_id} ; list-clients -t %1 -F")
    );
}

#[test]
fn nested_tmux_clients_receive_one_passthrough_frame() {
    let fixture = Fixture::new(Some("@1\\n0\\t1\\t@1\\t\\t%s\\ttmux-256color"));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let expected =
        format!("\x1bPtmux;\x1b\x1b]1337;SetUserVar=TERMNAV_OPEN_URL={ENCODED}\x07\x1b\\");
    assert_eq!(fixture.written(), expected.into_bytes());
}

#[test]
fn detached_tmux_sessions_fail_without_writing() {
    let fixture = Fixture::new(Some("@1"));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(fixture.written().is_empty());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no attached tmux client"));
}

#[test]
fn control_mode_clients_never_receive_requests() {
    // The control client is the most recently active, but its tty carries the
    // control protocol; the ordinary client must be chosen instead.
    let fixture = Fixture::new(Some(
        "@1\\n1\\t9\\t@1\\t\\t/dev/null\\txterm\\n0\\t1\\t@1\\t\\t%s\\txterm-256color",
    ));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), raw());
}

#[test]
fn vscode_clients_are_skipped_in_favor_of_capable_ones() {
    // VS Code's xterm.js ignores the user var. Even when most recently active
    // it must not swallow the request while a WezTerm client shows the pane.
    let fixture = Fixture::new(Some(
        "@1\\n0\\t9\\t@1\\txterm.js(6.1.0)\\t/dev/null\\txterm-256color\\n\
         0\\t1\\t@1\\tWezTerm 20240203\\t%s\\txterm-256color",
    ));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), raw());
}

#[test]
fn sessions_with_only_vscode_clients_fail_so_callers_fall_back() {
    let fixture = Fixture::new(Some(
        "@1\\n0\\t9\\t@1\\txterm.js(6.1.0)\\t%s\\txterm-256color",
    ));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(fixture.written().is_empty());
}

#[test]
fn commands_without_pane_context_query_their_own_session() {
    let fixture = Fixture::new(Some("@1\\n0\\t1\\t@1\\t\\t%s\\txterm-256color"));
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["open-url", URL])
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", fixture.root.join("bin").display()),
        )
        .env("TMUX", "/tmp/termnav-open-url,1,7")
        .env_remove("TMUX_PANE")
        .env("TERMNAV_TEST_TTY", &fixture.tty)
        .env("TERMNAV_TEST_LOG", &fixture.log)
        .output()
        .expect("run termnav open-url");

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(fixture.tmux_calls().contains("list-clients -t $7 -F"));
    assert_eq!(fixture.written(), raw());
}

#[test]
fn failed_tmux_queries_fail_without_writing() {
    let fixture = Fixture::new(None);
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(fixture.written().is_empty());
    assert!(output.stdout.is_empty());
}

#[test]
fn invalid_urls_are_usage_errors_that_never_reach_the_terminal() {
    let fixture = Fixture::new(Some("@1\\n0\\t1\\t@1\\t\\t%s\\txterm-256color"));
    for url in [
        "",
        "example.com",
        "file:///etc/passwd",
        "https://x/\x1b]0;x\x07",
    ] {
        let output = fixture.run(&["open-url", url]);
        assert_eq!(output.status.code(), Some(2), "{url:?}: {output:?}");
    }
    assert!(fixture.written().is_empty());
    assert!(fixture.tmux_calls().is_empty());
}

#[test]
fn arity_and_help_follow_the_cli_contract() {
    let fixture = Fixture::new(Some("@1\\n0\\t1\\t@1\\t\\t%s\\txterm-256color"));
    assert_eq!(fixture.run(&["open-url"]).status.code(), Some(2));
    assert_eq!(fixture.run(&["open-url", URL, URL]).status.code(), Some(2));
    let help = fixture.run(&["open-url", "--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert_eq!(help.stdout, b"usage: termnav open-url [--tty PATH] URL\n");
    assert!(fixture.written().is_empty());
}

/// Run outside tmux in a new session, optionally owning `controlling` as its
/// controlling terminal. Standard output is captured to prove it is never
/// used as a fallback destination.
fn run_in_session(controlling: Option<&Path>, term: &str) -> Output {
    let path = controlling.map(|path| CString::new(path.to_str().unwrap()).unwrap());
    let mut command = Command::new(env!("CARGO_BIN_EXE_termnav"));
    command
        .args(["open-url", URL])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("TERM", term);
    // SAFETY: the hook calls only async-signal-safe functions on data
    // prepared before fork.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(path) = &path {
                let descriptor = libc::open(path.as_ptr(), libc::O_RDWR);
                if descriptor == -1 || libc::ioctl(descriptor, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(descriptor);
            }
            Ok(())
        });
    }
    command
        .output()
        .expect("run termnav open-url in a new session")
}

/// `ptsname` returns static storage; serialize concurrent test threads.
static PTSNAME: Mutex<()> = Mutex::new(());

fn pseudo_terminal() -> (OwnedFd, PathBuf) {
    // SAFETY: plain libc pty allocation; every return value is checked.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0, "posix_openpt failed");
        let master = OwnedFd::from_raw_fd(master);
        assert_eq!(libc::grantpt(master.as_raw_fd()), 0);
        assert_eq!(libc::unlockpt(master.as_raw_fd()), 0);
        let _guard = PTSNAME.lock().unwrap();
        let name = libc::ptsname(master.as_raw_fd());
        assert!(!name.is_null(), "ptsname failed");
        let path = PathBuf::from(CStr::from_ptr(name).to_str().unwrap());
        (master, path)
    }
}

/// Read the pty concurrently until `expected` bytes arrive or time runs out.
///
/// Reading must overlap the child's lifetime: on BSD-derived kernels a
/// session leader's exit drains and then revokes its controlling terminal, so
/// output left unread until after `wait` can block the exit or be discarded.
fn read_pty(master: &OwnedFd, expected: usize) -> std::thread::JoinHandle<Vec<u8>> {
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    let mut file = File::from(master.try_clone().unwrap());
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buffer = [0; 4096];
        while bytes.len() < expected && Instant::now() < deadline {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        bytes
    })
}

#[test]
fn plain_sessions_write_to_the_controlling_terminal() {
    for (term, expected) in [
        ("xterm-256color", raw()),
        (
            // An SSH session started inside local tmux inherits its TERM; the
            // escape then crosses that outer tmux in one passthrough frame.
            "tmux-256color",
            format!("\x1bPtmux;\x1b\x1b]1337;SetUserVar=TERMNAV_OPEN_URL={ENCODED}\x07\x1b\\")
                .into_bytes(),
        ),
    ] {
        let (master, slave) = pseudo_terminal();
        // Hold the slave open so the master keeps buffered output readable
        // after the child exits instead of reporting a hangup.
        let _slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&slave)
            .expect("open pty slave");
        let reader = read_pty(&master, expected.len());
        let output = run_in_session(Some(&slave), term);

        assert_eq!(output.status.code(), Some(0), "{term}: {output:?}");
        assert!(output.stdout.is_empty(), "{term}: stdout fallback used");
        assert_eq!(reader.join().unwrap(), expected, "{term}");
    }
}

#[test]
fn sessions_without_a_terminal_fail_instead_of_writing_stdout() {
    let output = run_in_session(None, "xterm-256color");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
}

fn run_outside_tmux(arguments: &[&str], term: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(arguments)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("TERM", term)
        .output()
        .expect("run termnav open-url outside tmux")
}

#[test]
fn explicit_terminals_replace_the_controlling_terminal_outside_tmux() {
    let fixture = Fixture::new(None);
    let tty = fixture.tty.to_str().unwrap();

    let output = run_outside_tmux(&["open-url", "--tty", tty, URL], "xterm-256color");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), raw());

    std::fs::write(&fixture.tty, []).unwrap();
    let output = run_outside_tmux(&["open-url", "--tty", tty, URL], "screen-256color");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let expected =
        format!("\x1bPtmux;\x1b\x1b]1337;SetUserVar=TERMNAV_OPEN_URL={ENCODED}\x07\x1b\\");
    assert_eq!(fixture.written(), expected.into_bytes());
    assert!(fixture.tmux_calls().is_empty());
}

#[test]
fn explicit_terminals_are_ignored_inside_tmux() {
    let fixture = Fixture::new(Some("@1\\n0\\t1\\t@1\\t\\t%s\\txterm-256color"));
    let other = fixture.root.join("caller.out");
    std::fs::write(&other, []).unwrap();

    let output = fixture.run(&["open-url", "--tty", other.to_str().unwrap(), URL]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), raw());
    assert!(std::fs::read(&other).unwrap().is_empty());
}

#[test]
fn unusable_explicit_terminals_fail_without_writing_stdout() {
    let fixture = Fixture::new(None);
    let missing = fixture.root.join("missing/tty");
    let output = run_outside_tmux(
        &["open-url", "--tty", missing.to_str().unwrap(), URL],
        "xterm-256color",
    );

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
}

#[test]
fn malformed_terminal_options_are_usage_errors() {
    for arguments in [
        &["open-url", "--tty"][..],
        &["open-url", "--tty", "", URL][..],
        &["open-url", "--tty", URL][..],
        &["open-url", "--terminal", "/dev/null", URL][..],
        &["open-url", "--help", URL][..],
    ] {
        let output = run_outside_tmux(arguments, "xterm-256color");
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty());
    }
}
