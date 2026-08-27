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
    for command in ["navigate", "relay", "ssh", "tmux", "nvim", "vscode", "eza"] {
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
        .args(["relay", "send", "pane", "left"])
        .env_remove("TERMNAV_PARENT_RELAY")
        .output()
        .expect("run termnav");

    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}
