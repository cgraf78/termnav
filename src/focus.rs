//! Hierarchical tmux focus leases.
//!
//! Each nested client publishes a short lease only to its immediate parent.
//! Repeating that one-hop rule at every depth produces one highlighted leaf
//! without central topology knowledge. Generation tokens and exact client
//! identity make delayed releases, PID reuse, and shared inner sessions safe.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::navigation::{Backend, SystemBackend};
use crate::process;
use crate::relay::client::{new_nonce, send};

const FOCUS_OPTION: &str = "@termnav_child_focus";
const CLIENT_UNFOCUSED_OPTION: &str = "@termnav_client_unfocused";
const INACTIVE_STYLE_OPTION: &str = "@termnav_inactive_style";
const STYLE_RESTORE_OPTION: &str = "@termnav_child_focus_restore_active_style";
const WINDOW_ACTIVE_STYLE_OPTION: &str = "window-active-style";
/// Smallest accepted crash-recovery lease.
pub const LEASE_MIN_MS: u64 = 50;
/// Largest accepted crash-recovery lease.
pub const LEASE_MAX_MS: u64 = 30_000;
/// Largest accepted heartbeat interval.
pub const INTERVAL_MAX_MS: u64 = 10_000;

static WATCH_STOPPING: AtomicBool = AtomicBool::new(false);

/// Published lease generation and whether a new expiry watcher is needed.
pub struct Claim {
    /// Exact `token:deadline` value stored in tmux.
    pub value: String,
    /// True only when the publisher token replaced another generation.
    pub start_expirer: bool,
}

enum Parent {
    Tmux { socket: String, pane: String },
    Relay(String),
}

#[derive(Deserialize, Serialize)]
struct StyleRestore {
    had_override: bool,
    value: String,
}

