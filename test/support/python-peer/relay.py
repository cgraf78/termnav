#!/usr/bin/env python3
"""Minimal frozen v2 relay peer used only for compatibility tests.

This fixture intentionally implements only the prepare, commit, abort, and
tmux-forwarding wire operations needed to prove interoperability with the Rust
relay. It is not an alternate navigation engine, SSH wrapper, focus owner, or
production fallback. Keeping that boundary narrow makes a passing mixed-peer
test evidence about the protocol instead of evidence about deleted Python
policy accidentally surviving in the test tree.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import secrets
import signal
import socket
import stat
import subprocess
import tempfile
import threading
import time

COMMIT_KEY = b"\x1b[777009u"
COMMIT_QUERY = b"\x1b[?2004$p"
MAX_MESSAGE_BYTES = 512
PROTOCOL_TIMEOUT = 8.0
# Match the Rust relay's bounded admission policy. A connected client may be
# descheduled before its first write on a loaded cross-platform runner; 250 ms
# was short enough to close a valid request and surface EPIPE to the sender.
REQUEST_TIMEOUT = 2.0
VALID_NONCE = re.compile(r"[0-9a-f]{12}")
CLIENT_FORMAT = "#{client_tty}|#{client_pid}|#{client_created}"
STOP = threading.Event()
SERVER_SOCKET = ""


def debug(message: str) -> None:
    """Append diagnostics without making logging part of protocol behavior."""

    path = os.environ.get("TERMNAV_RELAY_LOG")
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8") as output:
            output.write(message + "\n")
    except OSError:
        pass


def reply(result: str) -> dict[str, object]:
    return {"v": 2, "result": result}


def valid_nonce(request: dict[str, object]) -> str | None:
    nonce = request.get("nonce")
    return nonce if isinstance(nonce, str) and VALID_NONCE.fullmatch(nonce) else None


def send_message(path: str, request: dict[str, object]) -> dict[str, object]:
    """Exchange one bounded line-delimited v2 request."""

    payload = json.dumps(request, separators=(",", ":")).encode() + b"\n"
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(PROTOCOL_TIMEOUT)
        client.connect(path)
        client.sendall(payload)
        received = bytearray()
        while not received.endswith(b"\n") and len(received) <= MAX_MESSAGE_BYTES:
            chunk = client.recv(MAX_MESSAGE_BYTES + 1 - len(received))
            if not chunk:
                break
            received.extend(chunk)
    decoded = json.loads(received)
    if not isinstance(decoded, dict):
        raise ValueError("relay response is not an object")
    return decoded


def state_root() -> pathlib.Path:
    """Return an owner-only state directory for this independent fixture."""

    base = pathlib.Path(os.environ.get("XDG_RUNTIME_DIR") or tempfile.gettempdir())
    root = base / f"termnav-python-peer-{os.getuid()}"
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    if root.is_symlink() or stat.S_IMODE(root.stat().st_mode) != 0o700:
        raise OSError(f"unsafe Python peer state directory: {root}")
    return root


def state_path(nonce: str) -> pathlib.Path:
    identity = hashlib.sha256(SERVER_SOCKET.encode()).hexdigest()[:16]
    return state_root() / f"{identity}-{nonce}.json"


def result_path(path: pathlib.Path) -> pathlib.Path:
    return path.with_suffix(".result")


def atomic_write(path: pathlib.Path, value: object) -> None:
    """Publish one state transition without exposing partial JSON."""

    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            json.dump(value, output, separators=(",", ":"))
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def read_state(path: pathlib.Path) -> dict[str, object] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) else None


def tmux_socket() -> str:
    value = os.environ.get("TMUX", "")
    fields = value.rsplit(",", 2)
    return fields[0] if len(fields) == 3 else ""


def run_tmux(tmux_socket_path: str, *arguments: str) -> subprocess.CompletedProcess[str]:
    """Query tmux with the same bounded deadline as one protocol exchange."""

    environment = os.environ.copy()
    environment.pop("TMUX", None)
    return subprocess.run(
        ["tmux", "-S", tmux_socket_path, *arguments],
        env=environment,
        text=True,
        capture_output=True,
        check=False,
        # Alpine's musl runners can take more than two seconds to spawn a tmux
        # client under load. This is only the frozen compatibility peer's
        # safety bound; it does not affect the production navigation hot path.
        timeout=PROTOCOL_TIMEOUT,
    )


def forwarding_state(nonce: str) -> dict[str, object]:
    """Snapshot the one tmux client used by this intentionally narrow peer."""

    tmux_socket_path = tmux_socket()
    pane = os.environ.get("TMUX_PANE", "")
    state: dict[str, object] = {
        "v": 2,
        "nonce": nonce,
        "phase": "prepared",
        "parent": os.environ.get("TERMNAV_PARENT_RELAY", ""),
        "tmux_socket": tmux_socket_path,
        "forward_pane": pane,
    }
    if not tmux_socket_path and not pane:
        return state
    if not tmux_socket_path or not re.fullmatch(r"%[0-9]+", pane):
        raise ValueError("incomplete tmux identity")
    listed = run_tmux(
        tmux_socket_path,
        "list-clients",
        "-F",
        CLIENT_FORMAT,
    )
    # tmux 3.7 sanitizes C0 controls in format output, including a literal tab.
    # A printable delimiter keeps the frozen v2 peer interoperable with both
    # old and current tmux without adding production client-selection policy.
    clients = [line.split("|") for line in listed.stdout.splitlines()]
    clients = [fields for fields in clients if len(fields) == 3]
    if listed.returncode != 0 or len(clients) != 1:
        # The compatibility oracle deliberately has no client-selection
        # policy. Mixed-version tests give it one unambiguous forwarding hop;
        # production multi-client decisions remain owned and tested in Rust.
        # Preserve tmux's exact observation because new tmux releases can
        # otherwise turn a useful fixture failure into an opaque timeout.
        raise ValueError(
            "Python protocol peer requires one tmux client: "
            f"returncode={listed.returncode}, clients={clients!r}, "
            f"stdout={listed.stdout!r}, stderr={listed.stderr!r}"
        )
    tty, pid, created = clients[0]
    state.update(
        {
            "client_tty": tty,
            "client_pid": int(pid),
            "client_created": int(created),
        }
    )
    return state


def abort_parent(parent: str, nonce: str) -> None:
    if not parent:
        return
    try:
        send_message(parent, {"v": 2, "op": "abort-path", "nonce": nonce})
    except (OSError, ValueError):
        pass


def prepare(request: dict[str, object]) -> dict[str, object]:
    nonce = valid_nonce(request)
    if nonce is None:
        return reply("error")
    path = state_path(nonce)
    try:
        state = forwarding_state(nonce)
        atomic_write(path, state)
        parent = str(state.get("parent", ""))
        response = reply("prepared") if not parent else send_message(parent, request)
        if response.get("v") == 2 and response.get("result") == "prepared":
            return response
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        debug(f"prepare failed: {error!r}")
        parent = os.environ.get("TERMNAV_PARENT_RELAY", "")
    abort_parent(parent, nonce)
    path.unlink(missing_ok=True)
    return reply("error")


def abort(request: dict[str, object]) -> dict[str, object]:
    nonce = valid_nonce(request)
    if nonce is None:
        return reply("error")
    path = state_path(nonce)
    state = read_state(path)
    parent = str(state.get("parent", "")) if state else os.environ.get("TERMNAV_PARENT_RELAY", "")
    path.unlink(missing_ok=True)
    result_path(path).unlink(missing_ok=True)
    abort_parent(parent, nonce)
    return reply("aborted")


def wait_for_result(path: pathlib.Path) -> bool:
    deadline = time.monotonic() + PROTOCOL_TIMEOUT
    while time.monotonic() < deadline:
        result = read_state(result_path(path))
        if result is not None:
            result_path(path).unlink(missing_ok=True)
            return result.get("handled") is True
        time.sleep(0.01)
    return False


def commit_path(request: dict[str, object]) -> dict[str, object]:
    nonce = valid_nonce(request)
    if nonce is None:
        return reply("error")
    path = state_path(nonce)
    state = read_state(path)
    if state is None or state.get("phase") != "prepared":
        return reply("error")
    state["phase"] = "committed"
    atomic_write(path, state)
    parent = str(state.get("parent", ""))
    try:
        if parent:
            response = send_message(parent, request)
        elif state.get("tmux_socket"):
            tty = str(state.get("client_tty", ""))
            descriptor = os.open(tty, os.O_WRONLY | os.O_NOCTTY | os.O_APPEND)
            try:
                os.write(descriptor, COMMIT_QUERY)
            finally:
                os.close(descriptor)
            response = reply("emitted")
        else:
            response = reply("emitted")
    except (OSError, ValueError) as error:
        debug(f"commit forwarding failed: {error!r}")
        response = reply("error")
    if state.get("tmux_socket"):
        handled = wait_for_result(path)
    else:
        handled = response.get("v") == 2 and response.get("result") == "emitted"
        path.unlink(missing_ok=True)
    return reply("emitted" if handled else "error")


def navigate(request: dict[str, object]) -> dict[str, object]:
    """Forward navigation while leaving all scope policy to the Rust hop."""

    nonce = valid_nonce(request)
    scope = request.get("scope")
    direction = request.get("direction")
    if nonce is None or scope not in ("pane", "window", "move") or not isinstance(direction, str):
        return reply("error")
    parent = os.environ.get("TERMNAV_PARENT_RELAY", "")
    if not parent:
        return reply("declined")
    try:
        response = send_message(parent, request)
    except (OSError, ValueError):
        return reply("error")
    return response if response.get("result") in ("armed", "declined", "error") else reply("error")


def dispatch(request: object) -> dict[str, object]:
    if not isinstance(request, dict) or request.get("v") != 2:
        return reply("error")
    operation = request.get("op")
    if operation == "prepare-path":
        return prepare(request)
    if operation == "abort-path":
        return abort(request)
    if operation == "commit-path":
        return commit_path(request)
    if operation == "navigate":
        return navigate(request)
    return reply("error")


def handle_connection(connection: socket.socket) -> None:
    with connection:
        connection.settimeout(REQUEST_TIMEOUT)
        received = bytearray()
        try:
            while not received.endswith(b"\n") and len(received) <= MAX_MESSAGE_BYTES:
                chunk = connection.recv(MAX_MESSAGE_BYTES + 1 - len(received))
                if not chunk:
                    return
                received.extend(chunk)
            if len(received) > MAX_MESSAGE_BYTES or not received.endswith(b"\n"):
                return
            response = dispatch(json.loads(received))
            connection.sendall(json.dumps(response, separators=(",", ":")).encode() + b"\n")
        except (OSError, ValueError):
            return


def serve(path: str) -> int:
    global SERVER_SOCKET
    SERVER_SOCKET = path
    socket_path = pathlib.Path(path)
    socket_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    socket_path.unlink(missing_ok=True)
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(path)
    os.chmod(path, 0o600)
    listener.listen()
    listener.settimeout(0.2)

    def stop(_signal: int, _frame: object) -> None:
        STOP.set()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        while not STOP.is_set():
            try:
                connection, _ = listener.accept()
            # Python 3.9 does not reliably alias this to builtin TimeoutError.
            except socket.timeout:  # noqa: UP041
                continue
            threading.Thread(target=handle_connection, args=(connection,), daemon=True).start()
    finally:
        listener.close()
        socket_path.unlink(missing_ok=True)
    return 0


def passthrough(tmux_socket_path: str, state: int | None, pane: str | None) -> None:
    if state is None or state not in range(5) or pane is None:
        return
    response = f"\x1b[?2004;{state}$y".encode()
    run_tmux(
        tmux_socket_path,
        "send-keys",
        "-t",
        pane,
        "-H",
        *[f"{byte:02x}" for byte in response],
    )


def commit(arguments: argparse.Namespace) -> int:
    candidates = []
    for path in state_root().glob("*.json"):
        state = read_state(path)
        if (
            state is not None
            and state.get("phase") == "committed"
            and state.get("tmux_socket") == arguments.tmux_socket
            and state.get("client_tty") == arguments.client_tty
            and state.get("client_pid") == arguments.client_pid
            and state.get("client_created") == arguments.client_created
        ):
            candidates.append((path, state))
    if len(candidates) != 1:
        passthrough(arguments.tmux_socket, arguments.passthrough_decrqm, arguments.pane)
        return 0
    path, state = candidates[0]
    listed = run_tmux(
        arguments.tmux_socket,
        "list-clients",
        "-F",
        CLIENT_FORMAT,
    )
    # Keep commit validation on the same printable wire shape as prepare.
    # tmux 3.7 sanitizes literal C0 controls in format output, so a tab-based
    # comparison can prepare successfully and then silently reject the exact
    # same client when the terminal response arrives.
    expected = f"{arguments.client_tty}|{arguments.client_pid}|{arguments.client_created}"
    handled = listed.returncode == 0 and expected in listed.stdout.splitlines()
    if handled:
        result = run_tmux(
            arguments.tmux_socket,
            "send-keys",
            "-t",
            str(state["forward_pane"]),
            "-H",
            *[f"{byte:02x}" for byte in COMMIT_KEY],
        )
        handled = result.returncode == 0
    atomic_write(result_path(path), {"handled": handled})
    path.unlink(missing_ok=True)
    return 0


def send(arguments: argparse.Namespace) -> int:
    parent = os.environ.get("TERMNAV_PARENT_RELAY", "")
    if not parent:
        return 3
    request = {
        "v": 2,
        "op": "navigate",
        "scope": arguments.scope,
        "direction": arguments.direction,
        "nonce": secrets.token_hex(6),
    }
    try:
        response = send_message(parent, request)
    except (OSError, ValueError):
        return 1
    result = response.get("result")
    return 0 if result == "armed" else 3 if result == "declined" else 1


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser()
    commands = result.add_subparsers(dest="command", required=True)
    serve_parser = commands.add_parser("serve")
    serve_parser.add_argument("--socket", required=True)
    send_parser = commands.add_parser("send")
    send_parser.add_argument("scope", choices=("pane", "window", "move"))
    send_parser.add_argument("direction")
    commit_parser = commands.add_parser("commit")
    commit_parser.add_argument("--tmux-socket", required=True)
    commit_parser.add_argument("--client-tty", required=True)
    commit_parser.add_argument("--client-pid", required=True, type=int)
    commit_parser.add_argument("--client-created", required=True, type=int)
    commit_parser.add_argument("--passthrough-decrqm", type=int)
    commit_parser.add_argument("--pane")
    return result


def main() -> int:
    arguments = parser().parse_args()
    if arguments.command == "serve":
        return serve(arguments.socket)
    if arguments.command == "send":
        return send(arguments)
    return commit(arguments)


if __name__ == "__main__":
    raise SystemExit(main())
