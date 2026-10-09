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

// The XTVERSION reply tmux records for a WezTerm client.
const WEZTERM: &str = "WezTerm 20240203-110809-5046fc22";

struct Fixture {
    root: PathBuf,
    tty: PathBuf,
    outer_tty: PathBuf,
    log: PathBuf,
    client_pid: u32,
    /// The tty the parent server reports for the hosting pane; the inner
    /// client's tty unless a test needs stale ancestry.
    pane_tty: Option<PathBuf>,
}

impl Fixture {
    /// Create a fake tmux whose client query prints `reply` (or fails).
    ///
    /// `%s` in a reply is the fake client tty. Each row ends with the client
    /// pid; pid 0 has no process environment to inspect.
    fn new(reply: Option<&str>) -> Self {
        let body = match reply {
            Some(reply) => format!("printf '{reply}\\n' \"$TERMNAV_TEST_TTY\""),
            None => "exit 1".to_owned(),
        };
        Self::with_body(&body)
    }

    /// Create a fake tmux pair: the caller's server answers `inner`, whose
    /// `%s` fields are the inner client tty and pid, and `tmux -S` (a parent
    /// server) answers `outer`, whose `%s` fields are the hosting pane's tty
    /// and then the outer client tty.
    fn nested(inner: &str, outer: &str) -> Self {
        Self::with_body(&format!(
            "if [ \"$1\" = -S ]; then\n\
             printf '{outer}\\n' \"$TERMNAV_TEST_PANE_TTY\" \"$TERMNAV_TEST_OUTER_TTY\"\n\
             else\n\
             printf '{inner}\\n' \"$TERMNAV_TEST_TTY\" \"$TERMNAV_TEST_CLIENT_PID\"\n\
             fi"
        ))
    }

    fn with_body(body: &str) -> Self {
        let root = common::temporary_root("open-url");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("create fake bin");
        let tty = root.join("client.out");
        std::fs::write(&tty, []).expect("create fake client tty");
        let outer_tty = root.join("outer-client.out");
        std::fs::write(&outer_tty, []).expect("create fake outer client tty");
        let log = root.join("tmux.log");
        let script = bin.join("tmux");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"$TERMNAV_TEST_LOG\"\n{body}\n"),
        )
        .expect("write fake tmux");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make fake tmux executable");
        Self {
            root,
            tty,
            outer_tty,
            log,
            client_pid: 0,
            pane_tty: None,
        }
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
            // Inside tmux the caller's own terminal variables describe where
            // the pane was created, not where it is shown; prove they are
            // ignored by making them claim WezTerm.
            .env("TERM_PROGRAM", "WezTerm")
            .env("WEZTERM_PANE", "1")
            .env("TERMNAV_TEST_TTY", &self.tty)
            .env("TERMNAV_TEST_OUTER_TTY", &self.outer_tty)
            .env(
                "TERMNAV_TEST_PANE_TTY",
                self.pane_tty.as_ref().unwrap_or(&self.tty),
            )
            .env("TERMNAV_TEST_CLIENT_PID", self.client_pid.to_string())
            .env("TERMNAV_TEST_LOG", &self.log)
            .output()
            .expect("run termnav open-url")
    }

    fn written(&self) -> Vec<u8> {
        std::fs::read(&self.tty).expect("read fake client tty")
    }

    fn written_outer(&self) -> Vec<u8> {
        std::fs::read(&self.outer_tty).expect("read fake outer client tty")
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

fn passthrough() -> Vec<u8> {
    format!("\x1bPtmux;\x1b\x1b]1337;SetUserVar=TERMNAV_OPEN_URL={ENCODED}\x07\x1b\\").into_bytes()
}

/// A live process standing in for a tmux client, with the tmux identity of
/// the pane it was started in.
struct ClientProcess(std::process::Child);

impl ClientProcess {
    fn start(tmux: &str, pane: &str) -> Self {
        // Its identity variables stand for the client's terminal; never let
        // the test runner's own terminal leak in.
        let child = Command::new("sleep")
            .arg("30")
            .env_remove("TERM_PROGRAM")
            .env_remove("WEZTERM_PANE")
            .env_remove("STY")
            .env("TMUX", tmux)
            .env("TMUX_PANE", pane)
            .spawn()
            .expect("start fake tmux client process");
        Self(child)
    }
}

impl Drop for ClientProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn tmux_requests_go_raw_to_the_attached_client_tty() {
    let fixture = Fixture::new(Some(&format!(
        "@1\\n0\\t1\\t@1\\t{WEZTERM}\\t%s\\txterm-256color\\t0"
    )));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), raw());
    assert!(output.stdout.is_empty());
    assert!(
        fixture
            .tmux_calls()
            .contains("display-message -p -t %1 #{window_id}\t#{pane_tty} ; list-clients -t %1 -F")
    );
}

