"""Persistent dispatch service for latency-sensitive navigation gestures."""

from __future__ import annotations

import fcntl
import os
import socket
import socketserver
import stat
import threading
import time
from collections.abc import Callable
from pathlib import Path

from navigation import Client, Navigator, SystemBackend
from navigation_protocol import valid_wire_request

MAX_HEADER_BYTES = 4096
# PATH can legitimately be several kilobytes on tool-managed developer hosts.
# Keep a strict local-protocol bound without rejecting realistic caller state.
MAX_BODY_BYTES = 65536
FALLBACK = 125
DEFAULT_IDLE_SECONDS = 3600.0
DEFAULT_SWEEP_SECONDS = 300.0
PROVIDER_ROOT = str(Path(__file__).resolve().parents[2])


def _option_pairs(arguments: list[str], required: set[str]) -> dict[str, str] | None:
    """Parse the fixed ``--name value`` forms emitted by tmux bindings."""

    if len(arguments) != len(required) * 2:
        return None
    values: dict[str, str] = {}
    for index in range(0, len(arguments), 2):
        name = arguments[index]
        if name not in required or name in values:
            return None
        values[name] = arguments[index + 1]
    return values if values.keys() == required else None


def _navigate(arguments: list[str], environment: dict[str, str]) -> int:
    """Route the exact parent-boundary form emitted by tmux."""

    if len(arguments) < 4 or arguments[3] != "--parent":
        return FALLBACK
    action, direction = arguments[1:3]
    required = {
        "--client-pid",
        "--client-tty",
        "--client-created",
        "--client-termtype",
        "--source-socket",
        "--source-pane",
        "--source-session",
    }
    values = _option_pairs(arguments[4:], required)
    if values is None:
        return FALLBACK
    try:
        pid = int(values["--client-pid"])
        created = int(values["--client-created"])
    except ValueError:
        return FALLBACK
    pane = values["--source-pane"]
    socket_path = values["--source-socket"]
    tty = values["--client-tty"]
    if (
        pid <= 0
        or created < 0
        or not tty.startswith("/")
        or not socket_path.startswith("/")
        or not pane.startswith("%")
        or not pane[1:].isdigit()
    ):
        return FALLBACK

    client = Client(
        activity=0,
        pid=pid,
        tty=tty,
        termtype=values["--client-termtype"],
        session=values["--source-session"],
        pane=pane,
        socket=socket_path,
        exact=True,
        created=created,
    )
    backend = SystemBackend(environment)
    try:
        return int(
            Navigator(backend, now=lambda: time.time_ns() // 1_000_000_000).navigate(
                action,
                direction,
                include_current=False,
                exact_client=client,
            )
        )
    except ValueError:
        return FALLBACK


def _commit(
    arguments: list[str],
    environment: dict[str, str],
    commit_command: Callable[..., int],
) -> int:
    """Run one exact relay commit without starting another interpreter."""

    required = {
        "--tmux-socket",
        "--client-tty",
        "--client-pid",
        "--client-created",
    }
    optional = {"--passthrough-decrqm", "--pane"}
    values = _option_pairs(arguments[1:], required)
    if values is None:
        values = _option_pairs(arguments[1:], required | optional)
    if values is None:
        return FALLBACK
    try:
        pid = int(values["--client-pid"])
        created = int(values["--client-created"])
        passthrough = (
            int(values["--passthrough-decrqm"]) if "--passthrough-decrqm" in values else None
        )
    except ValueError:
        return FALLBACK
    if passthrough is not None and not 0 <= passthrough <= 4:
        return FALLBACK
    return commit_command(
        values["--tmux-socket"],
        values["--client-tty"],
        pid,
        created,
        passthrough,
        values.get("--pane"),
        environment,
    )


