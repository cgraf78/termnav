use std::process::Command;

fn termnav() -> Command {
    Command::new(env!("CARGO_BIN_EXE_termnav"))
}

#[test]
fn version_is_traceable() {
    let output = termnav().arg("version").output().expect("run termnav");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("version is utf-8");
    let fields = stdout.trim().split('-').collect::<Vec<_>>();
    assert!(stdout.starts_with("termnav "));
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[2].len(), 8);
}

#[test]
fn top_level_help_lists_the_cohesive_surface() {
    let output = termnav().arg("--help").output().expect("run termnav");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help is utf-8");
    for command in [
        "navigate",
        "relay",
        "ssh",
        "link-host",
        "tmux",
        "nvim",
        "vscode",
        "eza",
    ] {
        assert!(
            stdout.contains(command),
            "missing {command:?} in {stdout:?}"
        );
    }
}

#[test]
fn unknown_command_is_a_usage_error() {
    let output = termnav()
        .arg("definitely-not-a-command")
        .output()
        .expect("run termnav");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

#[test]
fn pane_navigation_without_tmux_is_an_owned_terminal_noop() {
    let output = termnav()
        .args(["navigate", "pane-select", "left"])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("run termnav");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn parent_navigation_requires_exact_client_identity() {
    let output = termnav()
        .args(["navigate", "pane-select", "left", "--parent"])
        .output()
        .expect("run termnav");

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--client-pid"));
}

#[test]
fn relay_send_declines_without_an_advertised_parent() {
    let output = termnav()
        .args(["relay", "send", "pane-select", "left"])
        .env_remove("TERMNAV_PARENT_RELAY")
        .output()
        .expect("run termnav");

    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn malformed_relay_arguments_are_usage_errors() {
    for arguments in [
        &[
            "relay",
            "send",
            "--client-pid",
            "nope",
            "pane-select",
            "left",
        ][..],
        &["relay", "commit", "--tmux-socket"][..],
    ] {
        let output = termnav().args(arguments).output().expect("run termnav");

        assert_eq!(output.status.code(), Some(2), "arguments: {arguments:?}");
        assert!(output.stdout.is_empty(), "arguments: {arguments:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("usage: termnav relay"),
            "arguments: {arguments:?}"
        );
    }
}

#[test]
fn legacy_navigation_action_names_are_not_public_commands() {
    for arguments in [
        &["navigate", "pane", "left"][..],
        &["navigate", "window", "next"][..],
        &["navigate", "move-window", "right"][..],
        &["relay", "send", "pane", "left"][..],
        &["relay", "send", "window", "next"][..],
        &["relay", "send", "move", "right"][..],
    ] {
        let output = termnav()
            .args(arguments)
            .output()
            .expect("reject legacy navigation action");

        assert_eq!(output.status.code(), Some(2), "arguments: {arguments:?}");
        assert!(output.stdout.is_empty(), "arguments: {arguments:?}");
    }
}

#[test]
fn navigation_continuations_round_trip_and_fail_closed() {
    let initial = termnav()
        .args(["navigate", "--emit-continuation", "pane-select", "left"])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("run initial navigation");
    assert!(initial.status.success());
    let continuation = String::from_utf8(initial.stdout)
        .expect("continuation is utf-8")
        .trim()
        .to_owned();
    let parsed: serde_json::Value =
        serde_json::from_str(&continuation).expect("continuation is JSON");
    assert_eq!(parsed["version"], 1);

    let resumed = termnav()
        .args([
            "navigate",
            "--emit-continuation",
            "pane-select",
            "right",
            "--continuation",
            &continuation,
        ])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("resume navigation");
    assert!(resumed.status.success());
    serde_json::from_slice::<serde_json::Value>(&resumed.stdout)
        .expect("resumed continuation is JSON");

    let expired = r#"{"version":1,"expires_at_monotonic_ms":0,"client":null,"scope":null}"#;
    let refreshed = termnav()
        .args([
            "navigate",
            "--emit-continuation",
            "pane-select",
            "left",
            "--continuation",
            expired,
        ])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("refresh expired continuation");
    assert!(refreshed.status.success());

    for arguments in [
        vec![
            "navigate",
            "pane-select",
            "left",
            "--continuation",
            "not-json",
        ],
        vec![
            "navigate",
            "pane-select",
            "left",
            "--parent",
            "--continuation",
            expired,
        ],
        vec![
            "navigate",
            "pane-select",
            "left",
            "--client-pid",
            "123",
            "--client-tty",
            "/dev/pts/1",
            "--client-created",
            "456",
            "--client-termtype",
            "tmux-256color",
            "--source-socket",
            "/tmp/tmux.sock",
            "--source-pane",
            "%1",
            "--continuation",
            expired,
        ],
    ] {
        let output = termnav().args(arguments).output().expect("reject state");
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("termnav navigate"));
    }
}

#[test]
fn link_host_uses_the_explicit_remote_context() {
    let output = termnav()
        .arg("link-host")
        .env("TERMNAV_REMOTE_LINK_HOST", "remote.example.invalid")
        .output()
        .expect("run link host");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"remote.example.invalid\n");
}

#[test]
fn link_host_is_not_part_of_the_neovim_namespace() {
    let output = termnav()
        .args(["nvim", "link-host"])
        .output()
        .expect("reject legacy link host namespace");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("termnav nvim"));
}
