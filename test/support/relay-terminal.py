#!/usr/bin/env python3
"""Exercise Termnav's commit barrier through real tmux clients and PTYs."""

from __future__ import annotations

import argparse
import os
import pathlib
import pty
import select
import shlex
import signal
import socket
import subprocess
import tempfile
import time
import unittest

COMMIT_KEY = b"\x1b[777009u"
COMMIT_QUERY = b"\x1b[?2004$p"
DECRQM_RESPONSES = tuple(f"\x1b[?2004;{state}$y".encode() for state in range(5))
SENTINEL_KEY = b"\x07"


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

    def __init__(self, relay: str):
        self.relay = str(pathlib.Path(relay).resolve())
        self.temporary = tempfile.TemporaryDirectory(prefix="termnav-terminal-")
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
        commit = (
            f"{shlex.quote(self.relay)} commit "
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
    ) -> str:
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
        environment.pop("TERMNAV_PARENT_RELAY", None)
        process = subprocess.Popen(
            [self.relay, "serve", "--socket", relay_socket],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.relays.append(process)

        def listening() -> bool:
            if not pathlib.Path(relay_socket).exists():
                return False
            probe = socket.socket(socket.AF_UNIX)
            probe.settimeout(0.05)
            try:
                probe.connect(relay_socket)
                return True
            except OSError:
                return False
            finally:
                probe.close()

        wait_for(listening, f"relay listener {name}")
        return relay_socket

    def start_send(self, relay_socket: str, scope: str, direction: str) -> subprocess.Popen[str]:
        """Start a request whose terminal response may be driven by the test."""

        environment = self.environment.copy()
        environment["TERMNAV_PARENT_RELAY"] = relay_socket
        return subprocess.Popen(
            [self.relay, "send", scope, direction],
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

    def finish_send(self, process: subprocess.Popen[str], scope: str, direction: str) -> None:
        """Require one asynchronous request to finish successfully."""

        stdout, stderr = process.communicate(timeout=10)
        if process.returncode != 0:
            diagnostics = []
            for relay_log in self.relay_logs:
                if relay_log.exists():
                    diagnostics.append(
                        f"{relay_log.name}:\n{relay_log.read_text(encoding='utf-8')}"
                    )
            raise AssertionError(
                f"relay send {scope} {direction} exited {process.returncode}: "
                f"{stdout}{stderr}\n{''.join(diagnostics)}"
            )

    def send(self, relay_socket: str, scope: str, direction: str) -> None:
        """Run a request that completes without a test-driven terminal reply."""

        self.finish_send(self.start_send(relay_socket, scope, direction), scope, direction)

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

    def setUp(self) -> None:
        self.harness = RelayHarness(self.relay)

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
        relay_socket = self.harness.start_relay(
            "top",
            tmux_socket,
            source_pane,
        )
        return tmux_socket, source_pane, pane_ids, terminal, relay_socket

    def test_all_decrqm_states_commit_pane_navigation(self) -> None:
        tmux_socket, left, panes, terminal, relay_socket = self.top_level(panes=2)
        right = panes[1]
        for state, response in enumerate(DECRQM_RESPONSES):
            with self.subTest(state=state):
                self.harness.tmux(tmux_socket, "select-pane", "-t", left)
                terminal.drain()
                request = self.harness.start_send(relay_socket, "pane", "right")
                terminal.read_until(COMMIT_QUERY)
                active = self.harness.active_pane(tmux_socket, "top")
                self.assertEqual(left, active, "navigation committed before the reply")
                terminal.send(response)
                self.harness.finish_send(request, "pane", "right")
                wait_for(
                    lambda: self.harness.active_pane(tmux_socket, "top") == right,
                    f"pane commit for DECRQM state {state}",
                )

    def test_window_switch_commits_after_terminal_response(self) -> None:
        tmux_socket, _, _, terminal, relay_socket = self.top_level(windows=2)
        before = self.harness.tmux(
            tmux_socket, "display-message", "-p", "-t", "top", "#{window_id}"
        ).stdout.strip()
        request = self.harness.start_send(relay_socket, "window", "next")
        terminal.read_until(COMMIT_QUERY)
        self.assertEqual(
            before,
            self.harness.tmux(
                tmux_socket, "display-message", "-p", "-t", "top", "#{window_id}"
            ).stdout.strip(),
        )
        terminal.send(DECRQM_RESPONSES[2])
        self.harness.finish_send(request, "window", "next")
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
        request = self.harness.start_send(relay_socket, "move", "left")
        terminal.read_until(COMMIT_QUERY)
        self.assertEqual(
            before,
            self.harness.tmux(
                tmux_socket, "list-windows", "-t", "top", "-F", "#{window_name}"
            ).stdout.splitlines(),
        )
        terminal.send(DECRQM_RESPONSES[3])
        self.harness.finish_send(request, "move", "left")
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

        self.harness.send(relay_socket, "window", "next")

        payload = terminal.read_until(b"]1337;SetUserVar=DOT_SWITCH_TAB=")
        self.assertIn(b"\x1b]1337;SetUserVar=DOT_SWITCH_TAB=", payload)

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

        self.harness.send(relay_socket, "window", "next")

        second.read_until(b"]1337;SetUserVar=DOT_SWITCH_TAB=")
        self.assertNotIn(b"]1337;SetUserVar=DOT_SWITCH_TAB=", first.drain())

    def test_one_window_tab_selection_reaches_the_vscode_window_adapter(self) -> None:
        calls = self.harness.root / "vscode-calls"
        curl = self.harness.root / "curl"
        curl.write_text(
            '#!/bin/sh\nprintf "%s\\n" "$*" >"$TERMNAV_TEST_VSCODE_CALLS"\n',
            encoding="utf-8",
        )
        curl.chmod(0o700)
        token = "a" * 64
        self.harness.environment.update(
            {
                "TERM_PROGRAM": "vscode",
                "TERMNAV_VSCODE_SOCKET": str(self.harness.root / "vscode.sock"),
                "TERMNAV_VSCODE_TOKEN": token,
                "TERMNAV_VSCODE_CURL": str(curl),
                "TERMNAV_TEST_VSCODE_CALLS": str(calls),
            }
        )
        _, _, _, _, relay_socket = self.top_level()

        self.harness.send(relay_socket, "window", "previous")

        arguments = wait_for(
            lambda: calls.read_text(encoding="utf-8") if calls.exists() else None,
            "VS Code window adapter request",
        )
        self.assertIn(f"--unix-socket {self.harness.root / 'vscode.sock'}", arguments)
        self.assertIn(f'{{"direction":"previous","token":"{token}"}}', arguments)

    def test_fragmented_response_waits_for_its_terminator(self) -> None:
        tmux_socket, left, panes, terminal, relay_socket = self.top_level(panes=2)
        right = panes[1]
        request = self.harness.start_send(relay_socket, "pane", "right")
        terminal.read_until(COMMIT_QUERY)
        terminal.send(b"\x1b[?2004;1")
        self.assertEqual(left, self.harness.active_pane(tmux_socket, "top"))
        self.assertTrue(
            list(self.harness.runtime.rglob("*.json")),
            "the armed directive must remain pending until the reply terminates",
        )
        terminal.send(b"$y")
        self.harness.finish_send(request, "pane", "right")
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

        request = self.harness.start_send(relay_socket, "pane", "right")
        terminal.read_until(COMMIT_QUERY)
        terminal.send(DECRQM_RESPONSES[1] + DECRQM_RESPONSES[1])
        self.harness.finish_send(request, "pane", "right")
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
        self.harness.client_identity(inner_socket)
        inner_relay = self.harness.start_relay(
            "inner",
            inner_socket,
            inner_left,
        )

        # The inner server can handle this request. The outer server therefore
        # consumes the terminal reply and forwards User8 into the nested client.
        request = self.harness.start_send(inner_relay, "pane", "right")
        terminal.read_until(COMMIT_QUERY)
        terminal.send(DECRQM_RESPONSES[4])
        self.harness.finish_send(request, "pane", "right")
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
        request = self.harness.start_send(inner_relay, "pane", "right")
        terminal.read_until(COMMIT_QUERY)
        terminal.send(DECRQM_RESPONSES[0])
        self.harness.finish_send(request, "pane", "right")
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
        self.harness.client_identity(middle_socket)
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
        self.harness.client_identity(inner_socket)
        inner_relay = self.harness.start_relay("deep-inner", inner_socket, inner_left)

        request = self.harness.start_send(inner_relay, "pane", "right")
        terminal.read_until(COMMIT_QUERY)
        self.assertEqual(inner_left, self.harness.active_pane(inner_socket, "inner"))
        self.assertEqual(middle_pane, self.harness.active_pane(middle_socket, "middle"))
        self.assertEqual(outer_pane, self.harness.active_pane(outer_socket, "outer"))
        terminal.send(DECRQM_RESPONSES[1])
        self.harness.finish_send(request, "pane", "right")

        wait_for(
            lambda: self.harness.active_pane(inner_socket, "inner") == inner_right,
            "three-level nested commit",
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--relay", required=True)
    arguments, _ = parser.parse_known_args()
    RelayTerminalTest.relay = arguments.relay
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(RelayTerminalTest)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