def _send(arguments: list[str], parent_relay: str, send_command: Callable[..., int]) -> int:
    """Run the two production relay-send forms through the resident process."""

    if len(arguments) < 3 or not valid_wire_request(arguments[1], arguments[2]):
        return FALLBACK
    if len(arguments) == 3:
        return send_command(arguments[1], arguments[2], parent_override=parent_relay)
    if (
        len(arguments) == 7
        and arguments[3] == "--client-pid"
        and arguments[4].isdigit()
        and arguments[5] == "--client-tty"
    ):
        return send_command(
            arguments[1],
            arguments[2],
            int(arguments[4]),
            arguments[6],
            parent_override=parent_relay,
        )
    return FALLBACK


def dispatch(
    arguments: list[str],
    parent_relay: str,
    environment: dict[str, str],
    send_command: Callable[..., int],
    commit_command: Callable[..., int],
) -> int:
    """Dispatch one validated hot request and preserve public exit semantics."""

    if not arguments:
        return FALLBACK
    if arguments[0] == "warm" and len(arguments) == 1:
        return 0
    if arguments[0] == "navigate":
        return _navigate(arguments, environment)
    if arguments[0] == "commit":
        return _commit(arguments, environment, commit_command)
    if arguments[0] == "send":
        return _send(arguments, parent_relay, send_command)
    return FALLBACK


class _ThreadingUnixServer(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    """Serve independent client gestures without delaying unrelated terminals."""

    daemon_threads = True
    # Key repeat and several terminals can legitimately deliver a short burst
    # together. The stdlib default of five would turn excess local connects
    # into dropped gestures before worker threads even see them.
    request_queue_size = 64

    def __init__(
        self,
        socket_path: str,
        idle_seconds: float,
        dispatcher: Callable[[list[str], str, dict[str, str]], int],
    ):
        super().__init__(socket_path, _Handler)
        self.dispatcher = dispatcher
        self.idle_seconds = idle_seconds
        self.last_request = time.monotonic()
        self.activity = threading.Condition()
        self.active_requests = 0
        self.stopping = threading.Event()
        # ``handle_request`` uses this timeout to re-check the idle deadline
        # even when no terminal has sent a gesture recently.
        self.timeout = min(0.25, idle_seconds)

    def begin_activity(self) -> None:
        """Pin the service lifetime while one accepted request is in flight."""

        with self.activity:
            self.active_requests += 1
            self.last_request = time.monotonic()

    def end_activity(self) -> None:
        """Release a request and start its idle lifetime after the reply."""

        with self.activity:
            self.active_requests -= 1
            self.last_request = time.monotonic()
            self.activity.notify_all()

    def idle_expired(self) -> bool:
        """Return whether no request can be interrupted by idle retirement."""

        with self.activity:
            return self.active_requests == 0 and (
                time.monotonic() - self.last_request >= self.idle_seconds
            )

    def has_active_requests(self) -> bool:
        """Return whether shutdown must keep accepting dependent requests."""

        with self.activity:
            return self.active_requests != 0

    def process_request(self, request, client_address) -> None:
        """Pin activity before the worker thread can race the idle check."""

        self.begin_activity()
        try:
            super().process_request(request, client_address)
        except BaseException:
            self.end_activity()
            raise

    def process_request_thread(self, request, client_address) -> None:
        """Release the activity pin only after the complete request finishes."""

        try:
            super().process_request_thread(request, client_address)
        finally:
            self.end_activity()


class _Handler(socketserver.StreamRequestHandler):
    """Decode one bounded HTTP request carrying NUL-delimited argv fields."""

    def _reply(self, status: int) -> None:
        body = str(status).encode("ascii")
        self.wfile.write(
            b"HTTP/1.1 200 OK\r\n"
            + f"Content-Length: {len(body)}\r\n".encode("ascii")
            + b"Content-Type: text/plain\r\nConnection: close\r\n\r\n"
            + body
        )

    def handle(self) -> None:
        self.connection.settimeout(3)
        total = 0
        content_length = None
        try:
            request = self.rfile.readline(MAX_HEADER_BYTES + 1)
            total += len(request)
            if len(request) > MAX_HEADER_BYTES or request != b"POST /v1 HTTP/1.1\r\n":
                return
            while True:
                line = self.rfile.readline(MAX_HEADER_BYTES - total + 1)
                total += len(line)
                if total > MAX_HEADER_BYTES:
                    return
                if line == b"\r\n":
                    break
                if not line:
                    return
                name, separator, value = line.partition(b":")
                if separator and name.lower() == b"content-length":
                    raw = value.strip()
                    if not raw.isdigit():
                        return
                    content_length = int(raw)
            if content_length is None or content_length > MAX_BODY_BYTES:
                return
            body = self.rfile.read(content_length)
            if len(body) != content_length or not body.endswith(b"\0"):
                return
            fields = body[:-1].split(b"\0")
            decoded = [field.decode("utf-8") for field in fields]
        except (OSError, UnicodeError, ValueError):
            return

        if len(decoded) < 10 or decoded[0] != "termnav-hot-v1":
            return
        if decoded[1] != PROVIDER_ROOT:
            # One user may run an installed provider and a development checkout
            # simultaneously. Never execute a gesture through the other root's
            # already-loaded Python modules; the caller safely uses its parser.
            self._reply(FALLBACK)
            return
        parent_relay = decoded[2]
        environment = os.environ.copy()
        environment["PATH"] = decoded[3]
        for name, value in zip(
            (
                "HOME",
                "XDG_STATE_HOME",
                "TERMNAV_VSCODE_CURL",
                "TERMNAV_VSCODE_FALLBACK_BACKEND",
                "VSCODE_MCP_PORT",
            ),
            decoded[4:9],
            strict=True,
        ):
            if value:
                environment[name] = value
            else:
                environment.pop(name, None)
        arguments = decoded[9:]
        if arguments == ["stop"]:
            self._reply(0)
            self.server.stopping.set()
            return
        self._reply(self.server.dispatcher(arguments, parent_relay, environment))


def _live_socket(path: Path) -> bool:
    """Return whether an existing path accepts a bounded local connection."""

    try:
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(0.1)
            return client.connect_ex(str(path)) == 0
    except OSError:
        return False


def _prepare_parent(path: Path) -> bool:
    """Create and validate the owner-only directory for service state."""

    try:
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        parent_info = path.parent.lstat()
    except OSError:
        return False
    if not stat.S_ISDIR(parent_info.st_mode) or parent_info.st_uid != os.getuid():
        return False
    try:
        os.chmod(path.parent, 0o700)
    except OSError:
        return False
    return True


def _startup_lock(path: Path) -> int | None:
    """Serialize stale recovery and bind without trusting a symlinked lock."""

    flags = os.O_CREAT | os.O_RDWR | getattr(os, "O_CLOEXEC", 0)
    flags |= getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path.with_suffix(".lock"), flags, 0o600)
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
            os.close(descriptor)
            return None
        os.fchmod(descriptor, 0o600)
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        return descriptor
    except OSError:
        try:
            os.close(descriptor)
        except (OSError, UnboundLocalError):
            pass
        return None