/// Return whether a focus token is safe inside a tmux format guard.
#[must_use]
pub fn valid_token(token: &str) -> bool {
    token.len() == 24
        && token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Return whether the crash-recovery lease is bounded.
#[must_use]
pub fn valid_lease(lease_ms: u64) -> bool {
    (LEASE_MIN_MS..=LEASE_MAX_MS).contains(&lease_ms)
}

/// Return whether the heartbeat interval is bounded.
#[must_use]
pub fn valid_interval(interval_ms: u64) -> bool {
    (LEASE_MIN_MS..=INTERVAL_MAX_MS).contains(&interval_ms)
}

/// Publish or renew one child-focus lease on an immediate parent pane.
pub fn claim(socket: &str, pane: &str, token: &str, lease_ms: u64) -> Option<Claim> {
    if !valid_token(token) || !valid_pane(pane) || !valid_lease(lease_ms) {
        return None;
    }
    let _lock = mutation_lock(socket, pane).ok()?;
    let previous = current_claim(socket, pane);
    let deadline = monotonic_ns().saturating_add(lease_ms.saturating_mul(1_000_000));
    let value = format!("{token}:{deadline}");
    let mut arguments = vec![
        "set-option".to_owned(),
        "-p".to_owned(),
        "-t".to_owned(),
        pane.to_owned(),
        FOCUS_OPTION.to_owned(),
        value.clone(),
    ];
    let previous_token = previous
        .as_deref()
        .and_then(|claim| claim.split_once(':'))
        .map(|pair| pair.0);

    // Heartbeats for one publisher are intentionally one tmux option write.
    // Styling is initialized only when ownership changes, keeping the common
    // renewal path cheap enough to run while users type.
    if previous_token == Some(token) {
        return run_tmux(socket, &arguments)
            .is_some_and(|output| output.status.success())
            .then_some(Claim {
                value,
                start_expirer: false,
            });
    }

    let (style, restore, _) = inactive_style_setup(socket, pane);
    if !style.is_empty() {
        let mut combined = style;
        combined.push(";".to_owned());
        combined.append(&mut arguments);
        arguments = combined;
    }
    let mut succeeded = run_tmux(socket, &arguments).is_some_and(|output| output.status.success());
    if !succeeded && let Some(restore) = restore {
        rollback_claim_setup(socket, pane, &restore, previous.as_deref());
        succeeded = run_tmux(
            socket,
            &[
                "set-option".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                pane.to_owned(),
                FOCUS_OPTION.to_owned(),
                value.clone(),
            ],
        )
        .is_some_and(|output| output.status.success());
    }
    succeeded.then_some(Claim {
        value,
        start_expirer: previous_token != Some(token),
    })
}

/// Release a lease only when the token still owns the current generation.
pub fn release(socket: &str, pane: &str, token: &str) -> bool {
    if !valid_token(token) || !valid_pane(pane) {
        return false;
    }
    let Ok(_lock) = mutation_lock(socket, pane) else {
        return false;
    };
    let Some(value) = current_claim(socket, pane) else {
        return true;
    };
    if value.split_once(':').map(|pair| pair.0) != Some(token) {
        return true;
    }
    clear_exact_locked(socket, pane, &value)
}

/// Watch one pane and remove expired generations until no claim remains.
pub fn expire(socket: &str, pane: &str) -> i32 {
    let Ok(path) = lock_path("expire", &[socket, pane]) else {
        return 1;
    };
    let Ok(_lock) = exclusive_lock(&path, true) else {
        return 0;
    };
    loop {
        let Some(value) = current_claim(socket, pane) else {
            return 0;
        };
        let Some((_, deadline)) = parse_claim(&value) else {
            return 0;
        };
        let remaining = deadline.saturating_sub(monotonic_ns());
        if remaining > 0 {
            thread::sleep(Duration::from_nanos(remaining));
            continue;
        }
        let _ = clear_exact(socket, pane, &value);
    }
}

/// Reconcile one client's active pane with authoritative tmux focus state.
pub fn sync_client_style(socket: &str, client_pid: u32, client_tty: &str) -> bool {
    let Some(pane) = client_pane(socket, client_pid, client_tty) else {
        return true;
    };
    if pane_has_focused_client(socket, &pane) {
        clear_client_unfocused(socket, &pane)
    } else {
        set_client_unfocused(socket, &pane)
    }
}

/// Renew the immediate parent's claim while one exact client remains focused.
pub fn watch(
    socket: &str,
    client_pid: u32,
    client_tty: &str,
    lease_ms: u64,
    interval_ms: u64,
) -> i32 {
    if !valid_lease(lease_ms)
        || !valid_interval(interval_ms)
        || lease_ms < interval_ms.saturating_mul(2)
    {
        return 2;
    }
    if client_focused(socket, client_pid, client_tty) != Some(true) {
        return 0;
    }
    let _ = sync_client_style(socket, client_pid, client_tty);
    let Some(parent) = parent_for_client(client_pid, socket) else {
        return 0;
    };
    let token = format!("{}{}", new_nonce(), new_nonce());
    let client_pid_text = client_pid.to_string();
    let Ok(path) = lock_path("watch", &[socket, &client_pid_text, client_tty]) else {
        return 1;
    };
    let Ok(mut lock) = exclusive_lock(&path, true) else {
        return 0;
    };
    if lock.set_len(0).is_err()
        || lock.seek(SeekFrom::Start(0)).is_err()
        || writeln!(lock, "{}", std::process::id()).is_err()
        || lock.flush().is_err()
    {
        return 1;
    }
    WATCH_STOPPING.store(false, Ordering::SeqCst);
    install_watch_signals();
    let mut claimed = false;
    loop {
        let focused = client_focused(socket, client_pid, client_tty);
        if WATCH_STOPPING.load(Ordering::SeqCst) && focused == Some(true) {
            // A delayed focus-out may signal the old watcher after a focus-in.
            // The live tmux state is authoritative, so keep the renewed owner.
            WATCH_STOPPING.store(false, Ordering::SeqCst);
        }
        if WATCH_STOPPING.load(Ordering::SeqCst) || focused != Some(true) {
            break;
        }
        match update_parent(&parent, "claim", &token, lease_ms) {
            Some(true) => claimed = true,
            Some(false) | None => claimed = false,
        }
        thread::sleep(Duration::from_millis(interval_ms));
    }
    if claimed {
        let _ = update_parent(&parent, "release", &token, lease_ms);
    }
    0
}

/// Stop a stale focus watcher without racing a newer focus-in event.
pub fn stop_watch(socket: &str, client_pid: u32, client_tty: &str) -> i32 {
    if client_focused(socket, client_pid, client_tty) == Some(true) {
        return 0;
    }
    let synced = sync_client_style(socket, client_pid, client_tty);
    if client_focused(socket, client_pid, client_tty) == Some(true) {
        return if synced && sync_client_style(socket, client_pid, client_tty) {
            0
        } else {
            1
        };
    }
    let client_pid_text = client_pid.to_string();
    let Ok(path) = lock_path("watch", &[socket, &client_pid_text, client_tty]) else {
        return 1;
    };
    let watcher = fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok());
    if let Some(watcher) = watcher
        && process_is_watcher(watcher, socket, client_pid, client_tty)
    {
        let _ = unsafe { libc::kill(watcher as i32, libc::SIGTERM) };
    }
    if synced { 0 } else { 1 }
}