#[test]
fn wezterm_clients_with_a_tmux_terminal_name_receive_one_passthrough_frame() {
    // The framing rule is unchanged: a WezTerm client whose terminal name is
    // tmux* still gets the frame it got before identification existed.
    let fixture = Fixture::new(Some(&format!(
        "@1\\n0\\t1\\t@1\\t{WEZTERM}\\t%s\\ttmux-256color\\t0"
    )));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), passthrough());
}

#[test]
fn tmux_clients_without_a_local_parent_receive_one_passthrough_frame() {
    // A remote tmux attached from local tmux: the parent is across SSH and
    // cannot be inspected, so its passthrough is trusted as before. The
    // frame does not depend on the client's terminal name.
    for termname in ["tmux-256color", "xterm-256color"] {
        let fixture = Fixture::new(Some(&format!(
            "@1\\n0\\t1\\t@1\\ttmux 3.4\\t%s\\t{termname}\\t0"
        )));
        let output = fixture.run(&["open-url", URL]);

        assert_eq!(output.status.code(), Some(0), "{termname}: {output:?}");
        assert_eq!(fixture.written(), passthrough(), "{termname}");
        assert!(!fixture.tmux_calls().contains("-S"), "{termname}");
    }
}

#[test]
fn local_nested_tmux_requests_go_raw_to_the_outer_wezterm_client() {
    let mut fixture = Fixture::nested(
        "@5\\n0\\t1\\t@5\\ttmux 3.4\\t%s\\ttmux-256color\\t%s",
        &format!("@1\\t%s\\n0\\t1\\t@1\\t{WEZTERM}\\t%s\\txterm-256color\\t0"),
    );
    let outer = fixture.root.join("outer.sock");
    let client = ClientProcess::start(&format!("{},7,0", outer.display()), "%9");
    fixture.client_pid = client.0.id();
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written_outer(), raw());
    assert!(fixture.written().is_empty());
    assert!(fixture.tmux_calls().contains(&format!(
        "-S {} display-message -p -t %9 #{{window_id}}\t#{{pane_tty}} ; list-clients -t %9 -F",
        outer.display()
    )));
}

#[test]
fn local_nested_tmux_declines_when_the_outer_client_is_not_wezterm() {
    let mut fixture = Fixture::nested(
        "@5\\n0\\t1\\t@5\\ttmux 3.4\\t%s\\ttmux-256color\\t%s",
        "@1\\t%s\\n0\\t1\\t@1\\tiTerm2 3.5.0\\t%s\\txterm-256color\\t0",
    );
    let outer = fixture.root.join("outer.sock");
    let client = ClientProcess::start(&format!("{},7,0", outer.display()), "%9");
    fixture.client_pid = client.0.id();
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(fixture.written().is_empty());
    assert!(fixture.written_outer().is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not known to be WezTerm"));
}

#[test]
fn a_client_nested_in_its_own_server_is_not_followed_again() {
    // A parent that resolves back to the caller's server must not loop.
    let mut fixture = Fixture::nested(
        "@5\\n0\\t1\\t@5\\ttmux 3.4\\t%s\\ttmux-256color\\t%s",
        &format!("@1\\t%s\\n0\\t1\\t@1\\t{WEZTERM}\\t%s\\txterm-256color\\t0"),
    );
    let client = ClientProcess::start("/tmp/termnav-open-url,1,0", "%9");
    fixture.client_pid = client.0.id();
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), passthrough());
    assert!(fixture.written_outer().is_empty());
}

#[test]
fn stale_tmux_ancestry_is_not_followed() {
    // The ancestor's pane does not own the client's tty, e.g. a terminal
    // emulator launched from that pane runs the client. The parent is then
    // treated like one that cannot be inspected.
    let mut fixture = Fixture::nested(
        "@5\\n0\\t1\\t@5\\ttmux 3.4\\t%s\\ttmux-256color\\t%s",
        &format!("@1\\t%s\\n0\\t1\\t@1\\t{WEZTERM}\\t%s\\txterm-256color\\t0"),
    );
    fixture.pane_tty = Some(fixture.root.join("other-pane.tty"));
    let outer = fixture.root.join("outer.sock");
    let client = ClientProcess::start(&format!("{},7,0", outer.display()), "%9");
    fixture.client_pid = client.0.id();
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), passthrough());
    assert!(fixture.written_outer().is_empty());
}

