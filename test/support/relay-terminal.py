#!/usr/bin/env python3
"""Exercise Termnav's commit barrier through real tmux clients and PTYs."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import pty
import select
import shlex
import signal
import socket
import subprocess
import tempfile
import threading
import time
import unittest

COMMIT_KEY = b"\x1b[777009u"
COMMIT_QUERY = b"\x1b[?2004$p"
DECRQM_RESPONSES = tuple(f"\x1b[?2004;{state}$y".encode() for state in range(5))
SENTINEL_KEY = b"\x07"
MIXED_PEER_TIMEOUT = 10.0


def wait_for(getter, description: str, timeout: float = 4.0):
    """Poll fresh state until getter returns a truthy value or explain timeout."""
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = getter()
        if last:
            return last
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {description}; last state: {last!r}")


class TerminalClient:
    """A controlling PTY that emulates only the terminal replies tmux needs."""

    def __init__(self, harness: RelayHarness, tmux_socket: str, session: str):
        self.harness = harness
        self.tmux_socket = tmux_socket
        self.session = session
        self.pid, self.master = pty.fork()
        if self.pid == 0:
            os.environ.update(harness.environment)
            os.environ["TERM"] = "xterm-256color"
            os.execvp(
                "tmux",
                ["tmux", "-S", tmux_socket, "attach-session", "-t", session],
            )

        listing = wait_for(self._identity, "tmux client attachment")
        self.tty, listed_pid = listing.split()
        if int(listed_pid) != self.pid:
            raise AssertionError(f"tmux reported client pid {listed_pid}, expected {self.pid}")
        self._finish_initialization()
        try:
            fd = os.open(self.tty, os.O_WRONLY | os.O_NOCTTY | os.O_APPEND)
        except PermissionError as exc:
            self.close()
            raise unittest.SkipTest(
                "platform forbids reopening the synthetic tmux client PTY"
            ) from exc
        else:
            os.close(fd)

    def _identity(self) -> str | None:
        result = self.harness.tmux(
            self.tmux_socket,
            "list-clients",
            "-F",
            "#{client_tty} #{client_pid}",
            check=False,
        )
        for line in result.stdout.splitlines():
            fields = line.split()
            if len(fields) == 2 and fields[1] == str(self.pid):
                return line
        return None

    def _read(self, timeout: float) -> bytes:
        ready, _, _ = select.select([self.master], [], [], timeout)
        if not ready:
            return b""
        return os.read(self.master, 65536)

    def _answer_tmux_queries(self, payload: bytes) -> None:
        # tmux uses this private DSR handshake to finish terminal capability
        # discovery. Answering it makes the PTY behave like a real terminal
        # before the tests send any user keys.
        if b"\x1b[?996n" in payload:
            os.write(self.master, b"\x1b[?997;1n")

    def _finish_initialization(self) -> None:
        # New tmux versions probe for terminal capability passthrough with DSR
        # ?996, while older releases do not. The bound sentinel is the portable
        # readiness contract; answer capability probes while waiting for it.
        payload = bytearray()
        self.harness.sentinel.unlink(missing_ok=True)
        os.write(self.master, SENTINEL_KEY)
        deadline = time.monotonic() + 4.0
        while time.monotonic() < deadline:
            chunk = self._read(0.05)
            if chunk:
                payload.extend(chunk)
                self._answer_tmux_queries(chunk)
            if self.harness.sentinel.exists():
                break
        else:
            raise AssertionError(
                "timed out waiting for tmux input readiness sentinel; "
                f"terminal output: {bytes(payload)!r}"
            )
        self.drain()

    def drain(self) -> bytes:
        """Drain currently available output without waiting on wall-clock guesses."""
        payload = bytearray()
        while True:
            chunk = self._read(0)
            if not chunk:
                return bytes(payload)
            payload.extend(chunk)
            self._answer_tmux_queries(chunk)

    def read_until(self, needle: bytes, timeout: float = 4.0) -> bytes:
        payload = bytearray()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            chunk = self._read(min(0.05, max(0.0, deadline - time.monotonic())))
            if not chunk:
                continue
            payload.extend(chunk)
            self._answer_tmux_queries(chunk)
            if needle in payload:
                return bytes(payload)
        raise AssertionError(
            f"timed out waiting for terminal bytes {needle!r}; got {bytes(payload)!r}"
        )

    def read_until_or(self, needle: bytes, predicate, timeout: float = 4.0):
        """Wait until output contains needle or an independent condition wins."""
        payload = bytearray()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return bytes(payload), False
            chunk = self._read(min(0.05, max(0.0, deadline - time.monotonic())))
            if not chunk:
                continue
            payload.extend(chunk)
            self._answer_tmux_queries(chunk)
            if needle in payload:
                return bytes(payload), True
        raise AssertionError(
            f"timed out waiting for {needle!r} or alternate condition; got {bytes(payload)!r}"
        )

    def send(self, payload: bytes) -> None:
        os.write(self.master, payload)

    def focus(self) -> None:
        """Tell tmux this synthetic terminal is the focused attachment."""

        self.send(b"\x1b[I")
        self.drain()

    def blur(self) -> None:
        """Tell tmux this synthetic terminal is no longer focused."""

        self.send(b"\x1b[O")
        self.drain()

    def synchronize_input(self) -> None:
        """Prove all earlier client input was consumed using a bound sentinel."""
        self.harness.sentinel.unlink(missing_ok=True)
        self.send(SENTINEL_KEY)
        wait_for(self.harness.sentinel.exists, "tmux input sentinel")

    def close(self) -> None:
        try:
            os.kill(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 2.0
        while time.monotonic() < deadline:
            waited, _ = os.waitpid(self.pid, os.WNOHANG)
            if waited == self.pid:
                break
            time.sleep(0.02)
        else:
            try:
                os.kill(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(self.pid, 0)
        os.close(self.master)


class RelayHarness:
    """Own isolated tmux servers, relay processes, and terminal PTYs."""

    def __init__(self, relay: str, python_relay: str, termnav: str):
        self.relay = str(pathlib.Path(relay).resolve())
        self.python_relay = str(pathlib.Path(python_relay).resolve())
        self.termnav = str(pathlib.Path(termnav).resolve())
        # macOS AF_UNIX paths are limited to 103 non-NUL bytes. Its default
        # /var/folders TMPDIR plus descriptive mixed-version socket names can
        # exceed that before the test reaches any relay behavior.
        short_root = "/tmp" if os.path.isdir("/tmp") and os.access("/tmp", os.W_OK) else None
        self.temporary = tempfile.TemporaryDirectory(prefix="tnrt-", dir=short_root)
        self.root = pathlib.Path(self.temporary.name)
        self.runtime = self.root / "runtime"
        self.runtime.mkdir(mode=0o700)
        self.environment = os.environ.copy()
        self.environment["XDG_RUNTIME_DIR"] = str(self.runtime)
        self.sentinel = self.root / "input-ready"
        self.tmux_sockets: list[str] = []
        self.relays: list[subprocess.Popen] = []
        self.relay_logs: list[pathlib.Path] = []
        self.terminals: list[TerminalClient] = []

    def close(self) -> None:
        for relay in reversed(self.relays):
            relay.terminate()
            try:
                relay.wait(timeout=2)
            except subprocess.TimeoutExpired:
                relay.kill()
                relay.wait(timeout=2)
        for terminal in reversed(self.terminals):
            terminal.close()
        for tmux_socket in reversed(self.tmux_sockets):
            self.tmux(tmux_socket, "kill-server", check=False)
        self.temporary.cleanup()

    def tmux(
        self, tmux_socket: str, *arguments: str, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["tmux", "-S", tmux_socket, *arguments],
            env=self.environment,
            text=True,
            capture_output=True,
            check=check,
        )

    def new_server(self, name: str, session: str, command: str) -> str:
        tmux_socket = str(self.root / f"{name}.sock")
        self.tmux_sockets.append(tmux_socket)
        self.tmux(
            tmux_socket,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            session,
            "-n",
            "first",
            command,
        )
        return tmux_socket

    def configure_commits(self, tmux_socket: str, terminal_replies: bool) -> None:
        self.configure_commits_with(tmux_socket, terminal_replies, self.relay)

    def configure_commits_with(self, tmux_socket: str, terminal_replies: bool, relay: str) -> None:
        commit = (
            f"{shlex.quote(relay)} commit "
            "--tmux-socket #{q:socket_path} "
            "--client-tty #{q:client_tty} --client-pid #{client_pid} "
            "--client-created #{client_created}"
        )
        self.tmux(tmux_socket, "set-option", "-s", "user-keys[8]", COMMIT_KEY.decode())
        self.tmux(
            tmux_socket,
            "bind-key",
            "-n",
            "User8",
            "run-shell",
            commit,
        )
        if terminal_replies:
            for state, response in enumerate(DECRQM_RESPONSES):
                key = 9 + state
                self.tmux(
                    tmux_socket,
                    "set-option",
                    "-s",
                    f"user-keys[{key}]",
                    response.decode(),
                )
                self.tmux(
                    tmux_socket,
                    "bind-key",
                    "-n",
                    f"User{key}",
                    "run-shell",
                    f"{commit} --passthrough-decrqm {state} --pane #{{pane_id}}",
                )
            self.tmux(
                tmux_socket,
                "bind-key",
                "-n",
                "C-g",
                "run-shell",
                f"touch {shlex.quote(str(self.sentinel))}",
            )

    def attach(self, tmux_socket: str, session: str) -> TerminalClient:
        terminal = TerminalClient(self, tmux_socket, session)
        self.terminals.append(terminal)
        return terminal

    def client_identity(self, tmux_socket: str) -> tuple[str, int]:
        line = wait_for(
            lambda: (
                self.tmux(
                    tmux_socket,
                    "list-clients",
                    "-F",
                    "#{client_tty} #{client_pid}",
                    check=False,
                ).stdout.strip()
                or None
            ),
            "nested tmux client",
        )
        tty, pid = line.splitlines()[0].split()
        return tty, int(pid)

    def navigation_ready(self, tmux_socket: str, pane: str, pid: int) -> bool:
        """Return whether tmux exposes every predicate used by pane routing."""

        clients = self.tmux(
            tmux_socket,
            "list-clients",
            "-F",
            "|".join(
                (
                    "#{client_activity}",
                    "#{client_pid}",
                    "#{client_tty}",
                    "#{client_termtype}",
                    "#{session_id}",
                    "#{pane_id}",
                    "#{client_flags}",
                    "#{client_control_mode}",
                    "#{client_created}",
                )
            ),
            check=False,
        )
        if clients.returncode != 0:
            return False
        routes = []
        for line in clients.stdout.splitlines():
            fields = line.split("|")
            if len(fields) != 9:
                continue
            (
                activity,
                client_pid,
                tty,
                _termtype,
                session,
                client_pane,
                _,
                control,
                created,
            ) = fields
            if (
                activity.isdigit()
                and client_pid == str(pid)
                and tty
                and session
                and client_pane == pane
                and control == "0"
                and created.isdigit()
            ):
                routes.append(fields)
        if len(routes) != 1:
            return False

        ownership = self.tmux(
            tmux_socket,
            "display-message",
            "-p",
            "-t",
            pane,
            "#{window_active_clients}|#{pane_active}",
            check=False,
        )
        if ownership.returncode != 0:
            return False
        active_clients, separator, pane_active = ownership.stdout.strip().partition("|")
        return (
            separator == "|"
            and active_clients.isdigit()
            and int(active_clients) > 0
            and pane_active == "1"
        )

    def wait_for_nested_client(
        self, parent_socket: str, parent_pane: str, child_socket: str, child_pane: str
    ) -> tuple[str, int]:
        """Prove a newly attached child tmux is consuming pane input."""

        identity = self.client_identity(child_socket)
        # `list-clients` becomes visible before a new tmux client has necessarily
        # completed terminal setup and entered its input loop. Sending the test
        # sentinel through the parent pane exercises the same boundary as a real
        # key chord; waiting for the child binding avoids racing the first relay
        # request on slower macOS and container runners.
        self.tmux(
            child_socket,
            "bind-key",
            "-n",
            "C-g",
            "run-shell",
            f"touch {shlex.quote(str(self.sentinel))}",
        )
        self.sentinel.unlink(missing_ok=True)
        self.tmux(parent_socket, "send-keys", "-t", parent_pane, "C-g")
        wait_for(self.sentinel.exists, "nested tmux input readiness")
        wait_for(
            lambda: self.navigation_ready(child_socket, child_pane, identity[1]),
            "nested tmux navigation readiness",
        )
        return identity

    def focused_clients(self, tmux_socket: str) -> set[int]:
        """Return client PIDs whose terminal-focus flag is current."""

        listing = self.tmux(
            tmux_socket,
            "list-clients",
            "-F",
            "#{client_pid} #{client_flags}",
            check=False,
        ).stdout.splitlines()
        return {
            int(pid)
            for line in listing
            if len(fields := line.split(maxsplit=1)) == 2
            for pid, flags in (fields,)
            if pid.isdigit() and "focused" in flags.split(",")
        }

    def start_relay(
        self,
        name: str,
        tmux_socket: str,
        pane: str,
        relay: str | None = None,
        parent: str | None = None,
    ) -> str:
        relay = relay or self.relay
        relay_socket = str(self.root / f"{name}-relay.sock")
        environment = self.environment.copy()
        relay_log = self.root / f"{name}-relay.log"
        self.relay_logs.append(relay_log)
        environment.update(
            {
                "TMUX": f"{tmux_socket},0,0",
                "TMUX_PANE": pane,
                "TERMNAV_RELAY_LOG": str(relay_log),
            }
        )
        if parent is None:
            environment.pop("TERMNAV_PARENT_RELAY", None)
        else:
            # The frozen Python peer intentionally owns no process-ancestry
            # policy. Give every test peer the already-known adjacent socket;
            # Rust production tests cover discovering that same value from the
            # selected tmux client independently.
            environment["TERMNAV_PARENT_RELAY"] = parent
        process = subprocess.Popen(
            [relay, "serve", "--socket", relay_socket],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.relays.append(process)

        def ready() -> bool:
            if not pathlib.Path(relay_socket).exists():
                return False
            try:
                response = self.protocol(
                    relay_socket,
                    {"v": 2, "op": "readiness-probe"},
                    timeout=1.0,
                )
                return response == {"v": 2, "result": "error"}
            except (AssertionError, OSError, ValueError):
                return False

        # A successful connect proves only that listen(2) completed. Require a
        # complete request/reply turn so the first behavioral request cannot
        # race the server's accept loop on slower macOS and Android workers.
        wait_for(ready, f"relay request loop {name}")
        return relay_socket

    def start_send(
        self,
        relay_socket: str,
        action: str,
        direction: str,
        relay: str | None = None,
    ) -> subprocess.Popen[str]:
        """Start a request whose terminal response may be driven by the test."""

        environment = self.environment.copy()
        environment["TERMNAV_PARENT_RELAY"] = relay_socket
        command = relay or self.relay
        command_action = action
        if command == self.python_relay:
            # The frozen peer preserves protocol-v2's historical CLI only so
            # the mixed-version tests can prove rolling fleet interoperability.
            # Test callers still speak the canonical Rust public vocabulary,
            # keeping those wire-only names out of new integration surfaces.
            command_action = {
                "pane-select": "pane",
                "tab-select": "window",
                "tab-move": "move",
            }[action]
        return subprocess.Popen(
            [command, "send", command_action, direction],
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

    def finish_send(self, process: subprocess.Popen[str], action: str, direction: str) -> None:
        """Require one asynchronous request to finish successfully."""

        stdout, stderr = process.communicate(timeout=10)
        if process.returncode != 0:
            raise AssertionError(
                f"relay send {action} {direction} exited {process.returncode}: "
                f"{stdout}{stderr}\n{self.diagnostics()}"
            )

    def diagnostics(self) -> str:
        """Return relay-side failure detail without consuming process pipes."""

        diagnostics = []
        for tmux_socket in self.tmux_sockets:
            clients = self.tmux(
                tmux_socket,
                "list-clients",
                "-F",
                "#{client_tty}|#{client_pid}|#{client_created}|#{client_flags}",
                check=False,
            )
            diagnostics.append(
                f"{pathlib.Path(tmux_socket).name} clients: "
                f"returncode={clients.returncode}, stdout={clients.stdout!r}, "
                f"stderr={clients.stderr!r}"
            )
        for state in sorted(self.runtime.rglob("*.json")):
            try:
                body = state.read_text(encoding="utf-8")
            except OSError as error:
                body = f"<unreadable: {error!r}>"
            diagnostics.append(f"{state.relative_to(self.runtime)}:\n{body}")
        for relay_log in self.relay_logs:
            if relay_log.exists():
                diagnostics.append(f"{relay_log.name}:\n{relay_log.read_text(encoding='utf-8')}")
        return "\n".join(diagnostics)

    def send(self, relay_socket: str, action: str, direction: str) -> None:
        """Run a request that completes without a test-driven terminal reply."""

        self.finish_send(self.start_send(relay_socket, action, direction), action, direction)

    def protocol(self, relay_socket: str, request: dict, timeout: float = 4.0) -> dict:
        """Exchange one raw protocol object and require its complete reply."""

        payload = json.dumps(request, separators=(",", ":")).encode() + b"\n"
        client = socket.socket(socket.AF_UNIX)
        client.settimeout(timeout)
        client.connect(relay_socket)
        client.sendall(payload)
        chunks = bytearray()
        while not chunks.endswith(b"\n"):
            chunk = client.recv(513 - len(chunks))
            if not chunk:
                raise AssertionError(f"relay closed before replying to {request!r}")
            chunks.extend(chunk)
        client.close()
        return json.loads(chunks)

    def start_ignored_reply(self, relay_socket: str, request: dict) -> socket.socket:
        """Send a request while deliberately leaving its reply unread."""

        payload = json.dumps(request, separators=(",", ":")).encode() + b"\n"
        client = socket.socket(socket.AF_UNIX)
        client.settimeout(4)
        client.connect(relay_socket)
        client.sendall(payload)
        # Keep the read side open until the test observes durable preparation.
        # Closing immediately can make macOS report a peer reset before the
        # server has consumed already-written bytes, which tests transport
        # teardown rather than the intended lost-reply transaction semantics.
        client.shutdown(socket.SHUT_WR)
        return client

    def relay_for(self, implementation: str) -> str:
        """Resolve one named peer without hiding the topology in each test."""

        if implementation == "rust":
            return self.relay
        if implementation == "python":
            return self.python_relay
        raise AssertionError(f"unknown relay implementation: {implementation}")

    def active_pane(self, tmux_socket: str, session: str) -> str:
        """Return the pane carrying tmux's authoritative active flag."""
        listing = self.tmux(
            tmux_socket,
            "list-panes",
            "-t",
            session,
            "-F",
            "#{pane_id} #{pane_active}",
        ).stdout.splitlines()
        active = [line.split()[0] for line in listing if line.endswith(" 1")]
        if len(active) != 1:
            raise AssertionError(f"expected one active pane, got {listing!r}")
        return active[0]


