//! VS Code window focus publication.
//!
//! The companion extension owns one authenticated Unix socket per VS Code
//! window. Neovim publishes ordered leases to the window attached to its exact
//! tmux client, allowing multiple terminal windows to view one tmux server
//! without stealing each other's editor focus.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::process;

const TOKEN_LEN: usize = 64;
const MAX_ANCESTRY: usize = 64;
const HTTP_TIMEOUT: Duration = Duration::from_secs(1);

/// Focus ownership transition sent to the VS Code extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    /// Acquire or renew focus ownership.
    Claim,
    /// Relinquish focus ownership.
    Release,
}

/// Validated ordered update emitted by one Neovim instance.
#[derive(Debug)]
pub struct Update {
    /// Stable editor-source identifier.
    pub source: String,
    /// Editor lifecycle generation.
    pub cycle: u64,
    /// Monotonic update number within the generation.
    pub sequence: u64,
    /// Sender observation timestamp.
    pub observed: u64,
    /// Requested ownership transition.
    pub operation: Operation,
}

/// Outcome of publishing one update to all applicable VS Code windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    /// At least one authenticated window accepted the update.
    Posted,
    /// A candidate window existed but its socket request failed.
    Failed,
    /// No complete socket/token route was available.
    Unavailable,
}

impl Update {
    /// Build an update after validating JavaScript-safe counters and source.
    pub fn new(
        operation: Operation,
        source: &str,
        cycle: &str,
        sequence: &str,
        observed: &str,
    ) -> Result<Self, &'static str> {
        if source.is_empty()
            || source.len() > 128
            || !source
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
        {
            return Err("invalid source");
        }
        Ok(Self {
            source: source.to_owned(),
            cycle: js_uint(cycle).ok_or("invalid cycle")?,
            sequence: js_uint(sequence).ok_or("invalid sequence")?,
            observed: js_uint(observed).ok_or("invalid observation")?,
            operation,
        })
    }
}

/// Publish through direct environment state or exact tmux client routes.
#[must_use]
pub fn publish(update: &Update) -> PublishOutcome {
    if std::env::var_os("TMUX").is_some() && std::env::var_os("TMUX_PANE").is_some() {
        publish_tmux(update)
    } else {
        let route = Route::from_environment(unsafe { libc::getppid() as u32 });
        publish_routes(update, route)
    }
}

#[derive(Debug)]
struct Route {
    socket: PathBuf,
    token: String,
    client_pid: u32,
}

impl Route {
    fn from_environment(client_pid: u32) -> Option<Self> {
        let socket = std::env::var_os("TERMNAV_VSCODE_SOCKET")?;
        let token = std::env::var("TERMNAV_VSCODE_TOKEN").ok()?;
        valid_token(&token).then(|| Self {
            socket: PathBuf::from(socket),
            token,
            client_pid,
        })
    }

    fn from_process(client_pid: u32) -> Option<Self> {
        let socket = process::environment(client_pid, "TERMNAV_VSCODE_SOCKET")?;
        let token = process::environment(client_pid, "TERMNAV_VSCODE_TOKEN")?;
        valid_token(&token).then(|| Self {
            socket: PathBuf::from(socket),
            token,
            client_pid,
        })
    }
}

fn publish_tmux(update: &Update) -> PublishOutcome {
    let pane = std::env::var("TMUX_PANE").unwrap_or_default();
    let output = Command::new("tmux")
        .args([
            "list-clients",
            "-F",
            "#{client_pid}|#{pane_id}|#{client_flags}",
        ])
        .output();
    let Ok(output) = output else {
        return PublishOutcome::Unavailable;
    };
    let clients = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(ClientRow::parse)
        .collect::<Vec<_>>();
    let selected = clients.iter().filter(|client| match update.operation {
        Operation::Claim => client.pane == pane && client.focused,
        Operation::Release => true,
    });
    let outcome = publish_routes(
        update,
        selected.filter_map(|client| Route::from_process(client.pid)),
    );
    if update.operation == Operation::Claim && outcome != PublishOutcome::Posted {
        // A hidden pane must clear any window that may still hold its previous
        // lease. Broadcasting release is safe because the extension validates
        // source, generation, and ordering before changing ownership.
        let release = Update {
            source: update.source.clone(),
            cycle: update.cycle,
            sequence: update.sequence,
            observed: update.observed,
            operation: Operation::Release,
        };
        let release_outcome = publish_routes(
            &release,
            clients
                .iter()
                .filter_map(|client| Route::from_process(client.pid)),
        );
        return match (outcome, release_outcome) {
            (_, PublishOutcome::Posted) => PublishOutcome::Posted,
            (PublishOutcome::Failed, _) | (_, PublishOutcome::Failed) => PublishOutcome::Failed,
            _ => PublishOutcome::Unavailable,
        };
    }
    outcome
}

