//! Remote-host context shared by hyperlink-producing commands.

use std::process::Command;
use std::time::Duration;

use crate::process;

/// Return the remote host represented by the current terminal context.
#[must_use]
pub fn remote_host() -> Option<String> {
    if let Ok(value) = std::env::var("TERMNAV_REMOTE_LINK_HOST")
        && !value.is_empty()
    {
        return Some(value);
    }

    if std::env::var_os("TMUX").is_some() {
        let mut command = Command::new("tmux");
        command.args(["show-environment", "-g", "TERMNAV_REMOTE_LINK_HOST"]);
        if let Ok(output) = process::output_timeout(&mut command, Duration::from_secs(2))
            && output.status.success()
        {
            let line = String::from_utf8_lossy(&output.stdout);
            if let Some(value) = line.trim_end().strip_prefix("TERMNAV_REMOTE_LINK_HOST=")
                && !value.is_empty()
            {
                return Some(value.to_owned());
            }
        }
    }

    if std::env::var_os("SSH_CONNECTION").is_some() {
        for arguments in [["-s"].as_slice(), [].as_slice()] {
            let mut command = Command::new("hostname");
            command.args(arguments);
            if let Ok(output) = process::output_timeout(&mut command, Duration::from_secs(2))
                && output.status.success()
            {
                let host = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if !host.is_empty() {
                    return Some(host);
                }
            }
        }
    }
    None
}

/// Return the stable token required by ripgrep's hyperlink template.
#[must_use]
pub fn host() -> String {
    remote_host().unwrap_or_else(|| "localhost".to_owned())
}