/// Handle one focus message received by an SSH relay.
#[must_use]
pub fn handle_relay(request: &Value) -> Value {
    let Some(state) = request.get("state").and_then(Value::as_str) else {
        return reply("error");
    };
    let Some(token) = request
        .get("token")
        .and_then(Value::as_str)
        .filter(|value| valid_token(value))
    else {
        return reply("error");
    };
    let lease = request.get("lease_ms").and_then(Value::as_u64);
    if !matches!(state, "claim" | "release") || state == "claim" && !lease.is_some_and(valid_lease)
    {
        return reply("error");
    }

    let mut backend = SystemBackend::from_current_environment();
    let current = backend.current_scope();
    if current.is_none()
        && std::env::var_os("TMUX").is_some()
        && std::env::var_os("TMUX_PANE").is_some()
    {
        return reply("error");
    }
    let Some(current) = current else {
        let parent = std::env::var("TERMNAV_PARENT_RELAY").unwrap_or_default();
        if parent.is_empty() {
            return reply("declined");
        }
        return send(Path::new(&parent), request, Duration::from_secs(1))
            .ok()
            .filter(|value| {
                matches!(
                    value.get("result").and_then(Value::as_str),
                    Some("claimed" | "released" | "declined" | "error")
                )
            })
            .unwrap_or_else(|| reply("error"));
    };

    if state == "release" {
        return if release(&current.socket, &current.pane, token) {
            reply("released")
        } else {
            reply("error")
        };
    }
    let Some(published) = claim(&current.socket, &current.pane, token, lease.unwrap_or(0)) else {
        return reply("error");
    };
    if published.start_expirer && !start_expirer(&current.socket, &current.pane) {
        let _ = release(&current.socket, &current.pane, token);
        return reply("error");
    }
    reply("claimed")
}

/// Ask the parent tmux server to own the deduplicated expiry helper.
pub fn start_expirer(socket: &str, pane: &str) -> bool {
    let helper = crate::shell::join(&[
        "termnav".to_owned(),
        "tmux".to_owned(),
        "focus".to_owned(),
        "expire".to_owned(),
        "--parent-tmux".to_owned(),
        socket.to_owned(),
        "--parent-pane".to_owned(),
        pane.to_owned(),
    ]);
    let helper = crate::shell::escape_tmux_format(&helper);

    // The lease is state owned by this exact tmux server, so that server is
    // the natural supervisor too. Directly daemonizing from a short-lived CLI
    // leaves process lifetime up to the invoking shell; Android/Termux can
    // reap that orphan even though `spawn` succeeded. `run-shell -b` survives
    // the publishing command and dies with the server whose state it protects.
    // Resolve the one public `termnav` executable through the same server PATH
    // used by every other tmux hook. Android cannot reliably re-execute the
    // process-specific `current_exe` spelling from a later tmux job, and a
    // second lookup contract would make installed behavior less predictable.
    //
    // Tmux format escaping is separate from shell quoting: without both
    // layers, a valid socket or executable path containing `#{...}` or `#(...)`
    // would be rewritten before the shell starts. The helper still takes its
    // per-pane nonblocking lock, so concurrent replacements converge on one
    // worker rather than creating a timer fan-out.
    run_tmux(
        socket,
        &[
            "run-shell".to_owned(),
            "-b".to_owned(),
            format!("exec {helper}"),
        ],
    )
    .is_some_and(|output| output.status.success())
}

fn current_claim(socket: &str, pane: &str) -> Option<String> {
    let output = run_tmux(
        socket,
        &[
            "show-options".to_owned(),
            "-pqv".to_owned(),
            "-t".to_owned(),
            pane.to_owned(),
            FOCUS_OPTION.to_owned(),
        ],
    )?;
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && parse_claim(&value).is_some()).then_some(value)
}

