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
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create fake bin");
    let _cleanup = Cleanup(root.clone());
    let tty = root.join("client.out");
    let other = root.join("other.out");
    std::fs::write(&tty, []).expect("create fake client tty");
    std::fs::write(&other, []).expect("create fake other client tty");
    // Two WezTerm clients; the other one is listed first. The fake tmux is
    // the only server this test can reach.
    let tmux = bin.join("tmux");
    std::fs::write(
        &tmux,
        format!(
            "#!/bin/sh\ncase \" $* \" in *\" list-clients \"*)\n\
             printf '0\\tWezTerm 1\\t%s\\txterm-256color\\t0\\n' '{}'\n\
             printf '0\\tWezTerm 1\\t%s\\txterm-256color\\t0\\n' '{}'\n\
             exit 0;;\nesac\nexit 1\n",
            other.display(),
            tty.display()
        ),
    )
    .expect("write fake tmux");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755))
        .expect("make fake tmux executable");

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
fn script(path: &std::path::Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make script executable");
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