#[test]
fn stale_ancestry_of_an_unidentified_client_declines() {
    // Without a tmux reply the layer was only guessed from the client's
    // terminal name; ancestry that does not host it cannot confirm it.
    let mut fixture = Fixture::nested(
        "@5\\n0\\t1\\t@5\\t\\t%s\\ttmux-256color\\t%s",
        &format!("@1\\t%s\\n0\\t1\\t@1\\t{WEZTERM}\\t%s\\txterm-256color\\t0"),
    );
    fixture.pane_tty = Some(fixture.root.join("other-pane.tty"));
    let outer = fixture.root.join("outer.sock");
    let client = ClientProcess::start(&format!("{},7,0", outer.display()), "%9");
    fixture.client_pid = client.0.id();
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(fixture.written().is_empty());
    assert!(fixture.written_outer().is_empty());
}

#[test]
fn tmux_clients_that_are_not_wezterm_decline_without_writing() {
    for row in [
        "0\\t1\\t@1\\tiTerm2 3.5.0\\t%s\\txterm-256color\\t0",
        "0\\t1\\t@1\\tghostty 1.1.0\\t%s\\twezterm\\t0",
        // Older tmux records no reply; the terminal name then decides.
        "0\\t1\\t@1\\t\\t%s\\txterm-256color\\t0",
    ] {
        let fixture = Fixture::new(Some(&format!("@1\\n{row}")));
        let output = fixture.run(&["open-url", URL]);

        assert_eq!(output.status.code(), Some(3), "{row}: {output:?}");
        assert!(fixture.written().is_empty(), "{row}");
        assert!(output.stdout.is_empty(), "{row}");
    }
}

#[test]
fn tmux_clients_without_a_reply_are_identified_by_terminal_name() {
    let fixture = Fixture::new(Some("@1\\n0\\t1\\t@1\\t\\t%s\\twezterm\\t0"));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), raw());
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
    let fixture = Fixture::new(Some(&format!(
        "@1\\n1\\t9\\t@1\\t{WEZTERM}\\t/dev/null\\txterm\\t0\\n\
         0\\t1\\t@1\\t{WEZTERM}\\t%s\\txterm-256color\\t0",
    )));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), raw());
}

#[test]
fn vscode_clients_are_skipped_in_favor_of_capable_ones() {
    // VS Code's xterm.js ignores the user var. Even when most recently active
    // it must not swallow the request while a WezTerm client shows the pane.
    let fixture = Fixture::new(Some(&format!(
        "@1\\n0\\t9\\t@1\\txterm.js(6.1.0)\\t/dev/null\\txterm-256color\\t0\\n\
         0\\t1\\t@1\\t{WEZTERM}\\t%s\\txterm-256color\\t0",
    )));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), raw());
}

