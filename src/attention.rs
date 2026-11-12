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
///
/// The value stays `expiry:working:pending:failed`; older readers discard
/// leases with any other shape, so new counts travel in separate options.
pub const CHILD_OPTION: &str = "@termnav_child_attention";
/// Waiting subset of the parent-pane lease as `expiry:waiting`.
///
/// Written and removed in the same tmux command as [`CHILD_OPTION`] with the
/// same receiving-host expiry, and absent when nothing is waiting.
pub const CHILD_WAITING_OPTION: &str = "@termnav_child_waiting";
/// Terminal-facing snapshot understood by the WezTerm adapter.
pub const USER_VAR: &str = "TERM_PANE_STATE";
const LEASE_SECONDS: u64 = 15;

/// Per-pane effective-state counts chosen by the publisher.
///
/// Each pane is counted once, under its most urgent state: tmux-tools'
/// `term-pane-state` counts red (failed) panes in `failed`, yellow (waiting)
/// panes in `waiting`, and every attention pane in `pending`; only panes
/// without attention count as `working`. Consumers derive the remaining
/// pending panes as `pending - failed - waiting`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct State {
    /// Number of working panes without attention.
    pub working: u64,
    /// Number of panes awaiting attention, including failed and waiting ones.
    pub pending: u64,
    /// Number of pending panes whose attention is a failure.
    pub failed: u64,
    /// Number of pending panes waiting for an answer; absent in older peers.
    #[serde(default)]
    pub waiting: u64,
}

impl State {
    /// Reject malformed or unbounded metadata before it reaches a terminal.
    ///
    /// A missing `waiting` field is an older publisher and means zero; a
    /// present but malformed one is rejected like any other count.
    #[must_use]
    pub fn parse(request: &Value) -> Option<Self> {
        let state = Self {
            working: request.get("working")?.as_u64()?,
            pending: request.get("pending")?.as_u64()?,
            failed: request.get("failed")?.as_u64()?,
            waiting: match request.get("waiting") {
                None => 0,
                Some(value) => value.as_u64()?,
            },
        };
        (state.working <= 100_000
            && state.pending <= 100_000
            && state.failed.checked_add(state.waiting)? <= state.pending)
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
    foreground_owner(pid, tty)
}

/// Read the controlling device only when the kernel reports foreground ownership.
///
/// Linux permits spaces and parentheses inside comm; only the last closing
/// parenthesis separates it from the fixed-position numeric fields.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn foreground_device(stat: &str, pid: u32) -> Option<u32> {
    let (identity, fields) = stat.rsplit_once(')')?;
    let (actual, _) = identity.split_once(" (")?;
    if actual.parse::<u32>().ok()? != pid {
        return None;
    }
    let fields = fields.split_whitespace().take(6).collect::<Vec<_>>();
    if fields.len() != 6 {
        return None;
    }
    let group = fields[2].parse::<i32>().ok()?;
    let foreground = fields[5].parse::<i32>().ok()?;
    let device = fields[4].parse::<i32>().ok()?;
    (group > 0 && group == foreground && device != 0)
        .then_some(u32::from_ne_bytes(device.to_ne_bytes()))
}

/// Use procfs on minimal Linux installations without requiring procps or GNU ps.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn foreground_owner(pid: u32, tty: &str) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let Some(device) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| foreground_device(&stat, pid))
    else {
        return false;
    };
    let Ok(metadata) = std::fs::metadata(tty) else {
        return false;
    };
    // tty_nr uses Linux's encoded 32-bit dev_t layout, while stat.rdev uses
    // libc's dev_t. Compare the decoded device pair rather than raw integers.
    metadata.file_type().is_char_device()
        && i64::from((device >> 8) & 0xfff) == i64::from(libc::major(metadata.rdev()))
        && i64::from((device & 0xff) | ((device >> 12) & 0xfff00))
            == i64::from(libc::minor(metadata.rdev()))
}

/// macOS exposes controlling foreground ownership through the system ps.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn foreground_owner(pid: u32, _tty: &str) -> bool {
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
    let args = lease(pane, now() + LEASE_SECONDS, state);
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    tmux(socket, &args).is_some_and(|output| output.status.success())
}

/// One tmux command list that replaces both lease options together.
///
/// The main lease keeps its exact legacy shape. tmux stops a command list at
/// the first error, so a missing pane cannot leave a waiting lease behind.
fn lease(pane: &str, expiry: u64, state: State) -> Vec<String> {
    let main = format!(
        "{expiry}:{}:{}:{}",
        state.working, state.pending, state.failed
    );
    let waiting = format!("{expiry}:{}", state.waiting);
    let mut args = vec!["set-option", "-p", "-t", pane, CHILD_OPTION, &main, ";"];
    if state.waiting == 0 {
        args.extend(["set-option", "-pu", "-t", pane, CHILD_WAITING_OPTION]);
    } else {
        args.extend([
            "set-option",
            "-p",
            "-t",
            pane,
            CHILD_WAITING_OPTION,
            &waiting,
        ]);
    }
    args.into_iter().map(str::to_owned).collect()
}