fn option_value(socket: &str, pane: &str, option: &str, pane_local: bool) -> Option<String> {
    let scope = if pane_local {
        vec!["-p".to_owned(), "-t".to_owned(), pane.to_owned()]
    } else {
        vec!["-g".to_owned()]
    };
    let mut shown = vec!["show-options".to_owned()];
    shown.extend(scope.clone());
    shown.extend(["-q".to_owned(), option.to_owned()]);
    let present = run_tmux(socket, &shown)?;
    if !present.status.success() || present.stdout.is_empty() {
        return None;
    }
    let mut value = vec!["show-options".to_owned()];
    value.extend(scope);
    value.extend(["-qv".to_owned(), option.to_owned()]);
    let value = run_tmux(socket, &value)?;
    value.status.success().then(|| {
        String::from_utf8_lossy(&value.stdout)
            .trim_end_matches('\n')
            .to_owned()
    })
}

fn inactive_style_setup(socket: &str, pane: &str) -> (Vec<String>, Option<String>, bool) {
    let Some(inactive) = option_value(socket, pane, INACTIVE_STYLE_OPTION, false) else {
        return (Vec::new(), None, false);
    };
    if inactive.is_empty() {
        return (Vec::new(), None, false);
    }
    if option_value(socket, pane, STYLE_RESTORE_OPTION, true).is_some() {
        return (Vec::new(), None, true);
    }
    let existing = option_value(socket, pane, WINDOW_ACTIVE_STYLE_OPTION, true);
    let restore = serde_json::to_string(&StyleRestore {
        had_override: existing.is_some(),
        value: existing.unwrap_or_default(),
    })
    .ok();
    let Some(restore) = restore else {
        return (Vec::new(), None, true);
    };
    (
        vec![
            "set-option".to_owned(),
            "-p".to_owned(),
            "-t".to_owned(),
            pane.to_owned(),
            WINDOW_ACTIVE_STYLE_OPTION.to_owned(),
            inactive,
            ";".to_owned(),
            "set-option".to_owned(),
            "-p".to_owned(),
            "-t".to_owned(),
            pane.to_owned(),
            STYLE_RESTORE_OPTION.to_owned(),
            restore.clone(),
        ],
        Some(restore),
        true,
    )
}

fn restore_arguments(pane: &str, encoded: &str) -> Option<Vec<String>> {
    let state: StyleRestore = serde_json::from_str(encoded).ok()?;
    if state.had_override {
        Some(vec![
            "set-option".to_owned(),
            "-p".to_owned(),
            "-t".to_owned(),
            pane.to_owned(),
            WINDOW_ACTIVE_STYLE_OPTION.to_owned(),
            state.value,
        ])
    } else {
        Some(vec![
            "set-option".to_owned(),
            "-pu".to_owned(),
            "-t".to_owned(),
            pane.to_owned(),
            WINDOW_ACTIVE_STYLE_OPTION.to_owned(),
        ])
    }
}

fn rollback_claim_setup(socket: &str, pane: &str, restore: &str, previous: Option<&str>) {
    let Some(mut arguments) = restore_arguments(pane, restore) else {
        return;
    };
    arguments.extend([
        ";".to_owned(),
        "set-option".to_owned(),
        "-pu".to_owned(),
        "-t".to_owned(),
        pane.to_owned(),
        STYLE_RESTORE_OPTION.to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        if previous.is_some() { "-p" } else { "-pu" }.to_owned(),
        "-t".to_owned(),
        pane.to_owned(),
        FOCUS_OPTION.to_owned(),
    ]);
    if let Some(previous) = previous {
        arguments.push(previous.to_owned());
    }
    let _ = run_tmux(socket, &arguments);
}

fn set_client_unfocused(socket: &str, pane: &str) -> bool {
    let Ok(_lock) = mutation_lock(socket, pane) else {
        return false;
    };
    let (mut arguments, restore, enabled) = inactive_style_setup(socket, pane);
    if !enabled {
        return true;
    }
    if !arguments.is_empty() {
        arguments.push(";".to_owned());
    }
    arguments.extend([
        "set-option".to_owned(),
        "-p".to_owned(),
        "-t".to_owned(),
        pane.to_owned(),
        CLIENT_UNFOCUSED_OPTION.to_owned(),
        "1".to_owned(),
    ]);
    if run_tmux(socket, &arguments).is_some_and(|output| output.status.success()) {
        return true;
    }
    if let Some(restore) = restore
        && let Some(mut rollback) = restore_arguments(pane, &restore)
    {
        rollback.extend([
            ";".to_owned(),
            "set-option".to_owned(),
            "-pu".to_owned(),
            "-t".to_owned(),
            pane.to_owned(),
            STYLE_RESTORE_OPTION.to_owned(),
            ";".to_owned(),
            "set-option".to_owned(),
            "-pu".to_owned(),
            "-t".to_owned(),
            pane.to_owned(),
            CLIENT_UNFOCUSED_OPTION.to_owned(),
        ]);
        let _ = run_tmux(socket, &rollback);
    }
    false
}

