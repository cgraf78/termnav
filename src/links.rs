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

/// Decode percent escapes at an explicitly encoded URI boundary.
pub(crate) fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