fn request(state: State) -> Value {
    json!({"v":protocol::VERSION,"op":protocol::operation::ATTENTION,
        "working":state.working,"pending":state.pending,"failed":state.failed,
        "waiting":state.waiting})
}

/// Recover any prior partial advisory escape at each immediate parser boundary.
pub(crate) fn frame(payload: &str, termtype: &str) -> Vec<u8> {
    let raw = crate::terminal::user_var(USER_VAR, payload, crate::terminal::TmuxMode::Raw);
    let mut frame = vec![0x18]; // CAN affects terminal output parsing, never input.
    if crate::terminal::tmux_mode(termtype) == crate::terminal::TmuxMode::Passthrough {
        frame.extend_from_slice(b"\x1bPtmux;\x18");
        for byte in raw {
            frame.push(byte);
            if byte == 0x1b {
                frame.push(byte);
            }
        }
        frame.extend_from_slice(b"\x1b\\");
    } else {
        frame.extend_from_slice(&raw);
    }
    frame
}

fn payload(state: State) -> String {
    json!({"v":1,"working":state.working,"pending":state.pending,
        "failed":state.failed,"waiting":state.waiting,"heartbeat":now()})
    .to_string()
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

fn client(socket: &str, name: &str, pid: u32, created: &str) -> Option<(String, String)> {
    let output = tmux(
        socket,
        &[
            "list-clients",
            "-F",
            "#{client_pid} #{client_created} #{client_control_mode} #{client_tty} #{client_termname} #{client_name}",
        ],
    )
    .filter(|output| output.status.success())?;
    let listing = String::from_utf8_lossy(&output.stdout);
    listing.lines().find_map(|line| {
        let fields = line.splitn(6, ' ').collect::<Vec<_>>();
        (fields.len() == 6
            && fields[0] == pid.to_string()
            && fields[1] == created
            && fields[2] == "0"
            && fields[5] == name
            && fields[3].starts_with("/dev/")
            && owner(pid, fields[3]))
        .then(|| (fields[3].to_owned(), fields[4].to_owned()))
    })
}

/// Publish for one exact foreground client, including its direct terminal.
///
/// No exit status grants callers permission to implement a tty-write fallback.
pub fn publish(socket: &str, name: &str, pid: u32, created: &str, state: State) -> i32 {
    let Some(identity) = client(socket, name, pid, created) else {
        return 1;
    };
    if let Some(parent) = crate::focus::parent_for_client(pid, socket) {
        return if forward(parent, pid, state) { 0 } else { 1 };
    }
    // Present but unusable ancestry is a failed route, not evidence that this
    // is an outer terminal. Match relay admission instead of escaping a hidden
    // or malformed parent pane with raw output.
    if ["TMUX", "TMUX_PANE", "TERMNAV_PARENT_RELAY"]
        .iter()
        .any(|name| process::environment(pid, name).is_some())
    {
        return 1;
    }
    let path = Path::new(&identity.0);
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 1;
    };
    let frame = frame(&payload(state), &identity.1);
    if crate::terminal::write_attention(path, &metadata, &frame, || {
        client(socket, name, pid, created).as_ref() == Some(&identity)
    })
    .is_ok()
    {
        0
    } else {
        1
    }
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
    let emitted = terminal.emit_attention(&payload(state));
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
                    | "--waiting"
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
    let count = |name: &str| get(name).and_then(|v| v.parse::<u64>().ok());
    let mut counts = json!({"working":count("--working"),"pending":count("--pending"),
        "failed":count("--failed")});
    // Omitted means zero; a present but unparsable value stays null and fails.
    if get("--waiting").is_some() {
        counts["waiting"] = json!(count("--waiting"));
    }
    let state = State::parse(&counts);
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
    use super::{
        CHILD_OPTION, CHILD_WAITING_OPTION, State, command, foreground_device, lease, payload,
        request,
    };
    use serde_json::{Value, json};

    fn state(working: u64, pending: u64, failed: u64, waiting: u64) -> State {
        State {
            working,
            pending,
            failed,
            waiting,
        }
    }

    #[test]
    fn effective_state_counts_survive_the_protocol() {
        assert_eq!(
            State::parse(&json!({"working":2,"pending":3,"failed":1,"waiting":2})),
            Some(state(2, 3, 1, 2))
        );
    }

    #[test]
    fn older_publishers_without_waiting_mean_zero() {
        let old = json!({"v":2,"op":"attention","working":2,"pending":1,"failed":1});
        assert_eq!(State::parse(&old), Some(state(2, 1, 1, 0)));
        let snapshot: State =
            serde_json::from_value(json!({"working":2,"pending":1,"failed":1})).unwrap();
        assert_eq!(snapshot, state(2, 1, 1, 0));
    }

    #[test]
    fn rejects_negative_huge_and_inconsistent_counts() {
        for value in [
            json!({"working":-1,"pending":1,"failed":0}),
            json!({"working":100001,"pending":1,"failed":0}),
            json!({"working":0,"pending":0,"failed":1}),
            json!({"working":0,"pending":"1","failed":0}),
            json!({"working":0,"pending":1,"failed":0,"waiting":2}),
            json!({"working":0,"pending":2,"failed":1,"waiting":2}),
            json!({"working":0,"pending":1,"failed":1,"waiting":u64::MAX}),
            json!({"working":0,"pending":1,"failed":0,"waiting":-1}),
            json!({"working":0,"pending":1,"failed":0,"waiting":"1"}),
            json!({"working":0,"pending":1,"failed":0,"waiting":null}),
        ] {
            assert!(State::parse(&value).is_none(), "{value}");
        }
    }

    #[test]
    fn requests_and_snapshots_carry_waiting() {
        let value = request(state(2, 3, 1, 1));
        assert_eq!(value["v"], 2);
        assert_eq!(value["op"], "attention");
        assert_eq!(State::parse(&value), Some(state(2, 3, 1, 1)));
        let snapshot: Value = serde_json::from_str(&payload(state(2, 3, 1, 1))).unwrap();
        assert_eq!(snapshot["v"], 1);
        assert_eq!(
            (
                &snapshot["working"],
                &snapshot["pending"],
                &snapshot["failed"],
                &snapshot["waiting"]
            ),
            (&json!(2), &json!(3), &json!(1), &json!(1))
        );
        assert!(snapshot["heartbeat"].is_u64());
    }

    #[test]
    fn lease_sets_both_options_in_one_command_list() {
        assert_eq!(
            lease("%3", 1000, state(2, 3, 1, 2)),
            [
                "set-option",
                "-p",
                "-t",
                "%3",
                CHILD_OPTION,
                "1000:2:3:1",
                ";",
                "set-option",
                "-p",
                "-t",
                "%3",
                CHILD_WAITING_OPTION,
                "1000:2",
            ]
        );
    }

    #[test]
    fn lease_unsets_waiting_in_the_same_command_list_when_zero() {
        assert_eq!(
            lease("%3", 1000, state(2, 1, 1, 0)),
            [
                "set-option",
                "-p",
                "-t",
                "%3",
                CHILD_OPTION,
                "1000:2:1:1",
                ";",
                "set-option",
                "-pu",
                "-t",
                "%3",
                CHILD_WAITING_OPTION,
            ]
        );
    }

    #[test]
    fn cli_validates_waiting_before_routing() {
        let base = [
            "--tmux-socket",
            "/nonexistent/termnav-test-socket",
            "--client-name",
            "/dev/pts/999",
            "--client-pid",
            "1",
            "--client-created",
            "1",
            "--working",
            "0",
            "--pending",
            "1",
            "--failed",
            "1",
        ];
        for waiting in [&["--waiting", "1"][..], &["--waiting", "x"], &["--waiting"]] {
            let args = base
                .iter()
                .chain(waiting)
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>();
            let mut stderr = Vec::new();
            assert_eq!(command(&args, &mut stderr).unwrap(), 2, "{waiting:?}");
        }
        // A valid count passes validation and fails only at the absent client.
        let args = base
            .iter()
            .chain(&["--waiting", "0"])
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        assert_eq!(command(&args, &mut Vec::new()).unwrap(), 1);
    }

    #[test]
    fn proc_foreground_proof_handles_parentheses_in_process_names() {
        assert_eq!(
            foreground_device("42 (name ) (with spaces)) S 1 42 42 34816 42 0", 42),
            Some(34816)
        );
        assert_eq!(
            foreground_device("42 (name) S 1 42 42 -2147483648 42", 42),
            Some(2147483648)
        );
    }

    #[test]
    fn proc_foreground_proof_rejects_missing_or_unowned_terminals() {
        for stat in [
            "43 (name) S 1 42 42 34816 42",
            "42 (name) S 1 42 42 34816 43",
            "42 (name) S 1 42 42 0 42",
            "42 (name) S 1 0 42 34816 0",
            "42 (name) S 1 -1 42 34816 -1",
            "42 (name) S 1 42 42 34816 -1",
            "42 (name) S 1 42 42 4294967296 42",
            "42 (name) S 1 42 42 34816",
            "42 name S 1 42 42 34816 42",
            "42 (name) S 1 nope 42 34816 nope",
        ] {
            assert_eq!(foreground_device(stat, 42), None, "{stat}");
        }
    }
}
