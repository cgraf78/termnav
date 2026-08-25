"""Pane-scoped ownership primitives for hierarchical tmux focus."""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import re
import secrets
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

FOCUS_OPTION = "@termnav_child_focus"
TOKEN_PATTERN = re.compile(r"[0-9a-f]{24}")
CLAIM_PATTERN = re.compile(r"([0-9a-f]{24}):([0-9]+)")
LEASE_MIN_MS = 50
LEASE_MAX_MS = 30_000
INTERVAL_MAX_MS = 10_000


@dataclass(frozen=True)
class Claim:
    """One published generation and whether it needs an expiry watcher."""

    value: str
    start_expirer: bool


@dataclass(frozen=True)
class Parent:
    """The immediate parent route of one attached tmux client."""

    tmux_socket: str | None = None
    pane: str | None = None
    relay: str | None = None


def valid_token(token: str) -> bool:
    """Return whether a token is safe to embed in a tmux format guard."""
    return TOKEN_PATTERN.fullmatch(token) is not None


def valid_pane(pane: str) -> bool:
    """Accept only tmux's stable numeric pane identifiers."""
    return pane.startswith("%") and pane[1:].isdigit()


def valid_lease(lease_ms: object) -> bool:
    """Return whether a crash-cleanup lease is bounded and non-Boolean."""
    return (
        isinstance(lease_ms, int)
        and not isinstance(lease_ms, bool)
        and LEASE_MIN_MS <= lease_ms <= LEASE_MAX_MS
    )


def valid_interval(interval_ms: object) -> bool:
    """Return whether a heartbeat interval is bounded and non-Boolean."""
    return (
        isinstance(interval_ms, int)
        and not isinstance(interval_ms, bool)
        and LEASE_MIN_MS <= interval_ms <= INTERVAL_MAX_MS
    )


def run_tmux(socket_path: str, *arguments: str) -> subprocess.CompletedProcess[str]:
    """Run against an explicit server without inheriting another tmux scope."""
    environment = os.environ.copy()
    environment.pop("TMUX", None)
    return subprocess.run(
        ["tmux", "-S", socket_path, *arguments],
        env=environment,
        text=True,
        capture_output=True,
        check=False,
        timeout=2,
    )


def current_claim(socket_path: str, pane: str) -> str | None:
    """Read the pane's exact lease generation, if one is currently present."""
    result = run_tmux(
        socket_path,
        "show-options",
        "-pqv",
        "-t",
        pane,
        FOCUS_OPTION,
    )
    if result.returncode != 0:
        return None
    value = result.stdout.strip()
    return value if CLAIM_PATTERN.fullmatch(value) else None


def claim(socket_path: str, pane: str, token: str, lease_ms: int) -> Claim | None:
    """Publish one versioned child-focus lease on its immediate parent pane."""
    if not valid_token(token) or not valid_pane(pane) or not valid_lease(lease_ms):
        return None
    previous = current_claim(socket_path, pane)
    deadline_ns = time.monotonic_ns() + lease_ms * 1_000_000
    claim_value = f"{token}:{deadline_ns}"
    result = run_tmux(
        socket_path,
        "set-option",
        "-p",
        "-t",
        pane,
        FOCUS_OPTION,
        claim_value,
    )
    if result.returncode != 0:
        return None
    # One independent expirer follows all renewals for a publisher token. A
    # replacement token attempts to start another; the pane lock makes that a
    # cheap no-op while the original expirer is still alive.
    previous_token = previous.partition(":")[0] if previous else None
    return Claim(claim_value, previous_token != token)


def clear_exact(socket_path: str, pane: str, claim_value: str) -> bool:
    """Clear exactly one observed lease generation."""
    if not CLAIM_PATTERN.fullmatch(claim_value) or not valid_pane(pane):
        return False
    condition = f"#{{==:#{{{FOCUS_OPTION}}},{claim_value}}}"
    result = run_tmux(
        socket_path,
        "if-shell",
        "-F",
        "-t",
        pane,
        condition,
        f"set-option -p -u -t {pane} {FOCUS_OPTION}",
        "",
    )
    return result.returncode == 0


def release(socket_path: str, pane: str, token: str) -> bool:
    """Clear a claim only if it is still owned by the releasing publisher."""
    if not valid_token(token) or not valid_pane(pane):
        return False
    claim_value = current_claim(socket_path, pane)
    if claim_value is None or claim_value.partition(":")[0] != token:
        return True
    # The compare and unset are one tmux command. A concurrent renewal with the
    # same token, or a replacement publisher, cannot be erased after this read.
    return clear_exact(socket_path, pane, claim_value)