fn clear_client_unfocused(socket: &str, pane: &str) -> bool {
    let Ok(_lock) = mutation_lock(socket, pane) else {
        return false;
    };
    let mut arguments = vec![
        "set-option".to_owned(),
        "-pu".to_owned(),
        "-t".to_owned(),
        pane.to_owned(),
        CLIENT_UNFOCUSED_OPTION.to_owned(),
    ];
    if current_claim(socket, pane).is_none()
        && let Some(encoded) = option_value(socket, pane, STYLE_RESTORE_OPTION, true)
    {
        let restore = restore_arguments(pane, &encoded);
        arguments.extend([
            ";".to_owned(),
            "set-option".to_owned(),
            "-pu".to_owned(),
            "-t".to_owned(),
            pane.to_owned(),
            STYLE_RESTORE_OPTION.to_owned(),
        ]);
        if let Some(restore) = restore {
            arguments.push(";".to_owned());
            arguments.extend(restore);
        }
    }
    if run_tmux(socket, &arguments).is_some_and(|output| output.status.success()) {
        return true;
    }
    let _ = run_tmux(
        socket,
        &[
            "set-option".to_owned(),
            "-pu".to_owned(),
            "-t".to_owned(),
            pane.to_owned(),
            WINDOW_ACTIVE_STYLE_OPTION.to_owned(),
        ],
    );
    false
}

fn clear_exact(socket: &str, pane: &str, value: &str) -> bool {
    if parse_claim(value).is_none() || !valid_pane(pane) {
        return false;
    }
    let Ok(_lock) = mutation_lock(socket, pane) else {
        return false;
    };
    clear_exact_locked(socket, pane, value)
}

fn clear_exact_locked(socket: &str, pane: &str, value: &str) -> bool {
    let restore_marker = option_value(socket, pane, STYLE_RESTORE_OPTION, true);
    let restore = restore_marker
        .as_deref()
        .and_then(|encoded| restore_arguments(pane, encoded));
    let mut commands = vec![crate::shell::join(&[
        "set-option".to_owned(),
        "-pu".to_owned(),
        "-t".to_owned(),
        pane.to_owned(),
        FOCUS_OPTION.to_owned(),
    ])];
    if restore_marker.is_some()
        && option_value(socket, pane, CLIENT_UNFOCUSED_OPTION, true).is_none()
    {
        // A corrupt marker cannot tell us which style to restore, but leaving
        // it behind would permanently suppress every future style snapshot.
        // Clear the ownership marker unconditionally and preserve the current
        // pane style when decoding fails; that is the only lossless fallback.
        commands.push(crate::shell::join(&[
            "set-option".to_owned(),
            "-pu".to_owned(),
            "-t".to_owned(),
            pane.to_owned(),
            STYLE_RESTORE_OPTION.to_owned(),
        ]));
        if let Some(restore) = &restore {
            commands.push(crate::shell::join(restore));
        }
    }
    let condition = format!("#{{==:#{{{FOCUS_OPTION}}},{value}}}");
    let result = run_tmux(
        socket,
        &[
            "if-shell".to_owned(),
            "-F".to_owned(),
            "-t".to_owned(),
            pane.to_owned(),
            condition,
            commands.join(" ; "),
            String::new(),
        ],
    );
    let succeeded = result.is_some_and(|output| output.status.success());
    if !succeeded && restore_marker.is_some() {
        let _ = run_tmux(
            socket,
            &[
                "set-option".to_owned(),
                "-pu".to_owned(),
                "-t".to_owned(),
                pane.to_owned(),
                WINDOW_ACTIVE_STYLE_OPTION.to_owned(),
            ],
        );
    }
    succeeded
}

