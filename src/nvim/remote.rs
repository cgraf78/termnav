//! Remote Neovim reuse through an already-authenticated ControlMaster.

use std::ffi::OsString;
use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

const OPEN_TIMEOUT: Duration = Duration::from_secs(3);

/// Open a target remotely without permission to establish a new connection.
///
/// The command performs one non-connecting `ssh -G` configuration query and
/// then one ordinary session request with an exact `ControlPath`. OpenSSH would
/// normally fall back to a fresh transport if the master vanished. An explicit
/// local-failure `ProxyCommand` closes that race structurally: mux success runs
/// the command, while mux failure can reach neither the destination nor Duo.
pub fn ssh_open(host: &str, target: &str) -> io::Result<i32> {
    if !valid_host(host) {
        return Ok(11);
    }
    if !allowed_host(host) {
        return Ok(13);
    }
    let binary = match crate::ssh::real_ssh() {
        Ok(binary) => binary,
        // No process has started yet, so this failure is definitive: it is
        // safe for the link router to try the explicitly configured transport
        // without risking a duplicate remote open. Once ssh has started,
        // status_timeout errors remain indeterminate and are not collapsed
        // into this fallback code.
        Err(_) => return Ok(10),
    };
    let arguments = vec![OsString::from(host)];
    let settings = match crate::ssh::effective_config(&binary, &arguments) {
        Ok(settings) => settings,
        Err(_) => return Ok(10),
    };
    let Some(control_path) = settings
        .get("controlpath")
        .filter(|value| !value.eq_ignore_ascii_case("none"))
    else {
        return Ok(10);
    };

    let remote_command = remote_nvim_command(&["link", target, "", "terminal"]);
    let mut command = Command::new(binary);
    command
        .args(mux_only_options(control_path))
        .arg(host)
        .arg(remote_command)
        .stdin(Stdio::null());
    let status = crate::process::status_timeout(&mut command, OPEN_TIMEOUT)?;
    Ok(if status.success() { 0 } else { 12 })
}

/// Ask an explicitly configured transport to open a target in one exact pane.
///
/// There is no generic, acknowledged way to enter an arbitrary remote tmux
/// command prompt through a terminal byte stream. Consumers that own another
/// transport may opt in with an executable; its exit status is the acknowledgement.
/// Without that capability Termnav fails closed instead of guessing a pane or
/// injecting keystrokes into an unknown foreground application.
pub fn configured_open(
    kind: &str,
    scope: &str,
    pane: &str,
    host: &str,
    target: &str,
) -> io::Result<i32> {
    let valid_identity = match kind {
        "tmux" => !scope.is_empty() && !pane.is_empty(),
        // A pane number is only unique inside one WezTerm mux server. Require
        // its opaque socket/instance scope so helpers never guess when several
        // GUI classes or mux servers reuse the same pane ID.
        "wezterm" => !scope.is_empty() && !pane.is_empty(),
        _ => false,
    };
    if !valid_host(host) || !valid_identity {
        return Ok(12);
    }
    let Some(helper) = std::env::var_os("TERMNAV_REMOTE_OPEN_HELPER") else {
        return Ok(12);
    };
    if helper.is_empty() {
        return Ok(12);
    }
    let mut command = Command::new(helper);
    command
        .args([kind, scope, pane, host, target])
        .stdin(Stdio::null());
    let status = crate::process::status_timeout(&mut command, OPEN_TIMEOUT)?;
    Ok(if status.success() { 0 } else { 12 })
}

/// Build the remote shell command used by SSH and tmux typed-command fallback.
///
/// Remote hosts may update independently, but the versioned relay protocol is
/// the compatibility boundary. A host without the unified binary fails
/// visibly instead of silently invoking a second implementation with different
/// routing or authentication behavior.
#[must_use]
pub fn remote_nvim_command(arguments: &[&str]) -> String {
    let mut command = String::from(
        r#"PATH="$PATH:$HOME/.local/bin:$HOME/.local/share/mise/shims:/opt/homebrew/bin:/usr/local/bin"; export PATH; if command -v termnav >/dev/null 2>&1; then termnav nvim open"#,
    );
    for argument in arguments {
        command.push(' ');
        command.push_str(&crate::shell::quote(argument));
    }
    command.push_str(
        r#"; else printf "%s\n" "termnav: remote Neovim opener not found on PATH" >&2; exit 127; fi"#,
    );
    command
}

fn mux_only_options(control_path: &str) -> Vec<OsString> {
    [
        "-S",
        control_path,
        "-o",
        "ControlMaster=no",
        "-o",
        "CanonicalizeHostname=no",
        "-o",
        "ProxyJump=none",
        "-o",
        "ProxyCommand=/bin/false",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "ForwardX11Trusted=no",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=1",
        "-o",
        "ConnectionAttempts=1",
        "-o",
        "NumberOfPasswordPrompts=0",
        "-o",
        "PasswordAuthentication=no",
        "-o",
        "KbdInteractiveAuthentication=no",
        "-o",
        "PubkeyAuthentication=no",
        "-o",
        "GSSAPIAuthentication=no",
        "-o",
        "HostbasedAuthentication=no",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('.')
        && !host.contains("..")
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn allowed_host(host: &str) -> bool {
    std::env::var("TERMNAV_SSH_CONTROL_HOSTS")
        .ok()
        .is_some_and(|allowed| {
            allowed
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate == host)
        })
}

#[cfg(test)]
mod tests {
    use super::{remote_nvim_command, valid_host};

    #[test]
    fn remote_command_quotes_shell_metacharacters() {
        let command = remote_nvim_command(&["link", "/tmp/a b's.txt:4", "", "terminal"]);

        assert!(command.contains("'/tmp/a b'\\''s.txt:4'"));
        assert!(command.contains("termnav nvim open"));
        assert!(!command.contains("nvim-tmux-open"));
    }

    #[test]
    fn host_validation_rejects_option_and_path_injection() {
        assert!(valid_host("dev1.example"));
        assert!(!valid_host("-oProxyCommand=bad"));
        assert!(!valid_host("../host"));
    }
}
