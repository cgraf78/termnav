//! Remote Neovim reuse through an already-authenticated ControlMaster.

use std::ffi::OsString;
use std::io;
use std::process::{Command, Stdio};

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
    let binary = crate::ssh::real_ssh()?;
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
    let status = Command::new(binary)
        .args(mux_only_options(control_path))
        .arg(host)
        .arg(remote_command)
        .stdin(Stdio::null())
        .status()?;
    Ok(if status.success() { 0 } else { 12 })
}

/// Build the remote shell command used by SSH and tmux typed-command fallback.
///
/// The additive release retains the old opener as a rollout fallback because
/// local and remote hosts update independently. The cleanup release removes
/// that branch only after every supported host has the unified CLI.
#[must_use]
pub fn remote_nvim_command(arguments: &[&str]) -> String {
    let mut command = String::from(
        r#"PATH="$PATH:$HOME/.local/bin:$HOME/.local/share/mise/shims:/opt/homebrew/bin:/usr/local/bin"; export PATH; if command -v termnav >/dev/null 2>&1; then termnav nvim open"#,
    );
    for argument in arguments {
        command.push(' ');
        command.push_str(&shell_quote(argument));
    }
    command.push_str(r#"; elif command -v nvim-tmux-open >/dev/null 2>&1; then nvim-tmux-open"#);
    for argument in arguments {
        command.push(' ');
        command.push_str(&shell_quote(argument));
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{remote_nvim_command, shell_quote, valid_host};

    #[test]
    fn remote_command_quotes_shell_metacharacters() {
        let command = remote_nvim_command(&["link", "/tmp/a b's.txt:4", "", "terminal"]);

        assert!(command.contains("'/tmp/a b'\\''s.txt:4'"));
        assert!(command.contains("termnav nvim open"));
        assert!(command.contains("nvim-tmux-open"));
    }

    #[test]
    fn host_validation_rejects_option_and_path_injection() {
        assert!(valid_host("dev1.example"));
        assert!(!valid_host("-oProxyCommand=bad"));
        assert!(!valid_host("../host"));
    }

    #[test]
    fn shell_quote_handles_empty_values() {
        assert_eq!(shell_quote(""), "''");
    }
}
