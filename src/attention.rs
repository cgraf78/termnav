//! Bounded attention snapshots forwarded through existing authenticated routes.

use crate::focus::Parent;
use crate::navigation::{Backend, SystemBackend};
use crate::process;
use crate::relay::{client::send, protocol};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parent-pane contribution, expired using the receiving host's clock.
pub const CHILD_OPTION: &str = "@termnav_child_attention";
/// Terminal-facing snapshot understood by the WezTerm adapter.
pub const USER_VAR: &str = "TERM_PANE_STATE";
const LEASE_SECONDS: u64 = 15;

/// Independent signals: pending attention must not hide concurrent work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct State {
    /// Number of working activities.
    pub working: u64,
    /// Number of activities awaiting attention.
    pub pending: u64,
    /// Number of pending failures.
    pub failed: u64,
}

impl State {
    /// Reject malformed or unbounded metadata before it reaches a terminal.
    #[must_use]
    pub fn parse(request: &Value) -> Option<Self> {
        let state = Self {
            working: request.get("working")?.as_u64()?,
            pending: request.get("pending")?.as_u64()?,
            failed: request.get("failed")?.as_u64()?,
        };
        (state.working <= 100_000 && state.pending <= 100_000 && state.failed <= state.pending)
            .then_some(state)
    }
}