#[derive(Debug)]
struct ClientRow {
    pid: u32,
    pane: String,
    focused: bool,
}

impl ClientRow {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.splitn(3, '|');
        let pid = fields.next()?.parse().ok()?;
        let pane = fields.next()?.to_owned();
        let focused = fields.next()?.split(',').any(|flag| flag == "focused");
        Some(Self { pid, pane, focused })
    }
}

fn publish_routes(update: &Update, routes: impl IntoIterator<Item = Route>) -> PublishOutcome {
    let mut posted = false;
    let mut failed = false;
    for route in routes {
        let payload = Payload {
            version: 2,
            source: &update.source,
            cycle: update.cycle,
            sequence: update.sequence,
            observed: update.observed,
            operation: update.operation,
            token: &route.token,
            ancestors: ancestry(route.client_pid),
        };
        match post(&route.socket, &payload) {
            Ok(()) => posted = true,
            Err(_) => failed = true,
        }
    }
    if posted {
        PublishOutcome::Posted
    } else if failed {
        PublishOutcome::Failed
    } else {
        PublishOutcome::Unavailable
    }
}

#[derive(Serialize)]
struct Payload<'a> {
    version: u8,
    source: &'a str,
    cycle: u64,
    sequence: u64,
    observed: u64,
    operation: Operation,
    token: &'a str,
    ancestors: Vec<u32>,
}

fn ancestry(pid: u32) -> Vec<u32> {
    let mut current = pid;
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    while current > 0 && result.len() < MAX_ANCESTRY && seen.insert(current) {
        result.push(current);
        let Some(parent) = process::parent(current) else {
            break;
        };
        current = parent;
    }
    result
}

fn post(socket: &Path, payload: &Payload<'_>) -> io::Result<()> {
    let body = serde_json::to_vec(payload).map_err(io::Error::other)?;
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    write!(
        stream,
        "POST /nvim-focus HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;

    let mut response = [0_u8; 64];
    let length = stream.read(&mut response)?;
    let status = std::str::from_utf8(&response[..length])
        .ok()
        .and_then(|text| text.lines().next())
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok());
    if status.is_some_and(|status| (200..300).contains(&status)) {
        Ok(())
    } else {
        Err(io::Error::other("VS Code focus endpoint rejected update"))
    }
}

fn valid_token(token: &str) -> bool {
    token.len() == TOKEN_LEN
        && token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn js_uint(value: &str) -> Option<u64> {
    let parsed = value.parse::<u64>().ok()?;
    (parsed < 9_007_199_254_740_992).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::{ClientRow, Operation, Update, js_uint, valid_token};

    #[test]
    fn input_contract_matches_the_extension_number_model() {
        assert_eq!(js_uint("9007199254740991"), Some(9_007_199_254_740_991));
        assert_eq!(js_uint("9007199254740992"), None);
        assert!(valid_token(&"a".repeat(64)));
        assert!(!valid_token(&"A".repeat(64)));
        assert!(Update::new(Operation::Claim, "nvim:1", "1", "2", "3").is_ok());
        assert!(Update::new(Operation::Claim, "bad source", "1", "2", "3").is_err());
    }

    #[test]
    fn tmux_client_rows_preserve_exact_pane_and_focus_state() {
        let row = ClientRow::parse("123|%7|attached,focused").expect("valid client row");
        assert_eq!(row.pid, 123);
        assert_eq!(row.pane, "%7");
        assert!(row.focused);
    }
}