fn parent_for_client(client_pid: u32, own_socket: &str) -> Option<Parent> {
    let tmux = process::environment(client_pid, "TMUX").unwrap_or_default();
    let pane = process::environment(client_pid, "TMUX_PANE").unwrap_or_default();
    if valid_pane(&pane) {
        let parts = tmux.rsplitn(3, ',').collect::<Vec<_>>();
        if parts.len() == 3
            && !parts[2].is_empty()
            && fs::canonicalize(parts[2]).ok() != fs::canonicalize(own_socket).ok()
        {
            return Some(Parent::Tmux {
                socket: parts[2].to_owned(),
                pane,
            });
        }
    }
    process::environment(client_pid, "TERMNAV_PARENT_RELAY")
        .filter(|relay| Path::new(relay).is_absolute())
        .map(Parent::Relay)
}

fn update_parent(parent: &Parent, state: &str, token: &str, lease_ms: u64) -> Option<bool> {
    match parent {
        Parent::Tmux { socket, pane } => {
            if state == "claim" {
                let published = claim(socket, pane, token, lease_ms)?;
                if published.start_expirer && !start_expirer(socket, pane) {
                    let _ = release(socket, pane, token);
                    return Some(false);
                }
                Some(true)
            } else {
                Some(release(socket, pane, token))
            }
        }
        Parent::Relay(path) => {
            let mut request = json!({
                "v": 2,
                "op": "focus",
                "state": state,
                "token": token,
            });
            if state == "claim" {
                request["lease_ms"] = json!(lease_ms);
            }
            let expected = if state == "claim" {
                "claimed"
            } else {
                "released"
            };
            Some(
                send(Path::new(path), &request, Duration::from_secs(1))
                    .ok()
                    .and_then(|value| {
                        value
                            .get("result")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .as_deref()
                    == Some(expected),
            )
        }
    }
}

fn client_focused(socket: &str, client_pid: u32, client_tty: &str) -> Option<bool> {
    let output = run_tmux(
        socket,
        &[
            "list-clients".to_owned(),
            "-F".to_owned(),
            "#{client_pid} #{client_tty} #{client_flags}".to_owned(),
        ],
    )?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let fields = line.splitn(3, char::is_whitespace).collect::<Vec<_>>();
            (fields.len() == 3 && fields[0] == client_pid.to_string() && fields[1] == client_tty)
                .then(|| fields[2].split(',').any(|flag| flag == "focused"))
        })
}

fn client_pane(socket: &str, client_pid: u32, client_tty: &str) -> Option<String> {
    let output = run_tmux(
        socket,
        &[
            "list-clients".to_owned(),
            "-F".to_owned(),
            "#{client_pid} #{client_tty} #{pane_id}".to_owned(),
        ],
    )?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            (fields.len() == 3
                && fields[0] == client_pid.to_string()
                && fields[1] == client_tty
                && valid_pane(fields[2]))
            .then(|| fields[2].to_owned())
        })
}

fn pane_has_focused_client(socket: &str, pane: &str) -> bool {
    run_tmux(
        socket,
        &[
            "list-clients".to_owned(),
            "-F".to_owned(),
            "#{pane_id} #{m:*focused*,#{client_flags}} #{client_control_mode}".to_owned(),
        ],
    )
    .filter(|output| output.status.success())
    .is_some_and(|output| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.split_whitespace().collect::<Vec<_>>() == [pane, "1", "0"])
    })
}

fn process_is_watcher(pid: u32, socket: &str, client_pid: u32, client_tty: &str) -> bool {
    let client_pid = client_pid.to_string();
    let expected = watcher_identity(socket, &client_pid, client_tty);
    if let Ok(bytes) = fs::read(format!("/proc/{pid}/cmdline")) {
        let arguments = bytes
            .split(|byte| *byte == 0)
            .filter(|value| !value.is_empty())
            .map(|value| String::from_utf8_lossy(value))
            .collect::<Vec<_>>();
        return watcher_arguments_match(&arguments, &expected);
    }

    // macOS has no procfs. This is a cold focus-out path, so one bounded `ps`
    // query is preferable to leaving the publisher alive until its next lease
    // heartbeat. Match the complete ordered watcher identity before signaling;
    // a bare PID check could kill an unrelated process after PID reuse.
    let mut command = Command::new("ps");
    command.args(["-p", &pid.to_string(), "-o", "command="]);
    process::output_timeout(&mut command, Duration::from_secs(2))
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            watcher_command_matches(&String::from_utf8_lossy(&output.stdout), &expected)
        })
}