#[test]
fn sessions_with_only_vscode_clients_fail_so_callers_fall_back() {
    let fixture = Fixture::new(Some(
        "@1\\n0\\t9\\t@1\\txterm.js(6.1.0)\\t%s\\txterm-256color\\t0",
    ));
    let output = fixture.run(&["open-url", URL]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(fixture.written().is_empty());
}

#[test]
fn commands_without_pane_context_query_their_own_session() {
    let fixture = Fixture::new(Some(&format!(
        "@1\\n0\\t1\\t@1\\t{WEZTERM}\\t%s\\txterm-256color\\t0"
    )));
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
    let fixture = Fixture::new(Some(&format!(
        "@1\\n0\\t1\\t@1\\t{WEZTERM}\\t%s\\txterm-256color\\t0"
    )));
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
    let fixture = Fixture::new(Some(&format!(
        "@1\\n0\\t1\\t@1\\t{WEZTERM}\\t%s\\txterm-256color\\t0"
    )));
    assert_eq!(fixture.run(&["open-url"]).status.code(), Some(2));
    assert_eq!(fixture.run(&["open-url", URL, URL]).status.code(), Some(2));
    let help = fixture.run(&["open-url", "--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert_eq!(help.stdout, b"usage: termnav open-url [--tty PATH] URL\n");
    assert!(fixture.written().is_empty());
}

/// Clear every terminal identity a test runner may inherit, then apply
/// `identity` as `NAME=value` pairs.
fn terminal_identity<'a>(
    command: &'a mut Command,
    term: &str,
    identity: &[&str],
) -> &'a mut Command {
    command
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("TERM_PROGRAM")
        .env_remove("WEZTERM_PANE")
        .env_remove("STY")
        .env("TERM", term);
    for pair in identity {
        let (name, value) = pair.split_once('=').unwrap();
        command.env(name, value);
    }
    command
}

/// Run outside tmux in a new session, optionally owning `controlling` as its
/// controlling terminal. Standard output is captured to prove it is never
/// used as a fallback destination.
fn run_in_session(controlling: Option<&Path>, term: &str, identity: &[&str]) -> Output {
    let path = controlling.map(|path| CString::new(path.to_str().unwrap()).unwrap());
    let mut command = Command::new(env!("CARGO_BIN_EXE_termnav"));
    command.args(["open-url", URL]);
    terminal_identity(&mut command, term, identity);
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
    for (term, identity, expected) in [
        ("xterm-256color", &["TERM_PROGRAM=WezTerm"][..], raw()),
        ("xterm-256color", &["WEZTERM_PANE=3"][..], raw()),
        // WezTerm's own terminfo name also crosses SSH.
        ("wezterm", &[][..], raw()),
        // An SSH session started inside local tmux inherits its TERM; the
        // escape then crosses that outer tmux in one passthrough frame.
        ("tmux-256color", &[][..], passthrough()),
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
        let output = run_in_session(Some(&slave), term, identity);

        assert_eq!(
            output.status.code(),
            Some(0),
            "{term} {identity:?}: {output:?}"
        );
        assert!(output.stdout.is_empty(), "{term}: stdout fallback used");
        assert_eq!(reader.join().unwrap(), expected, "{term} {identity:?}");
    }
}

#[test]
fn sessions_without_a_terminal_fail_instead_of_writing_stdout() {
    let output = run_in_session(None, "xterm-256color", &["TERM_PROGRAM=WezTerm"]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
}

fn run_outside_tmux(arguments: &[&str], term: &str, identity: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_termnav"));
    command.args(arguments);
    terminal_identity(&mut command, term, identity)
        .output()
        .expect("run termnav open-url outside tmux")
}

#[test]
fn explicit_terminals_replace_the_controlling_terminal_outside_tmux() {
    let fixture = Fixture::new(None);
    let tty = fixture.tty.to_str().unwrap();

    let output = run_outside_tmux(
        &["open-url", "--tty", tty, URL],
        "xterm-256color",
        &["TERM_PROGRAM=WezTerm"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), raw());

    std::fs::write(&fixture.tty, []).unwrap();
    let output = run_outside_tmux(&["open-url", "--tty", tty, URL], "screen-256color", &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), passthrough());

    // A tmux pane whose caller unset TMUX: tmux sets TERM_PROGRAM even when
    // its default-terminal is xterm-like, and the frame follows the layer.
    std::fs::write(&fixture.tty, []).unwrap();
    let output = run_outside_tmux(
        &["open-url", "--tty", tty, URL],
        "xterm-256color",
        &["TERM_PROGRAM=tmux"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fixture.written(), passthrough());
    assert!(fixture.tmux_calls().is_empty());
}

#[test]
fn terminals_not_known_to_be_wezterm_decline_without_writing() {
    let fixture = Fixture::new(None);
    let tty = fixture.tty.to_str().unwrap();
    for (term, identity) in [
        // SSH from WezTerm without forwarded variables is indistinguishable
        // from any other xterm-compatible terminal.
        ("xterm-256color", &[][..]),
        ("", &[][..]),
        ("xterm-256color", &["TERM_PROGRAM=iTerm.app"][..]),
        // A VS Code window started from a WezTerm shell keeps WEZTERM_PANE.
        (
            "xterm-256color",
            &["TERM_PROGRAM=vscode", "WEZTERM_PANE=3"][..],
        ),
        ("wezterm", &["TERM_PROGRAM=Apple_Terminal"][..]),
        // GNU screen cannot forward the passthrough frame tmux layers get.
        ("screen-256color", &["STY=1234.pts-0.host"][..]),
        (
            "screen-256color",
            &["STY=1234.pts-0.host", "WEZTERM_PANE=3"][..],
        ),
    ] {
        let output = run_outside_tmux(&["open-url", "--tty", tty, URL], term, identity);
        assert_eq!(
            output.status.code(),
            Some(3),
            "{term} {identity:?}: {output:?}"
        );
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("not known to be WezTerm"));
    }
    assert!(fixture.written().is_empty());

    // Declining happens before the device is opened, so a missing terminal
    // is still a decline rather than a failure.
    let output = run_in_session(None, "xterm-256color", &[]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
}

#[test]
fn explicit_terminals_are_ignored_inside_tmux() {
    let fixture = Fixture::new(Some(&format!(
        "@1\\n0\\t1\\t@1\\t{WEZTERM}\\t%s\\txterm-256color\\t0"
    )));
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
        &["TERM_PROGRAM=WezTerm"],
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
        let output = run_outside_tmux(arguments, "xterm-256color", &["TERM_PROGRAM=WezTerm"]);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty());
    }
}