class RelayTerminalTest(unittest.TestCase):
    relay: str
    python_relay: str
    termnav: str

    def setUp(self) -> None:
        self.harness = RelayHarness(self.relay, self.python_relay, self.termnav)

    def tearDown(self) -> None:
        self.harness.close()

    def top_level(
        self, *, panes: int = 1, windows: int = 1, current_window: int = 0
    ) -> tuple[str, str, list[str], TerminalClient, str]:
        tmux_socket = self.harness.new_server("top", "top", "cat")
        pane_ids = [
            self.harness.tmux(
                tmux_socket, "display-message", "-p", "-t", "top:0", "#{pane_id}"
            ).stdout.strip()
        ]
        for _ in range(1, panes):
            pane_ids.append(
                self.harness.tmux(
                    tmux_socket,
                    "split-window",
                    "-h",
                    "-d",
                    "-t",
                    "top:0",
                    "-P",
                    "-F",
                    "#{pane_id}",
                    "cat",
                ).stdout.strip()
            )
        for index in range(1, windows):
            self.harness.tmux(
                tmux_socket,
                "new-window",
                "-d",
                "-t",
                f"top:{index}",
                "-n",
                f"window-{index}",
                "cat",
            )
        self.harness.tmux(tmux_socket, "select-window", "-t", f"top:{current_window}")
        source_pane = self.harness.tmux(
            tmux_socket,
            "display-message",
            "-p",
            "-t",
            f"top:{current_window}",
            "#{pane_id}",
        ).stdout.strip()
        if current_window == 0:
            self.harness.tmux(tmux_socket, "select-pane", "-t", pane_ids[0])
            source_pane = pane_ids[0]
        self.harness.configure_commits(tmux_socket, terminal_replies=True)
        terminal = self.harness.attach(tmux_socket, "top")
        wait_for(
            lambda: self.harness.navigation_ready(tmux_socket, source_pane, terminal.pid),
            "top-level tmux navigation readiness",
        )
        relay_socket = self.harness.start_relay(
            "top",
            tmux_socket,
            source_pane,
        )
        return tmux_socket, source_pane, pane_ids, terminal, relay_socket

    def mixed_topology(self, prefix: str, implementations: tuple[str, str, str]):
        """Build inner-to-outer relays using the requested peer sequence."""

        inner_socket = self.harness.new_server(f"{prefix}-inner", "inner", "cat")
        inner_left = self.harness.tmux(
            inner_socket, "display-message", "-p", "-t", "inner", "#{pane_id}"
        ).stdout.strip()
        inner_right = self.harness.tmux(
            inner_socket,
            "split-window",
            "-h",
            "-d",
            "-t",
            "inner",
            "-P",
            "-F",
            "#{pane_id}",
            "cat",
        ).stdout.strip()
        self.harness.tmux(inner_socket, "select-pane", "-t", inner_left)

        middle_socket = self.harness.new_server(f"{prefix}-middle", "middle", "cat")
        middle_pane = self.harness.tmux(
            middle_socket, "display-message", "-p", "-t", "middle", "#{pane_id}"
        ).stdout.strip()
        middle_right = self.harness.tmux(
            middle_socket,
            "split-window",
            "-h",
            "-d",
            "-t",
            middle_pane,
            "-P",
            "-F",
            "#{pane_id}",
            "cat",
        ).stdout.strip()
        self.harness.tmux(middle_socket, "select-pane", "-t", middle_pane)
        outer_socket = self.harness.new_server(f"{prefix}-outer", "outer", "cat")
        outer_pane = self.harness.tmux(
            outer_socket, "display-message", "-p", "-t", "outer", "#{pane_id}"
        ).stdout.strip()

        for tmux_socket, terminal_replies, implementation in (
            (inner_socket, False, implementations[0]),
            (middle_socket, False, implementations[1]),
            (outer_socket, True, implementations[2]),
        ):
            self.harness.configure_commits_with(
                tmux_socket,
                terminal_replies,
                self.harness.relay_for(implementation),
            )
        terminal = self.harness.attach(outer_socket, "outer")

        outer_relay = self.harness.start_relay(
            f"{prefix}-outer",
            outer_socket,
            outer_pane,
            self.harness.relay_for(implementations[2]),
        )
        self.harness.tmux(
            outer_socket,
            "respawn-pane",
            "-k",
            "-t",
            outer_pane,
            f"exec env TERMNAV_PARENT_RELAY={shlex.quote(outer_relay)} "
            f"tmux -S {shlex.quote(middle_socket)} attach-session -t middle",
        )
        self.harness.wait_for_nested_client(outer_socket, outer_pane, middle_socket, middle_pane)
        middle_relay = self.harness.start_relay(
            f"{prefix}-middle",
            middle_socket,
            middle_pane,
            self.harness.relay_for(implementations[1]),
            outer_relay,
        )
        self.harness.tmux(
            middle_socket,
            "respawn-pane",
            "-k",
            "-t",
            middle_pane,
            f"exec env TERMNAV_PARENT_RELAY={shlex.quote(middle_relay)} "
            f"tmux -S {shlex.quote(inner_socket)} attach-session -t inner",
        )
        self.harness.wait_for_nested_client(middle_socket, middle_pane, inner_socket, inner_left)
        inner_relay = self.harness.start_relay(
            f"{prefix}-inner",
            inner_socket,
            inner_left,
            self.harness.relay_for(implementations[0]),
            middle_relay,
        )
        if implementations[0] == "rust":
            target = (inner_socket, "inner", inner_right)
        else:
            target = (middle_socket, "middle", middle_right)
        return target, terminal, inner_relay

    def test_mixed_version_three_level_paths_commit_in_both_directions(self) -> None:
        for index, implementations in enumerate(
            (("rust", "python", "rust"), ("python", "rust", "python"))
        ):
            with self.subTest(implementations=implementations):
                target, terminal, relay_socket = self.mixed_topology(
                    f"mixed-success-{index}", implementations
                )
                request = self.harness.start_send(
                    relay_socket,
                    "pane-select",
                    "right",
                    self.harness.relay_for(implementations[0]),
                )
                try:
                    terminal.read_until(COMMIT_QUERY, timeout=MIXED_PEER_TIMEOUT)
                except AssertionError as error:
                    raise AssertionError(f"{error}\n{self.harness.diagnostics()}") from error
                terminal.send(DECRQM_RESPONSES[1])
                self.harness.finish_send(request, "pane-select", "right")
                target_socket, target_session, target_pane = target

                # Poll callbacks may outlive this subtest iteration, so bind the
                # exact mixed-version target instead of closing over loop state.
                def target_is_active(
                    target_socket: str = target_socket,
                    target_session: str = target_session,
                    target_pane: str = target_pane,
                ) -> bool:
                    return self.harness.active_pane(target_socket, target_session) == target_pane

                wait_for(target_is_active, "mixed-version nested commit")

    def test_mixed_version_lost_prepare_reply_can_be_aborted(self) -> None:
        for index, implementations in enumerate(
            (("rust", "python", "rust"), ("python", "rust", "python"))
        ):
            with self.subTest(implementations=implementations):
                _, _, relay_socket = self.mixed_topology(f"mixed-abort-{index}", implementations)
                nonce = f"{index + 1:012x}"
                request = {"v": 2, "op": "prepare-path", "nonce": nonce}
                ignored_reply = self.harness.start_ignored_reply(relay_socket, request)

                # Freeze the nonce for the same reason: a delayed poll must not
                # inspect state belonging to the next topology in the loop.
                def prepared_count(nonce: str = nonce) -> int:
                    return sum(
                        nonce in path.read_text(encoding="utf-8")
                        for path in self.harness.runtime.rglob("*.json")
                    )

                try:
                    wait_for(
                        lambda: prepared_count() == 3,
                        "three mixed prepared hops",
                        timeout=MIXED_PEER_TIMEOUT,
                    )
                except AssertionError as error:
                    raise AssertionError(f"{error}\n{self.harness.diagnostics()}") from error
                finally:
                    ignored_reply.close()
                reply = self.harness.protocol(
                    relay_socket,
                    {"v": 2, "op": "abort-path", "nonce": nonce},
                )
                self.assertEqual({"v": 2, "result": "aborted"}, reply)
                wait_for(lambda: prepared_count() == 0, "mixed abort cleanup")

    def test_one_shot_continuation_preserves_a_nested_navigation_burst(self) -> None:
        inner_socket = self.harness.new_server("burst-inner", "inner", "cat")
        inner_pane = self.harness.tmux(
            inner_socket, "display-message", "-p", "-t", "inner", "#{pane_id}"
        ).stdout.strip()
        outer_socket = self.harness.new_server("burst-outer", "outer", "cat")
        outer_nested = self.harness.tmux(
            outer_socket, "display-message", "-p", "-t", "outer", "#{pane_id}"
        ).stdout.strip()
        outer_middle = self.harness.tmux(
            outer_socket,
            "split-window",
            "-h",
            "-d",
            "-t",
            outer_nested,
            "-P",
            "-F",
            "#{pane_id}",
            "cat",
        ).stdout.strip()
        outer_right = self.harness.tmux(
            outer_socket,
            "split-window",
            "-h",
            "-d",
            "-t",
            outer_middle,
            "-P",
            "-F",
            "#{pane_id}",
            "cat",
        ).stdout.strip()
        self.harness.tmux(outer_socket, "select-pane", "-t", outer_nested)
        self.harness.configure_commits(outer_socket, terminal_replies=True)
        self.harness.attach(outer_socket, "outer")
        self.harness.tmux(
            outer_socket,
            "respawn-pane",
            "-k",
            "-t",
            outer_nested,
            f"exec tmux -S {shlex.quote(inner_socket)} attach-session -t inner",
        )
        self.harness.wait_for_nested_client(outer_socket, outer_nested, inner_socket, inner_pane)
        environment = self.harness.environment.copy()
        environment.update({"TMUX": f"{inner_socket},0,0", "TMUX_PANE": inner_pane})

        first = subprocess.run(
            [self.termnav, "navigate", "--emit-continuation", "pane-select", "right"],
            env=environment,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(0, first.returncode, first.stderr)
        continuation = first.stdout.strip()
        self.assertTrue(continuation.startswith("{"), continuation)

        # Resume immediately from the returned token. An intermediate tmux
        # polling subprocess can consume the deliberately short semantic TTL
        # on a loaded CI runner even though a real Neovim callback queues the
        # next chord as soon as the first job exits. Reaching the right pane
        # proves both the first move through `outer_middle` and the resumed
        # second move without testing machine scheduling speed.
        second = subprocess.run(
            [
                self.termnav,
                "navigate",
                "--emit-continuation",
                "pane-select",
                "right",
                "--continuation",
                continuation,
            ],
            env=environment,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(0, second.returncode, second.stderr)
        wait_for(
            lambda: self.harness.active_pane(outer_socket, "outer") == outer_right,
            "second nested burst step",
        )

    def test_all_decrqm_states_commit_pane_navigation(self) -> None:
        tmux_socket, left, panes, terminal, relay_socket = self.top_level(panes=2)
        right = panes[1]
        for state, response in enumerate(DECRQM_RESPONSES):
            with self.subTest(state=state):
                self.harness.tmux(tmux_socket, "select-pane", "-t", left)
                # Each DECRQM reply enters through the synthetic client's
                # input queue. Prove tmux consumed the previous reply before
                # starting the next transaction; otherwise a fast CI worker
                # can race client input processing and observe no next query.
                terminal.synchronize_input()
                terminal.drain()
                request = self.harness.start_send(relay_socket, "pane-select", "right")
                terminal.read_until(COMMIT_QUERY)
                active = self.harness.active_pane(tmux_socket, "top")
                self.assertEqual(left, active, "navigation committed before the reply")
                terminal.send(response)
                self.harness.finish_send(request, "pane-select", "right")
                wait_for(
                    lambda: self.harness.active_pane(tmux_socket, "top") == right,
                    f"pane commit for DECRQM state {state}",
                )

    def test_window_switch_commits_after_terminal_response(self) -> None:
        tmux_socket, _, _, terminal, relay_socket = self.top_level(windows=2)
        before = self.harness.tmux(
            tmux_socket, "display-message", "-p", "-t", "top", "#{window_id}"
        ).stdout.strip()
        request = self.harness.start_send(relay_socket, "tab-select", "next")
        terminal.read_until(COMMIT_QUERY)
        self.assertEqual(
            before,
            self.harness.tmux(
                tmux_socket, "display-message", "-p", "-t", "top", "#{window_id}"
            ).stdout.strip(),
        )
        terminal.send(DECRQM_RESPONSES[2])
        self.harness.finish_send(request, "tab-select", "next")
        wait_for(
            lambda: (
                self.harness.tmux(
                    tmux_socket, "display-message", "-p", "-t", "top", "#{window_id}"
                ).stdout.strip()
                != before
            ),
            "window switch commit",
        )

    def test_window_move_commits_after_terminal_response(self) -> None:
        tmux_socket, _, _, terminal, relay_socket = self.top_level(windows=3, current_window=1)
        before = self.harness.tmux(
            tmux_socket, "list-windows", "-t", "top", "-F", "#{window_name}"
        ).stdout.splitlines()
        request = self.harness.start_send(relay_socket, "tab-move", "left")
        terminal.read_until(COMMIT_QUERY)
        self.assertEqual(
            before,
            self.harness.tmux(
                tmux_socket, "list-windows", "-t", "top", "-F", "#{window_name}"
            ).stdout.splitlines(),
        )
        terminal.send(DECRQM_RESPONSES[3])
        self.harness.finish_send(request, "tab-move", "left")
        expected = [before[1], before[0], before[2]]
        wait_for(
            lambda: (
                self.harness.tmux(
                    tmux_socket, "list-windows", "-t", "top", "-F", "#{window_name}"
                ).stdout.splitlines()
                == expected
            ),
            "window move commit",
        )

    def test_one_window_tab_selection_reaches_the_outer_terminal(self) -> None:
        _, _, _, terminal, relay_socket = self.top_level()
        terminal.drain()

        self.harness.send(relay_socket, "tab-select", "next")

        payload = terminal.read_until(b"]1337;SetUserVar=TERMNAV_TAB_SELECT=")
        self.assertIn(b"\x1b]1337;SetUserVar=TERMNAV_TAB_SELECT=", payload)

    def test_relay_started_before_focus_switch_targets_the_live_client(self) -> None:
        tmux_socket, _, _, first, relay_socket = self.top_level()
        second = self.harness.attach(tmux_socket, "top")
        first.blur()
        second.focus()
        wait_for(
            lambda: self.harness.focused_clients(tmux_socket) == {second.pid},
            "second terminal focus",
        )
        first.drain()
        second.drain()

        self.harness.send(relay_socket, "tab-select", "next")

        second.read_until(b"]1337;SetUserVar=TERMNAV_TAB_SELECT=")
        self.assertNotIn(b"]1337;SetUserVar=TERMNAV_TAB_SELECT=", first.drain())

    def test_one_window_tab_selection_reaches_the_vscode_window_adapter(self) -> None:
        socket_path = self.harness.root / "vscode.sock"
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(socket_path))
        listener.listen(1)
        request = bytearray()

        def accept_request() -> None:
            connection, _ = listener.accept()
            with connection:
                while b"\r\n\r\n" not in request:
                    request.extend(connection.recv(4096))
                headers, _, body = request.partition(b"\r\n\r\n")
                length = 0
                for line in headers.split(b"\r\n"):
                    if line.lower().startswith(b"content-length:"):
                        length = int(line.split(b":", 1)[1])
                while len(body) < length:
                    request.extend(connection.recv(4096))
                    _, _, body = request.partition(b"\r\n\r\n")
                connection.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")

        server = threading.Thread(target=accept_request, daemon=True)
        server.start()
        token = "a" * 64
        self.harness.environment.update(
            {
                "TERM_PROGRAM": "vscode",
                "TERMNAV_VSCODE_SOCKET": str(socket_path),
                "TERMNAV_VSCODE_TOKEN": token,
            }
        )
        try:
            _, _, _, _, relay_socket = self.top_level()
            self.harness.send(relay_socket, "tab-select", "previous")
            server.join(timeout=4)
            self.assertFalse(server.is_alive(), "VS Code adapter request did not arrive")
        finally:
            listener.close()

        payload = bytes(request)
        self.assertIn(b"POST /switch-tab HTTP/1.1", payload)
        self.assertIn(f'{{"direction":"previous","token":"{token}"}}'.encode(), payload)

    def test_fragmented_response_waits_for_its_terminator(self) -> None:
        tmux_socket, left, panes, terminal, relay_socket = self.top_level(panes=2)
        right = panes[1]
        request = self.harness.start_send(relay_socket, "pane-select", "right")
        terminal.read_until(COMMIT_QUERY)
        terminal.send(b"\x1b[?2004;1")
        self.assertEqual(left, self.harness.active_pane(tmux_socket, "top"))
        self.assertTrue(
            list(self.harness.runtime.rglob("*.json")),
            "the armed directive must remain pending until the reply terminates",
        )
        terminal.send(b"$y")
        self.harness.finish_send(request, "pane-select", "right")
        wait_for(
            lambda: self.harness.active_pane(tmux_socket, "top") == right,
            "commit after the fragmented response terminator",
        )

    def test_stray_and_duplicate_responses_are_noops(self) -> None:
        tmux_socket, left, panes, terminal, relay_socket = self.top_level(panes=2)
        terminal.send(DECRQM_RESPONSES[1])
        terminal.synchronize_input()
        self.assertEqual(
            left,
            self.harness.active_pane(tmux_socket, "top"),
        )

        request = self.harness.start_send(relay_socket, "pane-select", "right")
        terminal.read_until(COMMIT_QUERY)
        terminal.send(DECRQM_RESPONSES[1] + DECRQM_RESPONSES[1])
        self.harness.finish_send(request, "pane-select", "right")
        terminal.synchronize_input()
        self.assertEqual(
            panes[1],
            self.harness.active_pane(tmux_socket, "top"),
        )

    def test_application_decrqm_response_is_preserved(self) -> None:
        tmux_socket, _, _, terminal, _ = self.top_level()
        reply_path = self.harness.root / "application-reply"
        application = ";".join(
            (
                "import os,pathlib,sys,tty",
                "tty.setraw(0)",
                f"sys.stdout.buffer.write({COMMIT_QUERY!r})",
                "sys.stdout.buffer.flush()",
                f"reply=os.read(0,{len(DECRQM_RESPONSES[0])})",
                f"pathlib.Path({str(reply_path)!r}).write_bytes(reply)",
                "os.read(0,1)",
            )
        )
        self.harness.tmux(
            tmux_socket,
            "respawn-pane",
            "-k",
            "-t",
            "top",
            f"python3 -c {shlex.quote(application)}",
        )
        try:
            payload, forwarded = terminal.read_until_or(
                COMMIT_QUERY, reply_path.exists, timeout=1.0
            )
        except AssertionError:
            self.skipTest("tmux neither virtualizes nor forwards application DECRQM")
        if forwarded:
            terminal.send(DECRQM_RESPONSES[2])
        wait_for(reply_path.exists, "application DECRQM reply")
        self.assertIn(reply_path.read_bytes(), DECRQM_RESPONSES)
        if not forwarded:
            self.assertNotIn(
                COMMIT_QUERY,
                payload,
                "tmux-virtualized DECRQM must not reach the outer client",
            )
        # unittest prints a method name even when the method is skipped. Emit a
        # distinct marker only after the behavioral assertions above so the
        # shell aggregator cannot report unsupported tmux behavior as covered.
        print("APPLICATION_DECRQM_PRESERVED")

    def test_nested_commit_and_parent_bubble_use_one_terminal_barrier(self) -> None:
        inner_socket = self.harness.new_server("inner", "inner", "cat")
        inner_left = self.harness.tmux(
            inner_socket, "display-message", "-p", "-t", "inner", "#{pane_id}"
        ).stdout.strip()
        inner_right = self.harness.tmux(
            inner_socket,
            "split-window",
            "-h",
            "-d",
            "-t",
            "inner",
            "-P",
            "-F",
            "#{pane_id}",
            "cat",
        ).stdout.strip()
        self.harness.tmux(inner_socket, "select-pane", "-t", inner_left)
        self.harness.configure_commits(inner_socket, terminal_replies=False)

        outer_socket = self.harness.new_server("outer", "outer", "cat")
        outer_nested = self.harness.tmux(
            outer_socket, "display-message", "-p", "-t", "outer", "#{pane_id}"
        ).stdout.strip()
        outer_right = self.harness.tmux(
            outer_socket,
            "split-window",
            "-h",
            "-d",
            "-t",
            "outer",
            "-P",
            "-F",
            "#{pane_id}",
            "cat",
        ).stdout.strip()
        self.harness.tmux(outer_socket, "select-pane", "-t", outer_nested)
        self.harness.configure_commits(outer_socket, terminal_replies=True)
        terminal = self.harness.attach(outer_socket, "outer")

        outer_relay = self.harness.start_relay(
            "outer",
            outer_socket,
            outer_nested,
        )
        nested_command = (
            f"exec env TERMNAV_PARENT_RELAY={shlex.quote(outer_relay)} "
            f"tmux -S {shlex.quote(inner_socket)} attach-session -t inner"
        )
        self.harness.tmux(
            outer_socket,
            "respawn-pane",
            "-k",
            "-t",
            outer_nested,
            nested_command,
        )
        self.harness.wait_for_nested_client(outer_socket, outer_nested, inner_socket, inner_left)
        inner_relay = self.harness.start_relay(
            "inner",
            inner_socket,
            inner_left,
        )

        # The inner server can handle this request. The outer server therefore
        # consumes the terminal reply and forwards User8 into the nested client.
        request = self.harness.start_send(inner_relay, "pane-select", "right")
        terminal.read_until(COMMIT_QUERY)
        terminal.send(DECRQM_RESPONSES[4])
        self.harness.finish_send(request, "pane-select", "right")
        wait_for(
            lambda: self.harness.active_pane(inner_socket, "inner") == inner_right,
            "nested User8 commit",
        )
        self.assertEqual(
            outer_nested,
            self.harness.active_pane(outer_socket, "outer"),
        )

        # At the inner edge, the same semantic request bubbles outward and is
        # committed directly by the nearest outer tmux scope.
        self.harness.tmux(inner_socket, "kill-pane", "-t", inner_right)
        terminal.drain()
        request = self.harness.start_send(inner_relay, "pane-select", "right")
        terminal.read_until(COMMIT_QUERY)
        terminal.send(DECRQM_RESPONSES[0])
        self.harness.finish_send(request, "pane-select", "right")
        wait_for(
            lambda: self.harness.active_pane(outer_socket, "outer") == outer_right,
            "parent pane bubble",
        )

    def test_three_tmux_levels_prepare_before_one_terminal_barrier(self) -> None:
        inner_socket = self.harness.new_server("deep-inner", "inner", "cat")
        inner_left = self.harness.tmux(
            inner_socket, "display-message", "-p", "-t", "inner", "#{pane_id}"
        ).stdout.strip()
        inner_right = self.harness.tmux(
            inner_socket,
            "split-window",
            "-h",
            "-d",
            "-t",
            "inner",
            "-P",
            "-F",
            "#{pane_id}",
            "cat",
        ).stdout.strip()
        self.harness.tmux(inner_socket, "select-pane", "-t", inner_left)
        self.harness.configure_commits(inner_socket, terminal_replies=False)

        middle_socket = self.harness.new_server("deep-middle", "middle", "cat")
        middle_pane = self.harness.tmux(
            middle_socket, "display-message", "-p", "-t", "middle", "#{pane_id}"
        ).stdout.strip()
        self.harness.configure_commits(middle_socket, terminal_replies=False)

        outer_socket = self.harness.new_server("deep-outer", "outer", "cat")
        outer_pane = self.harness.tmux(
            outer_socket, "display-message", "-p", "-t", "outer", "#{pane_id}"
        ).stdout.strip()
        self.harness.configure_commits(outer_socket, terminal_replies=True)
        terminal = self.harness.attach(outer_socket, "outer")

        outer_relay = self.harness.start_relay("deep-outer", outer_socket, outer_pane)
        self.harness.tmux(
            outer_socket,
            "respawn-pane",
            "-k",
            "-t",
            outer_pane,
            f"exec env TERMNAV_PARENT_RELAY={shlex.quote(outer_relay)} "
            f"tmux -S {shlex.quote(middle_socket)} attach-session -t middle",
        )
        self.harness.wait_for_nested_client(outer_socket, outer_pane, middle_socket, middle_pane)
        middle_relay = self.harness.start_relay("deep-middle", middle_socket, middle_pane)
        self.harness.tmux(
            middle_socket,
            "respawn-pane",
            "-k",
            "-t",
            middle_pane,
            f"exec env TERMNAV_PARENT_RELAY={shlex.quote(middle_relay)} "
            f"tmux -S {shlex.quote(inner_socket)} attach-session -t inner",
        )
        self.harness.wait_for_nested_client(middle_socket, middle_pane, inner_socket, inner_left)
        inner_relay = self.harness.start_relay("deep-inner", inner_socket, inner_left)

        request = self.harness.start_send(inner_relay, "pane-select", "right")
        terminal.read_until(COMMIT_QUERY)
        self.assertEqual(inner_left, self.harness.active_pane(inner_socket, "inner"))
        self.assertEqual(middle_pane, self.harness.active_pane(middle_socket, "middle"))
        self.assertEqual(outer_pane, self.harness.active_pane(outer_socket, "outer"))
        terminal.send(DECRQM_RESPONSES[1])
        self.harness.finish_send(request, "pane-select", "right")

        wait_for(
            lambda: self.harness.active_pane(inner_socket, "inner") == inner_right,
            "three-level nested commit",
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--relay", required=True)
    parser.add_argument("--python-relay", required=True)
    parser.add_argument("--termnav", required=True)
    arguments, _ = parser.parse_known_args()
    RelayTerminalTest.relay = arguments.relay
    RelayTerminalTest.python_relay = arguments.python_relay
    RelayTerminalTest.termnav = arguments.termnav
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(RelayTerminalTest)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