def _prepare_socket(path: Path) -> str:
    """Return ready, live, or unsafe for the locked service socket path."""

    try:
        info = path.lstat()
    except FileNotFoundError:
        return "ready"
    if not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.getuid():
        return "unsafe"
    if _live_socket(path):
        return "live"
    path.unlink()
    return "ready"


def _socket_identity(path: Path) -> tuple[int, int] | None:
    """Return the inode identity used to protect a replacement service."""

    try:
        info = path.lstat()
    except OSError:
        return None
    if not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.getuid():
        return None
    return info.st_dev, info.st_ino


def _unlink_socket(path: Path, identity: tuple[int, int] | None) -> None:
    """Remove only the socket inode created by this service instance."""

    if identity is None or _socket_identity(path) != identity:
        return
    try:
        path.unlink()
    except FileNotFoundError:
        pass


def _maintain(stopped: threading.Event, sweep_command: Callable[[], int], interval: float) -> None:
    """Sweep abandoned forwards periodically without blocking request admission."""

    while not stopped.is_set():
        sweep_command()
        if stopped.wait(interval):
            break


def serve_hot(
    socket_path: str,
    send_command: Callable[..., int],
    commit_command: Callable[..., int],
    sweep_command: Callable[[], int],
    *,
    detach: bool = False,
) -> int:
    """Serve hot-path requests until stopped or idle for a bounded period."""

    if detach:
        # Some managed Python launchers remain as a supervisor while their
        # interpreter child is alive. Forking after interpreter startup lets
        # that launcher and the short shell wrapper both exit, leaving only
        # the process that actually serves navigation requests.
        pid = os.fork()
        if pid > 0:
            return 0
        os.setsid()
        os.chdir("/")

    idle_override = (
        os.environ.get("TERMNAV_TEST_HOT_IDLE_SECONDS")
        if os.environ.get("REPO_TEST") == "1"
        else None
    )
    try:
        idle_seconds = float(idle_override or DEFAULT_IDLE_SECONDS)
    except ValueError:
        idle_seconds = DEFAULT_IDLE_SECONDS
    if idle_seconds <= 0:
        idle_seconds = DEFAULT_IDLE_SECONDS
    sweep_override = (
        os.environ.get("TERMNAV_TEST_HOT_SWEEP_SECONDS")
        if os.environ.get("REPO_TEST") == "1"
        else None
    )
    try:
        sweep_seconds = float(sweep_override or DEFAULT_SWEEP_SECONDS)
    except ValueError:
        sweep_seconds = DEFAULT_SWEEP_SECONDS
    if sweep_seconds <= 0:
        sweep_seconds = DEFAULT_SWEEP_SECONDS

    def dispatcher(arguments: list[str], parent_relay: str, environment: dict[str, str]) -> int:
        return dispatch(arguments, parent_relay, environment, send_command, commit_command)

    path = Path(socket_path)
    if not path.is_absolute() or not _prepare_parent(path):
        return 1
    lock = _startup_lock(path)
    if lock is None:
        return 1
    server = None
    identity = None
    try:
        state = _prepare_socket(path)
        if state == "live":
            return 0
        if state != "ready":
            return 1
        try:
            # Bind, listen, and restrict permissions under one startup lock.
            # A concurrent launcher can observe only the prior live service or
            # this completely initialized replacement, never a bound-only gap.
            server = _ThreadingUnixServer(str(path), idle_seconds, dispatcher)
            identity = _socket_identity(path)
            os.chmod(path, 0o600)
        except OSError:
            if server is not None:
                server.server_close()
                _unlink_socket(path, identity)
            # A process that did not honor our lock can still win the bind.
            # Treat only a verifiably live replacement as successful startup.
            return 0 if _live_socket(path) else 1
    finally:
        fcntl.flock(lock, fcntl.LOCK_UN)
        os.close(lock)

    maintenance_stopped = None
    try:
        maintenance_stopped = threading.Event()
        # Cleanup begins immediately but never occupies the sole accept loop.
        # Even a slow probe of abandoned SSH sockets cannot delay the first
        # navigation gesture or a dependent commit connection.
        maintenance = threading.Thread(
            target=_maintain,
            args=(maintenance_stopped, sweep_command, sweep_seconds),
            name="termnav-sweep",
            daemon=True,
        )
        maintenance.start()
        # A user-scoped service should stay warm across normal terminal use,
        # but a deleted test runtime or abandoned login must not leave an
        # unreachable Python process behind forever. ``handle_request`` keeps
        # request threads concurrent while this owner loop enforces both the
        # explicit stop event and a generous idle lifetime.
        while True:
            server.handle_request()
            if server.stopping.is_set():
                # A navigate request may need a later commit connection before
                # it can finish. Continue accepting while any pre-stop request
                # remains active; otherwise shutdown itself would break the
                # atomic cross-terminal gesture it is trying to preserve.
                if not server.has_active_requests():
                    break
            elif server.idle_expired():
                break
    finally:
        if maintenance_stopped is not None:
            maintenance_stopped.set()
        server.server_close()
        _unlink_socket(path, identity)
    return 0