def runtime_dir() -> Path:
    """Return a private runtime directory for focus-watcher lock files."""
    configured = os.environ.get("XDG_RUNTIME_DIR", "")
    base = (
        Path(configured)
        if configured and os.path.isabs(configured)
        else Path(tempfile.gettempdir())
    )
    path = base / f"termnav-{os.getuid()}" / "focus"
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        raise OSError(f"unsafe focus runtime directory: {path}")
    os.chmod(path, 0o700)
    return path


def lock_path(kind: str, *identity: str) -> Path:
    """Map unbounded socket and tty names to one portable filename."""
    digest = hashlib.sha256("\0".join(identity).encode()).hexdigest()[:24]
    return runtime_dir() / f"{kind}-{digest}.lock"


def expire(socket_path: str, pane: str) -> None:
    """Keep one low-cost expiry watcher for all generations on a parent pane."""
    path = lock_path("expire", socket_path, pane)
    with path.open("a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return
        while True:
            value = current_claim(socket_path, pane)
            if value is None:
                return
            match = CLAIM_PATTERN.fullmatch(value)
            if match is None:
                return
            remaining_ns = int(match.group(2)) - time.monotonic_ns()
            if remaining_ns > 0:
                time.sleep(remaining_ns / 1_000_000_000)
                continue
            # A heartbeat may land between our read and this command. Exact
            # comparison makes that race harmless; then re-read for replacement.
            clear_exact(socket_path, pane, value)


def parse_ps_environment(output: str, name: str) -> str | None:
    """Extract NAME from `ps eww` output without truncating spaces in values."""
    pattern = rf"(?:^|\s){re.escape(name)}=(.*?)(?=\s[A-Za-z_][A-Za-z0-9_]*=|$)"
    match = re.search(pattern, output)
    return match.group(1) if match else None


def process_env(pid: int, name: str) -> str | None:
    """Read one client-process variable on Linux or the macOS ps fallback."""
    proc_readable = True
    try:
        data = Path(f"/proc/{pid}/environ").read_bytes()
    except OSError:
        proc_readable = False
        data = b""
    prefix = name.encode() + b"="
    for entry in data.split(b"\0"):
        if entry.startswith(prefix):
            return entry[len(prefix) :].decode(errors="strict")
    if proc_readable:
        return None
    try:
        output = subprocess.run(
            ["ps", "eww", "-p", str(pid), "-o", "command="],
            text=True,
            capture_output=True,
            check=False,
            timeout=2,
        ).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    return parse_ps_environment(output, name)


def parent_for_client(client_pid: int, own_socket: str) -> Parent | None:
    """Resolve only the immediate parent of the exact attached tmux client."""
    parent_tmux = process_env(client_pid, "TMUX") or ""
    parent_pane = process_env(client_pid, "TMUX_PANE") or ""
    if parent_tmux and valid_pane(parent_pane):
        parent_socket = parent_tmux.rsplit(",", 2)[0]
        if parent_socket and os.path.realpath(parent_socket) != os.path.realpath(own_socket):
            return Parent(tmux_socket=parent_socket, pane=parent_pane)
    relay = process_env(client_pid, "TERMNAV_PARENT_RELAY") or ""
    if relay and os.path.isabs(relay):
        return Parent(relay=relay)
    return None


def client_focused(socket_path: str, client_pid: int, client_tty: str) -> bool | None:
    """Return the exact client's focus state, or None after it detaches."""
    result = run_tmux(
        socket_path,
        "list-clients",
        "-F",
        "#{client_pid} #{client_tty} #{client_flags}",
    )
    if result.returncode != 0:
        return None
    for line in result.stdout.splitlines():
        # tmux 3.7 sanitizes literal tabs in format output to underscores.
        # These three native fields cannot contain whitespace, so ordinary
        # field splitting is portable across old and new tmux releases.
        fields = line.split(maxsplit=2)
        if len(fields) != 3 or fields[0] != str(client_pid) or fields[1] != client_tty:
            continue
        return "focused" in fields[2].split(",")
    return None


def send_relay(path: str, state: str, token: str, lease_ms: int) -> bool:
    """Send one bounded focus update through the nearest SSH relay."""
    message: dict[str, object] = {
        "v": 2,
        "op": "focus",
        "state": state,
        "token": token,
    }
    if state == "claim":
        message["lease_ms"] = lease_ms
    payload = json.dumps(message, separators=(",", ":")).encode() + b"\n"
    try:
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(1)
            client.connect(path)
            client.sendall(payload)
            reply = b""
            while b"\n" not in reply and len(reply) <= 512:
                chunk = client.recv(513 - len(reply))
                if not chunk:
                    break
                reply += chunk
        decoded = json.loads(reply)
    except (OSError, ValueError):
        return False
    expected = "claimed" if state == "claim" else "released"
    return isinstance(decoded, dict) and decoded.get("v") == 2 and decoded.get("result") == expected


def update_parent(parent: Parent, state: str, token: str, lease_ms: int) -> Claim | bool | None:
    """Apply one update through either the local or relayed parent interface."""
    if parent.tmux_socket is not None and parent.pane is not None:
        if state == "claim":
            return claim(parent.tmux_socket, parent.pane, token, lease_ms)
        return release(parent.tmux_socket, parent.pane, token)
    if parent.relay is not None:
        return send_relay(parent.relay, state, token, lease_ms)
    return None


def new_token() -> str:
    """Return an unguessable ownership token for one publisher lifetime."""
    return secrets.token_hex(12)


def start_expirer(executable: Path, socket_path: str, pane: str) -> bool:
    """Start the pane's deduplicated expiry watcher outside this process."""
    try:
        subprocess.Popen(
            [
                sys.executable,
                str(executable),
                "expire",
                "--parent-tmux",
                socket_path,
                "--parent-pane",
                pane,
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except OSError:
        # A lease without an expiry worker could leave a dead container pane
        # bright indefinitely. Let the caller retract it and retry instead.
        return False
    return True


def process_is_watcher(
    executable: Path,
    pid: int,
    socket_path: str,
    client_pid: int,
    client_tty: str,
) -> bool:
    """Guard signals against PID reuse by matching the complete watcher identity."""
    expected = [
        str(executable),
        "watch",
        "--tmux-socket",
        socket_path,
        "--client-pid",
        str(client_pid),
        "--client-tty",
        client_tty,
    ]
    try:
        arguments = [
            value.decode(errors="replace")
            for value in Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
            if value
        ]
    except OSError:
        try:
            command = subprocess.run(
                ["ps", "-p", str(pid), "-o", "command="],
                text=True,
                capture_output=True,
                check=False,
                timeout=2,
            ).stdout
        except (OSError, subprocess.TimeoutExpired):
            return False
        return all(value in command for value in expected)
    return any(
        arguments[index : index + len(expected)] == expected for index in range(len(arguments))
    )


def stop_watch(executable: Path, socket_path: str, client_pid: int, client_tty: str) -> int:
    """Stop an unfocused publisher while rejecting a delayed focus-out hook."""
    # Background hooks can complete out of order during a rapid focus bounce.
    # Re-read authoritative client state before signaling so an old focus-out
    # cannot kill the publisher created by the newer focus-in event.
    if client_focused(socket_path, client_pid, client_tty):
        return 0
    path = lock_path("watch", socket_path, str(client_pid), client_tty)
    try:
        raw_pid = path.read_text(encoding="ascii").strip()
        watcher_pid = int(raw_pid)
    except (OSError, ValueError):
        return 0
    if not process_is_watcher(executable, watcher_pid, socket_path, client_pid, client_tty):
        return 0
    try:
        os.kill(watcher_pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    return 0


def watch(
    executable: Path,
    socket_path: str,
    client_pid: int,
    client_tty: str,
    lease_ms: int,
    interval_ms: int,
) -> int:
    """Renew one focused nested client's immediate-parent claim."""
    if not valid_lease(lease_ms) or not valid_interval(interval_ms) or lease_ms < interval_ms * 2:
        return 2
    parent = parent_for_client(client_pid, socket_path)
    if parent is None:
        # Direct terminal clients have no parent to claim. Exiting immediately
        # keeps the ordinary, non-nested path free of persistent helpers.
        return 0
    token = new_token()
    path = lock_path("watch", socket_path, str(client_pid), client_tty)
    stopping = False

    def stop(_signum: int, _frame: object) -> None:
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    with path.open("a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return 0
        lock.seek(0)
        lock.truncate()
        lock.write(f"{os.getpid()}\n")
        lock.flush()
        claimed = False
        try:
            while not stopping:
                focused = client_focused(socket_path, client_pid, client_tty)
                if not focused:
                    break
                result = update_parent(parent, "claim", token, lease_ms)
                if isinstance(result, Claim):
                    expirer_started = True
                    if result.start_expirer and parent.tmux_socket and parent.pane:
                        expirer_started = start_expirer(executable, parent.tmux_socket, parent.pane)
                    if expirer_started:
                        claimed = True
                    else:
                        # Clear this exact token so a resource-exhaustion event
                        # cannot turn a bounded lease into permanent state.
                        update_parent(parent, "release", token, lease_ms)
                        claimed = False
                elif result is True:
                    claimed = True
                time.sleep(interval_ms / 1000)
        finally:
            if claimed:
                update_parent(parent, "release", token, lease_ms)
    return 0