fn watcher_identity<'a>(socket: &'a str, client_pid: &'a str, client_tty: &'a str) -> [&'a str; 9] {
    [
        "tmux",
        "focus",
        "watch",
        "--tmux-socket",
        socket,
        "--client-pid",
        client_pid,
        "--client-tty",
        client_tty,
    ]
}

fn watcher_arguments_match(arguments: &[std::borrow::Cow<'_, str>], expected: &[&str]) -> bool {
    arguments.windows(expected.len()).any(|window| {
        window
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.as_ref() == *expected)
    })
}

fn watcher_command_matches(command: &str, expected: &[&str]) -> bool {
    command.contains(&expected.join(" "))
}

fn mutation_lock(socket: &str, pane: &str) -> io::Result<File> {
    exclusive_lock(&lock_path("state", &[socket, pane])?, false)
}

fn exclusive_lock(path: &Path, nonblocking: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    let operation = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn lock_path(kind: &str, identity: &[&str]) -> io::Result<PathBuf> {
    let mut digest = Sha256::new();
    for (index, value) in identity.iter().enumerate() {
        if index > 0 {
            digest.update([0]);
        }
        digest.update(value.as_bytes());
    }
    let encoded = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    crate::runtime::private_subdirectory(&[std::ffi::OsStr::new("focus")])
        .map(|path| path.join(format!("{kind}-{}.lock", &encoded[..24])))
}

fn parse_claim(value: &str) -> Option<(&str, u64)> {
    let (token, deadline) = value.split_once(':')?;
    (valid_token(token))
        .then(|| deadline.parse().ok())
        .flatten()
        .map(|deadline| (token, deadline))
}

fn valid_pane(value: &str) -> bool {
    value.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn monotonic_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut time) } != 0 {
        return 0;
    }
    (time.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(time.tv_nsec as u64)
}

fn run_tmux(socket: &str, arguments: &[String]) -> Option<std::process::Output> {
    let mut command = Command::new("tmux");
    command
        .arg("-S")
        .arg(socket)
        .args(arguments)
        .env_remove("TMUX");
    process::output_timeout(&mut command, Duration::from_secs(2)).ok()
}

fn install_watch_signals() {
    extern "C" fn stop(_signal: libc::c_int) {
        WATCH_STOPPING.store(true, Ordering::SeqCst);
    }
    let handler = stop as *const () as libc::sighandler_t;
    let _ = unsafe { libc::signal(libc::SIGTERM, handler) };
    let _ = unsafe { libc::signal(libc::SIGINT, handler) };
}

fn reply(result: &str) -> Value {
    json!({"v": 2, "result": result})
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{
        parse_claim, valid_interval, valid_lease, valid_token, watcher_arguments_match,
        watcher_command_matches, watcher_identity,
    };

    #[test]
    fn focus_identifiers_and_timings_are_bounded() {
        assert!(valid_token("0123456789abcdef01234567"));
        assert!(!valid_token("0123456789ABCDEF01234567"));
        assert!(valid_lease(50));
        assert!(valid_lease(30_000));
        assert!(!valid_lease(30_001));
        assert!(valid_interval(10_000));
        assert!(!valid_interval(10_001));
    }

    #[test]
    fn claim_parser_requires_an_exact_generation() {
        assert_eq!(
            parse_claim("0123456789abcdef01234567:123"),
            Some(("0123456789abcdef01234567", 123))
        );
        assert!(parse_claim("short:123").is_none());
        assert!(parse_claim("0123456789abcdef01234567:nope").is_none());
    }

    #[test]
    fn watcher_identity_rejects_pid_reuse_on_procfs_and_ps_paths() {
        let expected = watcher_identity("/tmp/tmux.sock", "123", "/dev/pts/4");
        let exact = [
            "/bin/termnav",
            "tmux",
            "focus",
            "watch",
            "--tmux-socket",
            "/tmp/tmux.sock",
            "--client-pid",
            "123",
            "--client-tty",
            "/dev/pts/4",
        ]
        .map(Cow::Borrowed);
        assert!(watcher_arguments_match(&exact, &expected));
        assert!(watcher_command_matches(
            "/bin/termnav tmux focus watch --tmux-socket /tmp/tmux.sock --client-pid 123 --client-tty /dev/pts/4",
            &expected,
        ));
        assert!(!watcher_command_matches(
            "/bin/termnav tmux focus watch --tmux-socket /tmp/other.sock --client-pid 123 --client-tty /dev/pts/4",
            &expected,
        ));
    }
}
