use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use termnav::click::{Input, Target, resolve};
use termnav::nvim::open::LinkKind;

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
                kind: if fields[1].starts_with("file://") {
                    LinkKind::File
                } else {
                    LinkKind::Location
                },
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
fn moving_output_between_captures_never_falls_back_to_another_file() {
    let root = temporary_root();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create capture test tools");
    let _cleanup = Cleanup(root.clone());
    let log = root.join("opened");
    script(
        &bin.join("tmux"),
        r#"
case "$1" in
  capture-pane)
    case " $* " in
      *" -J "*) printf '/tmp/clicked.rs\n';;
      *)
        if test -e "$CAPTURE_COUNT"; then printf '/tmp/changed.rs\n'
        else touch "$CAPTURE_COUNT"; printf '/tmp/clicked.rs\n'; fi;;
    esac;;
  list-panes) printf '%%99\tnvim\n';;
  send-keys) printf '%s\n' "$*" >>"$OPEN_LOG";;
  *) exit 1;;
esac
"#,
    );
    let result = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args([
            "tmux",
            "follow-click",
            "",
            "/tmp/fallback.rs",
            "/tmp/clicked.rs",
            "5",
            "/repo",
            "",
            "%1",
            "0",
            "0",
            "0",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("TMUX", "/tmp/termnav-capture-fixture.sock,1,0")
        .env("CAPTURE_COUNT", root.join("reads"))
        .env("OPEN_LOG", &log)
        .output()
        .expect("run changing capture");
    assert!(result.status.success(), "{:?}", result.stderr);
    assert!(!log.exists(), "a changed snapshot opened a file");
}

fn hung_editor_tmux(phase: &str) {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    let root = temporary_root();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let _cleanup = Cleanup(root.clone());
    script(
        &bin.join("tmux"),
        r#"
case "$1" in
 list-panes)
  if test "$HANG_PHASE" = output; then exec sleep 30; fi
  printf '%%99\tnvim\n';;
 send-keys) exec sleep 30;;
 *) exit 1;;
esac
"#,
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["nvim", "open", "link", "/tmp/test.rs", "/repo", "terminal"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("TMUX", "/tmp/termnav-editor-fixture.sock,1,0")
        .env("TMUX_PANE", "%1")
        .env("HANG_PHASE", phase)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        match child.try_wait().unwrap() {
            Some(status) => {
                assert_eq!(status.code(), Some(1));
                break;
            }
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => {
                // Only this invocation created the group, and try_wait proved
                // its leader still alive; kill descendants before reaping it.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                panic!("native opener did not bound its tmux {phase} request");
            }
        }
    }
}

#[test]
fn hung_tmux_query_does_not_leave_the_editor_opener_running() {
    hung_editor_tmux("output");
}

#[test]
fn hung_tmux_status_does_not_leave_the_editor_opener_running() {
    hung_editor_tmux("status");
}

#[test]
fn browser_targets_are_returned_to_the_exact_click_client() {
    let root = temporary_root();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create fake bin");
    let _cleanup = Cleanup(root.clone());
    let tty = root.join("client.out");
    let other = root.join("other.out");
    std::fs::write(&tty, []).expect("create fake client tty");
    std::fs::write(&other, []).expect("create fake other client tty");
    // Two WezTerm clients; the other one is listed first. The fake tmux is
    // the only server this test can reach.
    script(
        &bin.join("tmux"),
        &format!(
            "case \" $* \" in *\" list-clients \"*)\n\
             printf '0\\tWezTerm 1\\t%s\\txterm-256color\\t0\\n' '{}'\n\
             printf '0\\tWezTerm 1\\t%s\\txterm-256color\\t0\\n' '{}'\n\
             exit 0;;\nesac\nexit 1\n",
            other.display(),
            tty.display()
        ),
    );

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
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("TMUX", "/tmp/termnav-fake.sock,1,0")
        .env_remove("TMUX_PANE")
        .env("BROWSER", "false")
        .status()
        .expect("route browser target");
    assert!(status.success());
    let output = std::fs::read(&tty).expect("read terminal escape");
    assert!(output.starts_with(b"\x1b]1337;SetUserVar=TERMNAV_OPEN_URL="));
    assert!(std::fs::read(&other).expect("read other client").is_empty());
}