fn tmux(socket: &str, args: &[&str]) -> Option<std::process::Output> {
    let mut command = Command::new("tmux");
    command.args(["-S", socket]).args(args).env_remove("TMUX");
    process::output_timeout(&mut command, Duration::from_secs(2)).ok()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Confirm that a live process still owns the terminal's foreground group.
pub(crate) fn owner(pid: u32, tty: &str) -> bool {
    if !process::tty_matches(pid, tty) {
        return false;
    }
    let mut command = Command::new("ps");
    command.args(["-p", &pid.to_string(), "-o", "pgid=", "-o", "tpgid="]);
    let Some(output) = process::output_timeout(&mut command, Duration::from_secs(2))
        .ok()
        .filter(|output| output.status.success())
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let fields = text.split_whitespace().collect::<Vec<_>>();
    fields.len() == 2 && fields[0] == fields[1] && fields[0] != "0"
}

fn claim(socket: &str, pane: &str, pid: u32, state: State) -> bool {
    let Some(output) = tmux(
        socket,
        &["display-message", "-p", "-t", pane, "#{pane_tty}"],
    )
    .filter(|output| output.status.success()) else {
        return false;
    };
    let tty = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !owner(pid, &tty) {
        return false;
    }
    let value = format!(
        "{}:{}:{}:{}",
        now() + LEASE_SECONDS,
        state.working,
        state.pending,
        state.failed
    );
    tmux(
        socket,
        &["set-option", "-p", "-t", pane, CHILD_OPTION, &value],
    )
    .is_some_and(|output| output.status.success())
}

fn request(state: State) -> Value {
    json!({"v":protocol::VERSION,"op":protocol::operation::ATTENTION,
        "working":state.working,"pending":state.pending,"failed":state.failed})
}

fn forward(parent: Parent, pid: u32, state: State) -> bool {
    match parent {
        Parent::Tmux { socket, pane } => claim(&socket, &pane, pid, state),
        Parent::Relay(path) => send(Path::new(&path), &request(state), Duration::from_secs(2))
            .ok()
            .is_some_and(|reply| {
                reply.get("result").and_then(Value::as_str) == Some(protocol::result::EMITTED)
            }),
    }
}

/// Publish for one exact live client; decline only when no parent route exists.
///
/// The caller may write to the validated terminal itself on status 3. Errors
/// on an existing parent route must never fall through to guessed passthrough.
pub fn publish(socket: &str, name: &str, pid: u32, created: &str, state: State) -> i32 {
    let Some(output) = tmux(
        socket,
        &[
            "list-clients",
            "-F",
            "#{client_pid} #{client_created} #{client_control_mode} #{client_tty} #{client_name}",
        ],
    )
    .filter(|output| output.status.success()) else {
        return 1;
    };
    let listing = String::from_utf8_lossy(&output.stdout);
    let exact = listing.lines().any(|line| {
        let fields = line.splitn(5, ' ').collect::<Vec<_>>();
        fields.len() == 5
            && fields[0] == pid.to_string()
            && fields[1] == created
            && fields[2] == "0"
            && fields[4] == name
            && owner(pid, fields[3])
    });
    if !exact {
        return 1;
    }
    let Some(parent) = crate::focus::parent_for_client(pid, socket) else {
        return 3;
    };
    if forward(parent, pid, state) { 0 } else { 1 }
}

/// Route an admitted SSH snapshot through its immediate owning scope.
pub(crate) fn relay(
    request: &Value,
    terminal: Option<&crate::relay::server::DirectTerminal>,
) -> Value {
    let Some(state) = State::parse(request) else {
        return json!({"result":protocol::result::ERROR});
    };
    let mut backend = SystemBackend::from_current_environment();
    if let Some(scope) = backend.current_scope() {
        return json!({"result": if claim(&scope.socket, &scope.pane, std::process::id(), state) {
            protocol::result::EMITTED
        } else {protocol::result::ERROR}});
    }
    // A broken inherited scope is not evidence that this is a direct terminal.
    // Match focus routing: never retarget a malformed tmux request to another
    // connection merely because that connection also exists in the environment.
    if std::env::var_os("TMUX").is_some() || std::env::var_os("TMUX_PANE").is_some() {
        return json!({"result":protocol::result::ERROR});
    }
    if let Ok(parent) = std::env::var("TERMNAV_PARENT_RELAY") {
        return send(Path::new(&parent), request, Duration::from_secs(2))
            .unwrap_or_else(|_| json!({"result":protocol::result::ERROR}));
    }
    let Some(terminal) = terminal else {
        return json!({"result":protocol::result::DECLINED});
    };
    let payload = json!({"v":1,"working":state.working,"pending":state.pending,
        "failed":state.failed,"heartbeat":now()})
    .to_string();
    let emitted = terminal.emit_attention(&payload);
    json!({"result": if emitted {protocol::result::EMITTED} else {protocol::result::ERROR}})
}

/// Validate the thin tmux CLI adapter without shell evaluation.
pub fn command(args: &[String], stderr: &mut dyn Write) -> std::io::Result<i32> {
    let mut fields = std::collections::HashMap::new();
    for pair in args.chunks(2) {
        if pair.len() != 2
            || fields.insert(pair[0].as_str(), pair[1].as_str()).is_some()
            || !matches!(
                pair[0].as_str(),
                "--tmux-socket"
                    | "--client-name"
                    | "--client-pid"
                    | "--client-created"
                    | "--working"
                    | "--pending"
                    | "--failed"
            )
        {
            writeln!(stderr, "termnav tmux attention: invalid options")?;
            return Ok(2);
        }
    }
    let get = |name: &str| fields.get(name).copied();
    let values = (
        get("--tmux-socket"),
        get("--client-name"),
        get("--client-pid"),
        get("--client-created"),
    );
    let state = State::parse(
        &json!({"working":get("--working").and_then(|v| v.parse::<u64>().ok()),
        "pending":get("--pending").and_then(|v| v.parse::<u64>().ok()),
        "failed":get("--failed").and_then(|v| v.parse::<u64>().ok())}),
    );
    if let (Some(socket), Some(name), Some(pid), Some(created)) = values
        && Path::new(socket).is_absolute()
        && !name.is_empty()
        && let Ok(pid) = pid.parse::<u32>()
        && pid > 0
        && created.parse::<u64>().is_ok()
        && let Some(state) = state
    {
        return Ok(publish(socket, name, pid, created, state));
    }
    writeln!(
        stderr,
        "termnav tmux attention: exact client and bounded counts are required"
    )?;
    Ok(2)
}

#[cfg(test)]
mod tests {
    use super::State;
    use serde_json::json;

    #[test]
    fn independent_work_and_attention_survive_the_protocol() {
        assert_eq!(
            State::parse(&json!({"working":2,"pending":1,"failed":1})),
            Some(State {
                working: 2,
                pending: 1,
                failed: 1
            })
        );
    }

    #[test]
    fn rejects_negative_huge_and_inconsistent_counts() {
        for value in [
            json!({"working":-1,"pending":1,"failed":0}),
            json!({"working":100001,"pending":1,"failed":0}),
            json!({"working":0,"pending":0,"failed":1}),
            json!({"working":0,"pending":"1","failed":0}),
        ] {
            assert!(State::parse(&value).is_none());
        }
    }
}