/// Write an executable `#!/bin/sh` script.
///
/// Tests run on parallel threads. A thread that spawns a process while this
/// one holds a script open for writing hands that descriptor to its child
/// until the child execs, and running the script meanwhile fails with
/// ETXTBSY ("Text file busy"). A short-lived `sh` writes the file instead,
/// so no test thread ever holds it open. Concurrent spawns can inherit the
/// pipe to it, which only delays its end of input until they exec.
fn script(path: &std::path::Path, body: &str) {
    let mut writer = Command::new("/bin/sh")
        .args(["-c", "cat >\"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("start script writer");
    writer
        .stdin
        .take()
        .expect("script writer stdin")
        .write_all(format!("#!/bin/sh\n{body}").as_bytes())
        .expect("write script");
    let status = writer.wait().expect("wait for script writer");
    assert!(
        status.success(),
        "writing {} failed: {status}",
        path.display()
    );
}

/// Run one URL click through a fake tmux. A plain `list-clients` prints
/// `clients` (the click lookup's five fields); the clicked pane's own client
/// query prints `pane_clients` (seven fields). `$BROWSER` is `browser`.
/// Returns the tmux calls.
fn click_url(
    root: &std::path::Path,
    tty: &str,
    clients: &str,
    pane_clients: &str,
    browser: &str,
) -> String {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create fake bin");
    let log = root.join("tmux.log");
    let _ = std::fs::remove_file(&log);
    script(
        &bin.join("tmux"),
        &format!(
            "printf '%s\\n' \"$*\" >>'{log}'\n\
             case \" $* \" in *\" display-message -p -t \"*)\n\
             printf '@1\\t/dev/pts/0\\n'; printf '{pane_clients}';;\n\
             *\" list-clients \"*) printf '{clients}';;\nesac\nexit 0\n",
            log = log.display()
        ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args([
            "tmux",
            "follow-click",
            "https://example.com/clicked",
            "",
            "",
            "0",
            "/repo",
            tty,
            "%3",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("TMUX", "/tmp/termnav-fake.sock,1,0")
        .env_remove("TMUX_PANE")
        .env("BROWSER", browser)
        .output()
        .expect("route browser target");
    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
    std::fs::read_to_string(log).unwrap_or_default()
}

#[test]
fn a_clicking_vscode_client_falls_back_to_browser_instead_of_another_client() {
    let root = temporary_root();
    std::fs::create_dir_all(&root).expect("create test root");
    let _cleanup = Cleanup(root.clone());
    let other = root.join("other.out");
    std::fs::write(&other, []).expect("create other client tty");
    let opened = root.join("opened");
    let browser = root.join("browser");
    script(
        &browser,
        &format!(
            "printf '%s %s\\n' \"$TMUX_PANE\" \"$1\" >'{}'\n",
            opened.display()
        ),
    );
    let clients = format!(
        "0\\tWezTerm 1\\t{}\\txterm-256color\\t0\\n0\\txterm.js(6.1.0)\\t/dev/pts/clicked\\txterm-256color\\t0\\n",
        other.display()
    );
    // The clicked pane's own choice would be the other client.
    let pane_clients = format!(
        "0\\t9\\t@1\\tWezTerm 1\\t{}\\txterm-256color\\t0\\n",
        other.display()
    );

    click_url(
        &root,
        "/dev/pts/clicked",
        &clients,
        &pane_clients,
        browser.to_str().expect("utf-8"),
    );

    assert!(std::fs::read(&other).expect("read other client").is_empty());
    assert_eq!(
        std::fs::read_to_string(&opened).expect("browser ran"),
        "%3 https://example.com/clicked\n"
    );
}

#[test]
fn a_client_gone_since_the_click_hands_over_to_the_clicked_pane() {
    let root = temporary_root();
    std::fs::create_dir_all(&root).expect("create test root");
    let _cleanup = Cleanup(root.clone());
    let pane_client = root.join("pane-client.out");
    std::fs::write(&pane_client, []).expect("create pane client tty");
    // The click lookup has no row for the clicked tty; the pane's own client
    // query has one.
    let pane_clients = format!(
        "0\\t1\\t@1\\tWezTerm 1\\t{}\\txterm-256color\\t0\\n",
        pane_client.display()
    );

    let calls = click_url(&root, "/dev/pts/gone", "", &pane_clients, "false");

    assert!(calls.contains("-t %3"), "calls: {calls}");
    let output = std::fs::read(&pane_client).expect("read pane client");
    assert!(output.starts_with(b"\x1b]1337;SetUserVar=TERMNAV_OPEN_URL="));
}

#[test]
fn a_click_without_a_tmux_server_never_reaches_one() {
    let root = temporary_root();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create fake bin");
    let _cleanup = Cleanup(root.clone());
    let log = root.join("tmux.log");
    script(
        &bin.join("tmux"),
        &format!("printf '%s\\n' \"$*\" >>'{}'\nexit 1\n", log.display()),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args([
            "tmux",
            "follow-click",
            "https://example.com/x",
            "",
            "",
            "0",
            "/repo",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("BROWSER")
        .output()
        .expect("route browser target");
    assert!(output.status.success());
    assert!(
        !log.exists(),
        "tmux ran: {:?}",
        std::fs::read_to_string(&log)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("https://example.com/x"));
}

#[test]
fn a_browser_that_keeps_running_is_left_running() {
    let root = temporary_root();
    std::fs::create_dir_all(&root).expect("create test root");
    let _cleanup = Cleanup(root.clone());
    let pid_file = root.join("browser.pid");
    let alive = root.join("alive");
    let browser = root.join("browser");
    // A browser started in the foreground lives as long as its window; this
    // one proves it outlived the grace period by marking a file later.
    script(
        &browser,
        &format!(
            "echo $$ >'{}'\nsleep 5\ntouch '{}'\nexec sleep 30\n",
            pid_file.display(),
            alive.display()
        ),
    );
    let clients = "0\\txterm.js(6.1.0)\\t/dev/pts/clicked\\txterm-256color\\t0\\n";
    let started = std::time::Instant::now();

    let calls = click_url(
        &root,
        "/dev/pts/clicked",
        clients,
        "",
        browser.to_str().expect("utf-8"),
    );

    // Far below the browser's own lifetime: termnav did not wait for it.
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !alive.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let survived = alive.exists();
    if let Some(pid) = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|pid| pid.trim().parse::<i32>().ok())
    {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(survived, "the browser was killed");
    assert!(!calls.contains("display-message"), "calls: {calls}");
}

#[test]
fn a_failing_browser_passes_the_url_on_and_then_tells_the_client() {
    let root = temporary_root();
    std::fs::create_dir_all(&root).expect("create test root");
    let _cleanup = Cleanup(root.clone());
    let opened = root.join("opened");
    let failing = root.join("failing");
    let recording = root.join("recording");
    script(&failing, "exit 1\n");
    script(
        &recording,
        &format!("printf '%s\\n' \"$1\" >'{}'\n", opened.display()),
    );
    let clients = "0\\txterm.js(6.1.0)\\t/dev/pts/clicked\\txterm-256color\\t0\\n";

    let calls = click_url(
        &root,
        "/dev/pts/clicked",
        clients,
        "",
        &format!("{}:{}", failing.display(), recording.display()),
    );
    assert_eq!(
        std::fs::read_to_string(&opened).expect("second entry ran"),
        "https://example.com/clicked\n"
    );
    assert!(!calls.contains("display-message -c"), "calls: {calls}");

    let calls = click_url(
        &root,
        "/dev/pts/clicked",
        clients,
        "",
        failing.to_str().expect("utf-8"),
    );
    assert!(
        calls.contains("display-message -c /dev/pts/clicked")
            && calls.contains("https://example.com/clicked"),
        "calls: {calls}"
    );
}

#[test]
fn the_browser_never_inherits_a_stale_pane() {
    let root = temporary_root();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create fake bin");
    let _cleanup = Cleanup(root.clone());
    let seen = root.join("seen");
    let browser = root.join("browser");
    script(
        &browser,
        &format!(
            "printf '%s\\n' \"${{TMUX_PANE-unset}}\" >'{}'\n",
            seen.display()
        ),
    );
    script(
        &bin.join("tmux"),
        "printf '0\\txterm.js(6.1.0)\\t/dev/pts/clicked\\txterm-256color\\t0\\n'\n",
    );
    // The click names its server only by socket, so this process has no tmux
    // context, yet its environment holds an outer server's pane.
    let status = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args([
            "tmux",
            "follow-click",
            "https://example.com/x",
            "",
            "",
            "0",
            "/repo",
            "/dev/pts/clicked",
            "%3",
            "0",
            "0",
            "0",
            "/tmp/termnav-fake.sock",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env_remove("TMUX")
        .env("TMUX_PANE", "%9")
        .env("BROWSER", &browser)
        .status()
        .expect("route browser target");
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(&seen).expect("browser ran"),
        "unset\n"
    );
}

/// Run a click that ends in the message, through a fake tmux that behaves as
/// every supported tmux does: `display-message -c` for a client it does not
/// know succeeds and shows nothing, unless `client_flag_fails` makes it fail
/// (an unreachable server). The clicked pane's session check answers `check`
/// (`COUNT`, empty, or `fail`), and only when it names pane `%3`.
/// Returns the tmux calls with their `TMUX_PANE`, and termnav's stderr.
fn tell_through(tty: &str, check: &str, client_flag_fails: bool) -> (String, String) {
    let root = temporary_root();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create fake bin");
    let _cleanup = Cleanup(root.clone());
    let log = root.join("tmux.log");
    let answer = if check == "fail" {
        "exit 1".to_owned()
    } else {
        format!("echo '{check}'")
    };
    let client_flag = if client_flag_fails {
        "exit 1"
    } else {
        "exit 0"
    };
    script(
        &bin.join("tmux"),
        &format!(
            "printf '%s|TMUX_PANE=%s\\n' \"$*\" \"${{TMUX_PANE-unset}}\" >>'{log}'\n\
             case \" $* \" in\n\
             *\" list-clients \"*) printf '0\\txterm.js(6.1.0)\\t/dev/pts/clicked\\txterm-256color\\t0\\n';;\n\
             *\" display-message -c \"*) {client_flag};;\n\
             *\" -t %3 \"*session_attached*) {answer};;\n\
             *session_attached*) exit 1;;\n\
             esac\nexit 0\n",
            log = log.display()
        ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args([
            "tmux",
            "follow-click",
            "https://example.com/told",
            "",
            "",
            "0",
            "/repo",
            tty,
            "%3",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("TMUX", "/tmp/termnav-fake.sock,1,0")
        // A server started inside another tmux pane holds that pane's id.
        .env("TMUX_PANE", "%9")
        .env_remove("BROWSER")
        .output()
        .expect("route browser target");
    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
    (
        std::fs::read_to_string(log).unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The message calls, as (targeted at a client, `TMUX_PANE`).
fn messages(calls: &str) -> Vec<(bool, &str)> {
    calls
        .lines()
        .filter(|line| {
            line.starts_with("display-message") && line.contains("https://example.com/told")
        })
        .map(|line| {
            (
                line.contains(" -c "),
                line.rsplit_once("|TMUX_PANE=").map_or("", |(_, pane)| pane),
            )
        })
        .collect()
}

#[test]
fn a_message_for_the_clicking_client_goes_only_to_that_client() {
    // tmux drops a message for a client that has gone, with status 0: nothing
    // else is tried, and no other session is asked about.
    let (calls, stderr) = tell_through("/dev/pts/clicked", "1", false);
    assert_eq!(messages(&calls), [(true, "unset")], "calls: {calls}");
    assert!(
        calls.contains("display-message -c /dev/pts/clicked -d 10000 "),
        "calls: {calls}"
    );
    assert!(!calls.contains("session_attached"), "calls: {calls}");
    assert!(stderr.is_empty(), "stderr: {stderr}");
}

#[test]
fn a_message_tmux_cannot_show_its_client_goes_to_stderr_only() {
    let (calls, stderr) = tell_through("/dev/pts/clicked", "1", true);
    assert_eq!(messages(&calls), [(true, "unset")], "calls: {calls}");
    assert!(
        stderr.contains("https://example.com/told"),
        "stderr: {stderr}"
    );
}

#[test]
fn a_message_for_a_click_without_a_client_goes_to_the_pane_session() {
    let (calls, stderr) = tell_through("", "1", false);
    assert_eq!(messages(&calls), [(false, "%3")], "calls: {calls}");
    assert!(stderr.is_empty(), "stderr: {stderr}");
}

#[test]
fn a_message_without_a_client_needs_the_pane_session_attached() {
    // The pane's session is detached; the check fails; the pane is gone,
    // which real tmux reports with no output and status 0.
    for check in ["0", "fail", ""] {
        let (calls, stderr) = tell_through("", check, false);
        assert!(messages(&calls).is_empty(), "{check}: {calls}");
        assert!(
            stderr.contains("https://example.com/told"),
            "{check}: {stderr}"
        );
    }
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
    script(
        &bin.join("tmux"),
        "if [ \"$1\" = list-panes ]; then printf '%s\\t%s\\n' '%2' nvim; exit 0; fi\nprintf '%s\\n' \"$*\" >>\"$TERMNAV_CLICK_LOG\"\n",
    );
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
