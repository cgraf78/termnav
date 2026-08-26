#!/usr/bin/env python3
"""Tests for the shared navigation state machine."""

from __future__ import annotations

import base64
import os
import shlex
import subprocess
import sys
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "lib" / "termnav"))

from navigation import (  # noqa: E402
    Client,
    Navigator,
    Outcome,
    Scope,
    SystemBackend,
    choose_client,
    parse_clients,
    process_tmux_parent,
    validate_request,
)


class FakeBackend:
    """Record traversal decisions without coupling tests to subprocess syntax."""

    def __init__(self) -> None:
        self.current: Scope | None = None
        self.results: dict[str, Outcome] = {}
        self.clients: dict[str, Client | None] = {}
        self.refreshed: dict[int, Client | None] = {}
        self.refreshed_scopes: dict[str, Scope | None] = {}
        self.parents: dict[int, Scope | None] = {}
        self.relays: dict[int, Outcome] = {}
        self.terminal_result = Outcome.DECLINED
        self.valid_clients: dict[int, bool] = {}
        self.events: list[str] = []
        self.executed_scopes: list[Scope] = []

    def current_scope(self) -> Scope | None:
        self.events.append("current")
        return self.current

    def execute(self, scope: Scope, action: str, direction: str) -> Outcome:
        self.executed_scopes.append(scope)
        self.events.append(f"execute:{scope.identity}:{action}:{direction}")
        return self.results.get(scope.identity, Outcome.DECLINED)

    def resolve_client(self, scope: Scope, started_at: int) -> Client | None:
        self.events.append(f"resolve:{scope.identity}:{started_at}")
        return self.clients.get(scope.identity)

    def refresh_client(self, client: Client) -> Client | None:
        self.events.append(f"refresh:{client.pid}")
        return self.refreshed.get(client.pid, client)

    def inspect_scope(self, scope: Scope, started_at: int) -> tuple[Scope | None, Client | None]:
        self.events.append(f"inspect:{scope.identity}:{started_at}")
        client = self.clients.get(scope.identity)
        if scope.session is not None:
            return scope, client
        if client is None:
            return None, None
        return replace(scope, session=client.session), client

    def refresh_scope(self, scope: Scope) -> Scope | None:
        self.events.append(f"refresh-scope:{scope.identity}")
        return self.refreshed_scopes.get(scope.identity, scope)

    def parent_scope(self, client: Client) -> Scope | None:
        self.events.append(f"parent:{client.pid}")
        return self.parents.get(client.pid)

    def validate_client(self, client: Client, started_at: int) -> bool:
        self.events.append(f"validate:{client.pid}")
        return self.valid_clients.get(client.pid, True)

    def relay(self, client: Client, action: str, direction: str) -> Outcome:
        self.events.append(f"relay:{client.pid}:{action}:{direction}")
        return self.relays.get(client.pid, Outcome.DECLINED)

    def terminal(self, client: Client | None, action: str, direction: str) -> Outcome:
        pid = client.pid if client else 0
        self.events.append(f"terminal:{pid}:{action}:{direction}")
        return self.terminal_result


def scope(name: str) -> Scope:
    """Build a stable synthetic tmux scope."""
    return Scope(socket=f"/tmp/{name}.sock", pane=f"%{len(name)}", session=f"${name}")


def client(
    pid: int,
    *,
    activity: int = 100,
    focused: bool = False,
    termtype: str = "tmux-256color",
) -> Client:
    """Build a client with enough identity to traverse to another scope."""
    return Client(
        activity=activity,
        pid=pid,
        tty=f"/dev/pts/{pid}",
        termtype=termtype,
        session="$session",
        pane="%1",
        focused=focused,
        socket="/tmp/source.sock",
    )


class ClientSelectionTest(unittest.TestCase):
    def test_client_parser_preserves_route_identity_and_control_mode(self) -> None:
        parsed = parse_clients(
            "/tmp/tmux.sock",
            "100|10|/dev/pts/10|xterm.js(6.1)|$1|%2|attached,focused,UTF-8|0|80\n"
            "101|11|/dev/pts/11|tmux-256color|$2|%3|attached,UTF-8|1|81\n",
        )

        self.assertEqual(
            parsed,
            [
                Client(
                    activity=100,
                    pid=10,
                    tty="/dev/pts/10",
                    termtype="xterm.js(6.1)",
                    session="$1",
                    pane="%2",
                    focused=True,
                    control=False,
                    socket="/tmp/tmux.sock",
                    created=80,
                ),
                Client(
                    activity=101,
                    pid=11,
                    tty="/dev/pts/11",
                    termtype="tmux-256color",
                    session="$2",
                    pane="%3",
                    focused=False,
                    control=True,
                    socket="/tmp/tmux.sock",
                    created=81,
                ),
            ],
        )

    def test_single_eligible_client_is_selected_without_focus(self) -> None:
        only = client(10, activity=1)
        self.assertEqual(choose_client([only], started_at=100), only)

    def test_unique_focused_client_wins_before_newer_activity(self) -> None:
        focused = client(10, activity=90, focused=True)
        newer = client(11, activity=100)
        self.assertEqual(choose_client([focused, newer], started_at=100), focused)

    def test_two_focused_clients_fail_closed(self) -> None:
        self.assertIsNone(
            choose_client(
                [client(10, focused=True), client(11, focused=True)],
                started_at=100,
            )
        )

    def test_unique_recent_client_is_a_bounded_focus_fallback(self) -> None:
        older = client(10, activity=98)
        newest = client(11, activity=100)
        self.assertEqual(
            choose_client([older, newest], started_at=100, freshness_seconds=1),
            newest,
        )

    def test_stale_or_tied_activity_fails_closed(self) -> None:
        stale = [client(10, activity=90), client(11, activity=89)]
        tied = [client(10, activity=100), client(11, activity=100)]
        self.assertIsNone(choose_client(stale, started_at=100, freshness_seconds=1))
        self.assertIsNone(choose_client(tied, started_at=100, freshness_seconds=1))


class RequestValidationTest(unittest.TestCase):
    def test_each_public_action_accepts_only_its_directions(self) -> None:
        for action, directions in {
            "pane-select": ("left", "down", "up", "right"),
            "tab-select": ("next", "previous"),
            "tab-move": ("left", "right"),
        }.items():
            for direction in directions:
                validate_request(action, direction)

    def test_unknown_action_or_direction_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "invalid navigation action"):
            validate_request("pane", "left")
        with self.assertRaisesRegex(ValueError, "invalid pane-select direction"):
            validate_request("pane-select", "diagonal")


class ProcessParentTest(unittest.TestCase):
    def test_tmux_parent_search_has_no_artificial_nesting_limit(self) -> None:
        parents = {pid: pid - 1 for pid in range(3, 103)}

        def environment(pid: int, name: str) -> str | None:
            if pid != 2:
                return None
            return "/tmp/outer.sock,7,0" if name == "TMUX" else "%9"

        with (
            mock.patch("navigation.process_env", side_effect=environment),
            mock.patch("navigation.process_parent", side_effect=parents.get),
        ):
            self.assertEqual(process_tmux_parent(102), ("/tmp/outer.sock", "%9"))

    def test_tmux_parent_search_stops_on_a_process_cycle(self) -> None:
        with (
            mock.patch("navigation.process_env", return_value=None),
            mock.patch("navigation.process_parent", side_effect={10: 11, 11: 10}.get),
        ):
            self.assertIsNone(process_tmux_parent(10))


class ClientRevalidationTest(unittest.TestCase):
    def setUp(self) -> None:
        self.backend = SystemBackend("/tmp")
        self.expected = client(10, activity=100, focused=True)

    @staticmethod
    def listing(*clients: Client) -> str:
        return "".join(
            f"{item.activity}|{item.pid}|{item.tty}|{item.termtype}|"
            f"{item.session}|{item.pane}|"
            f"{'attached,focused' if item.focused else 'attached'}|0|"
            f"{item.created}\n"
            for item in clients
        )

    def test_inferred_client_is_rejected_after_focus_moves(self) -> None:
        replacement = client(11, activity=100, focused=True)
        old = replace(self.expected, focused=False)
        listed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=self.listing(old, replacement), stderr=""
        )

        with mock.patch.object(self.backend, "_tmux", return_value=listed):
            self.assertFalse(self.backend.validate_client(self.expected, 100))

    def test_exact_tmux_origin_survives_another_focused_attachment(self) -> None:
        exact = replace(self.expected, exact=True)
        replacement = client(11, activity=100, focused=True)
        old = replace(self.expected, focused=False)
        listed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=self.listing(old, replacement), stderr=""
        )

        with mock.patch.object(self.backend, "_tmux", return_value=listed):
            self.assertTrue(self.backend.validate_client(exact, 100))

    def test_refresh_rejects_a_recreated_client_with_reused_pid_and_tty(self) -> None:
        expected = replace(self.expected, created=80)
        recreated = replace(expected, created=81)

        with mock.patch.object(self.backend, "_all_clients", return_value=[recreated]):
            self.assertIsNone(self.backend.refresh_client(expected))

    def test_exact_origin_rejects_recreated_client_identity(self) -> None:
        expected = replace(self.expected, exact=True, created=80)
        recreated = replace(expected, created=81)

        with mock.patch.object(self.backend, "_all_clients", return_value=[recreated]):
            self.assertFalse(self.backend.validate_client(expected, 100))

    def test_shared_session_resolves_without_choosing_a_physical_client(self) -> None:
        current = Scope("/tmp/tmux.sock", "%1")
        clients = [
            replace(client(10), socket=current.socket, pane=current.pane, session="$1"),
            replace(client(11), socket=current.socket, pane=current.pane, session="$1"),
        ]

        with mock.patch.object(self.backend, "_all_clients", return_value=clients):
            self.assertEqual(
                self.backend.inspect_scope(current, 100),
                (replace(current, session="$1"), None),
            )


class TraversalTest(unittest.TestCase):
    def test_local_pane_retains_optional_client_provenance(self) -> None:
        backend = FakeBackend()
        backend.current = scope("inner")
        backend.clients[backend.current.identity] = client(10)
        backend.results[backend.current.identity] = Outcome.HANDLED

        result = Navigator(backend, now=lambda: 100).navigate(
            "pane-select", "left", include_current=True
        )

        self.assertEqual(result, Outcome.HANDLED)
        self.assertEqual(
            backend.events,
            [
                "current",
                "inspect:/tmp/inner.sock:%5:100",
                "execute:/tmp/inner.sock:%5:pane-select:left",
            ],
        )

    def test_linked_window_resolves_its_session_before_local_tab_action(self) -> None:
        backend = FakeBackend()
        current = Scope(socket="/tmp/linked.sock", pane="%7")
        owner = Client(
            activity=100,
            pid=10,
            tty="/dev/pts/10",
            termtype="xterm.js(6.1)",
            session="$chosen",
            pane="%7",
            focused=True,
            socket=current.socket,
        )
        backend.current = current
        backend.clients[current.identity] = owner
        backend.results[current.identity] = Outcome.HANDLED

        result = Navigator(backend, now=lambda: 100).navigate(
            "tab-select", "next", include_current=True
        )

        self.assertEqual(result, Outcome.HANDLED)
        self.assertEqual(backend.executed_scopes[0].session, "$chosen")
        self.assertEqual(backend.events[:2], ["current", "inspect:/tmp/linked.sock:%7:100"])

    def test_local_pane_action_does_not_require_a_linked_session(self) -> None:
        backend = FakeBackend()
        current = Scope(socket="/tmp/linked.sock", pane="%7")
        owner = Client(
            activity=100,
            pid=10,
            tty="/dev/pts/10",
            termtype="xterm.js(6.1)",
            session="$chosen",
            pane="%7",
            focused=True,
            socket=current.socket,
        )
        backend.current = current
        backend.clients[current.identity] = owner
        backend.results[current.identity] = Outcome.HANDLED

        result = Navigator(backend, now=lambda: 100).navigate(
            "pane-select", "right", include_current=True
        )

        self.assertEqual(result, Outcome.HANDLED)
        self.assertIsNone(backend.executed_scopes[0].session)
        self.assertEqual(
            backend.events,
            [
                "current",
                "inspect:/tmp/linked.sock:%7:100",
                "execute:/tmp/linked.sock:%7:pane-select:right",
            ],
        )

    def test_ambiguous_clients_do_not_block_a_local_session_action(self) -> None:
        backend = FakeBackend()
        current = Scope(socket="/tmp/shared.sock", pane="%7", session="$shared")
        backend.current = current
        backend.results[current.identity] = Outcome.HANDLED

        result = Navigator(backend, now=lambda: 100).navigate(
            "tab-select", "next", include_current=True
        )

        self.assertEqual(result, Outcome.HANDLED)
        self.assertEqual(backend.executed_scopes, [current])

    def test_local_parent_precedes_an_available_relay(self) -> None:
        backend = FakeBackend()
        inner = scope("inner")
        parent = scope("parent")
        inner_client = client(10)
        backend.current = inner
        backend.clients[inner.identity] = inner_client
        backend.parents[inner_client.pid] = parent
        backend.clients[parent.identity] = client(11)
        backend.results[parent.identity] = Outcome.HANDLED
        backend.relays[inner_client.pid] = Outcome.HANDLED

        result = Navigator(backend, now=lambda: 100).navigate(
            "tab-select", "next", include_current=True
        )

        self.assertEqual(result, Outcome.HANDLED)
        self.assertEqual(
            backend.events,
            [
                "current",
                "inspect:/tmp/inner.sock:%5:100",
                "execute:/tmp/inner.sock:%5:tab-select:next",
                "validate:10",
                "parent:10",
                "execute:/tmp/parent.sock:%6:tab-select:next",
            ],
        )

    def test_parent_mode_does_not_reenter_the_declined_current_scope(self) -> None:
        backend = FakeBackend()
        parent = scope("parent")
        origin = client(10)
        backend.current = scope("current")
        backend.parents[origin.pid] = parent
        backend.clients[parent.identity] = client(11)
        backend.results[parent.identity] = Outcome.HANDLED

        result = Navigator(backend, now=lambda: 100).navigate(
            "pane-select",
            "down",
            include_current=False,
            exact_client=origin,
        )

        self.assertEqual(result, Outcome.HANDLED)
        self.assertEqual(
            backend.events,
            [
                "validate:10",
                "parent:10",
                "execute:/tmp/parent.sock:%6:pane-select:down",
            ],
        )

    def test_parent_mode_rejects_a_client_that_left_the_source_scope(self) -> None:
        backend = FakeBackend()
        origin = client(10)
        backend.valid_clients[origin.pid] = False
        backend.parents[origin.pid] = scope("parent")

        result = Navigator(backend, now=lambda: 100).navigate(
            "pane-select",
            "down",
            include_current=False,
            exact_client=origin,
        )

        self.assertEqual(result, Outcome.ERROR)
        self.assertEqual(backend.events, ["validate:10"])

    def test_each_declined_local_parent_is_walked_before_the_relay(self) -> None:
        backend = FakeBackend()
        first = scope("first")
        second = scope("second")
        third = scope("third")
        first_client = client(10)
        second_client = client(11)
        third_client = client(12)
        backend.current = first
        backend.clients[first.identity] = first_client
        backend.parents[first_client.pid] = second
        backend.clients[second.identity] = second_client
        backend.parents[second_client.pid] = third
        backend.clients[third.identity] = third_client
        backend.relays[third_client.pid] = Outcome.HANDLED

        result = Navigator(backend, now=lambda: 100).navigate(
            "pane-select", "right", include_current=True
        )

        self.assertEqual(result, Outcome.HANDLED)
        self.assertEqual(
            backend.events,
            [
                "current",
                "inspect:/tmp/first.sock:%5:100",
                "execute:/tmp/first.sock:%5:pane-select:right",
                "validate:10",
                "parent:10",
                "execute:/tmp/second.sock:%6:pane-select:right",
                "resolve:/tmp/second.sock:%6:100",
                "validate:11",
                "parent:11",
                "execute:/tmp/third.sock:%5:pane-select:right",
                "resolve:/tmp/third.sock:%5:100",
                "validate:12",
                "parent:12",
                "validate:12",
                "relay:12:pane-select:right",
            ],
        )

    def test_continuation_follows_the_same_scope_after_a_tab_switch(self) -> None:
        backend = FakeBackend()
        first = Scope(socket="/tmp/server.sock", pane="%1")
        first_client = Client(
            activity=100,
            pid=10,
            tty="/dev/pts/10",
            termtype="xterm-256color",
            session="$session",
            pane="%1",
            focused=True,
            socket=first.socket,
        )
        backend.current = first
        backend.clients[first.identity] = first_client
        backend.results[first.identity] = Outcome.HANDLED

        first_result = Navigator(backend, now=lambda: 100).route(
            "tab-select", "next", include_current=True
        )
        second = Scope(first.socket, "%2", first_client.session)
        second_client = replace(first_client, pane="%2")
        backend.refreshed_scopes[first.identity] = second
        backend.refreshed[first_client.pid] = second_client
        backend.results[second.identity] = Outcome.HANDLED
        second_result = Navigator(backend, now=lambda: 101).route(
            "tab-select",
            "next",
            include_current=False,
            continuing_client=first_result.client,
            continuing_scope=first_result.scope,
        )

        self.assertEqual(first_result.outcome, Outcome.HANDLED)
        self.assertEqual(second_result.outcome, Outcome.HANDLED)
        self.assertEqual(second_result.client, second_client)
        self.assertEqual(second_result.scope, second)
        self.assertIn("refresh:10", backend.events)
        self.assertIn("refresh-scope:/tmp/server.sock:%1", backend.events)
        self.assertEqual(backend.executed_scopes[-1].pane, "%2")

    def test_transport_error_never_falls_through_to_terminal(self) -> None:
        backend = FakeBackend()
        current = scope("current")
        origin = client(10)
        backend.current = current
        backend.clients[current.identity] = origin
        backend.relays[origin.pid] = Outcome.ERROR
        backend.terminal_result = Outcome.HANDLED

        result = Navigator(backend, now=lambda: 100).navigate(
            "tab-select", "next", include_current=True
        )

        self.assertEqual(result, Outcome.ERROR)
        self.assertNotIn("terminal:10:tab-select:next", backend.events)

    def test_declined_relay_reaches_supported_terminal_endpoint(self) -> None:
        backend = FakeBackend()
        current = scope("current")
        origin = client(10)
        backend.current = current
        backend.clients[current.identity] = origin
        backend.terminal_result = Outcome.HANDLED

        result = Navigator(backend, now=lambda: 100).navigate(
            "tab-select", "previous", include_current=True
        )

        self.assertEqual(result, Outcome.HANDLED)
        self.assertEqual(backend.events[-1], "terminal:10:tab-select:previous")

    def test_cycle_in_local_parent_metadata_fails_closed(self) -> None:
        backend = FakeBackend()
        first = scope("first")
        second = scope("second")
        first_client = client(10)
        second_client = client(11)
        backend.current = first
        backend.clients[first.identity] = first_client
        backend.parents[first_client.pid] = second
        backend.clients[second.identity] = second_client
        backend.parents[second_client.pid] = first

        result = Navigator(backend, now=lambda: 100).navigate(
            "pane-select", "up", include_current=True
        )

        self.assertEqual(result, Outcome.ERROR)
        self.assertEqual(backend.events.count("execute:/tmp/first.sock:%5:pane-select:up"), 1)


class SystemBackendDispatchTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.bin_dir = Path(self.temporary.name)
        self.log = self.bin_dir / "calls.log"

    def executable(self, name: str, body: str) -> None:
        path = self.bin_dir / name
        path.write_text("#!/usr/bin/env bash\nset -eu\n" + body, encoding="utf-8")
        path.chmod(0o755)

    def spawn_client(self, **environment: str) -> subprocess.Popen[bytes]:
        child_environment = os.environ.copy()
        child_environment.update(environment)
        process = subprocess.Popen(["sleep", "60"], env=child_environment)
        self.addCleanup(process.wait)
        self.addCleanup(process.terminate)
        return process

    def matching_tmux(
        self,
        process: subprocess.Popen[bytes],
        *,
        tty: str | None = None,
    ) -> None:
        """Expose the exact synthetic client expected by dispatch tests."""

        tty = tty or f"/dev/pts/{process.pid}"
        self.executable(
            "tmux",
            'printf "100|%s|%s|tmux-256color|%s|%%1|attached|0|1\\n" '
            f'"{process.pid}" "{tty}" '
            "'$session'\n",
        )

    def test_relay_reads_the_selected_clients_parent_socket(self) -> None:
        self.executable(
            "tmux",
            'printf "%s|%s|%s|%s|%s|%s|%s|0|1\\n" '
            '"$TERMNAV_CLIENT_ACTIVITY" "$TERMNAV_CLIENT_PID" '
            '"$TERMNAV_CLIENT_TTY" tmux-256color "$TERMNAV_CLIENT_SESSION" '
            '"$TERMNAV_CLIENT_PANE" attached\n',
        )
        self.executable(
            "termnav-relay",
            'printf "%s\\n" "$*" >>"$TERMNAV_TEST_LOG"\n',
        )
        process = self.spawn_client(TERMNAV_PARENT_RELAY="/tmp/selected.sock")
        backend = SystemBackend(
            self.bin_dir,
            environment={
                **os.environ,
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
                "TERMNAV_TEST_LOG": str(self.log),
                "TERMNAV_CLIENT_ACTIVITY": "100",
                "TERMNAV_CLIENT_PID": str(process.pid),
                "TERMNAV_CLIENT_TTY": f"/dev/pts/{process.pid}",
                "TERMNAV_CLIENT_SESSION": "$session",
                "TERMNAV_CLIENT_PANE": "%1",
            },
        )

        result = backend.relay(client(process.pid), "pane-select", "down")

        self.assertEqual(result, Outcome.HANDLED)
        self.assertEqual(
            self.log.read_text(encoding="utf-8").strip(),
            f"send pane down --client-pid {process.pid} --client-tty /dev/pts/{process.pid}",
        )

    def test_relay_accepts_exact_client_identity_without_optional_session(self) -> None:
        self.executable(
            "tmux",
            'printf "%s|%s|%s|tmux-256color|%s|%%1|attached|0|1\\n" '
            '"$TERMNAV_CLIENT_ACTIVITY" "$TERMNAV_CLIENT_PID" '
            "\"$TERMNAV_CLIENT_TTY\" '$session'\n",
        )
        self.executable(
            "termnav-relay",
            'printf "%s\\n" "$*" >>"$TERMNAV_TEST_LOG"\n',
        )
        process = self.spawn_client(TERMNAV_PARENT_RELAY="/tmp/selected.sock")
        backend = SystemBackend(
            self.bin_dir,
            environment={
                **os.environ,
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
                "TERMNAV_TEST_LOG": str(self.log),
                "TERMNAV_CLIENT_ACTIVITY": "100",
                "TERMNAV_CLIENT_PID": str(process.pid),
                "TERMNAV_CLIENT_TTY": f"/dev/pts/{process.pid}",
            },
        )

        selected = replace(client(process.pid), session="")
        result = backend.relay(selected, "pane-select", "down")

        self.assertEqual(result, Outcome.HANDLED)
        self.assertTrue(self.log.exists())

    def test_vscode_terminal_uses_only_the_selected_clients_credentials(self) -> None:
        self.executable(
            "curl",
            'printf "%s\\n" "$*" >>"$TERMNAV_TEST_LOG"\n',
        )
        token = "a" * 64
        process = self.spawn_client(
            TERM_PROGRAM="vscode",
            TERMNAV_VSCODE_SOCKET="/tmp/selected-vscode.sock",
            TERMNAV_VSCODE_TOKEN=token,
        )
        self.matching_tmux(process)
        backend = SystemBackend(
            self.bin_dir,
            environment={
                **os.environ,
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
                "TERMNAV_TEST_LOG": str(self.log),
                "TERMNAV_VSCODE_SOCKET": "/tmp/stale.sock",
                "TERMNAV_VSCODE_TOKEN": "stale-token",
            },
        )

        result = backend.terminal(client(process.pid), "tab-select", "next")

        self.assertEqual(result, Outcome.HANDLED)
        request = self.log.read_text(encoding="utf-8")
        self.assertIn("--unix-socket /tmp/selected-vscode.sock", request)
        self.assertIn(f'{{"direction":"next","token":"{token}"}}', request)
        self.assertNotIn("/tmp/stale.sock", request)

    def test_vscode_socket_rejects_a_partial_capability_without_curl(self) -> None:
        self.executable(
            "curl",
            'printf "called\\n" >>"$TERMNAV_TEST_LOG"\n',
        )
        process = self.spawn_client(
            TERM_PROGRAM="vscode",
            TERMNAV_VSCODE_SOCKET="/tmp/selected-vscode.sock",
            TERMNAV_VSCODE_TOKEN="short",
        )
        backend = SystemBackend(
            self.bin_dir,
            environment={
                **os.environ,
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
                "TERMNAV_TEST_LOG": str(self.log),
            },
        )

        result = backend.terminal(client(process.pid), "tab-select", "next")

        self.assertEqual(result, Outcome.ERROR)
        self.assertFalse(self.log.exists())

    def test_vscode_terminal_uses_only_an_explicit_process_global_fallback(
        self,
    ) -> None:
        self.executable(
            "curl",
            'printf "%s\\n" "$*" >>"$TERMNAV_TEST_LOG"\n'
            'printf \'{"jsonrpc":"2.0","id":1,"result":{}}\'\n',
        )
        state = self.bin_dir / "state"
        token_path = state / "dot" / "vscode-mcp-auth-token"
        token_path.parent.mkdir(parents=True)
        token_path.write_text("mcp-token\n", encoding="utf-8")
        process = self.spawn_client(TERM_PROGRAM="vscode")
        self.matching_tmux(process)
        backend = SystemBackend(
            self.bin_dir,
            environment={
                **os.environ,
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
                "TERMNAV_TEST_LOG": str(self.log),
                "TERMNAV_VSCODE_FALLBACK_BACKEND": "mcp",
                "XDG_STATE_HOME": str(state),
                "TERMNAV_VSCODE_SOCKET": "/tmp/stale.sock",
                "TERMNAV_VSCODE_TOKEN": "stale-token",
            },
        )

        result = backend.terminal(client(process.pid), "tab-select", "previous")

        self.assertEqual(result, Outcome.HANDLED)
        request = self.log.read_text(encoding="utf-8")
        self.assertIn("http://127.0.0.1:9876/mcp", request)
        self.assertIn("Authorization: Bearer mcp-token", request)
        self.assertIn("workbench.action.terminal.focusPrevious", request)
        self.assertNotIn("/tmp/stale.sock", request)

    def test_vscode_mcp_initializes_once_then_retries_the_command(self) -> None:
        state = self.bin_dir / "state-retry"
        token_path = state / "dot" / "vscode-mcp-auth-token"
        token_path.parent.mkdir(parents=True)
        token_path.write_text("mcp-token\n", encoding="utf-8")
        count = self.bin_dir / "curl-count"
        count.write_text("0\n", encoding="utf-8")
        self.executable(
            "curl",
            f"count=$(cat {shlex.quote(str(count))})\n"
            "count=$((count + 1))\n"
            f'printf "%s\\n" "$count" >{shlex.quote(str(count))}\n'
            'printf "%s\\n" "$*" >>"$TERMNAV_TEST_LOG"\n'
            'if [ "$count" -eq 1 ]; then\n'
            '  printf \'{"jsonrpc":"2.0","id":1,"error":{}}\'\n'
            "else\n"
            '  printf \'{"jsonrpc":"2.0","id":1,"result":{}}\'\n'
            "fi\n",
        )
        process = self.spawn_client(TERM_PROGRAM="vscode")
        backend = SystemBackend(
            self.bin_dir,
            environment={
                **os.environ,
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
                "TERMNAV_TEST_LOG": str(self.log),
                "TERMNAV_VSCODE_FALLBACK_BACKEND": "mcp",
                "XDG_STATE_HOME": str(state),
            },
        )

        result = backend.terminal(client(process.pid), "tab-select", "next")

        self.assertEqual(result, Outcome.HANDLED)
        calls = self.log.read_text(encoding="utf-8").splitlines()
        self.assertEqual(3, len(calls))
        self.assertIn('"method":"initialize"', calls[1])
        self.assertIn('"method":"tools/call"', calls[2])

    def test_unsupported_vscode_move_is_consumed_without_a_helper(self) -> None:
        process = self.spawn_client(TERM_PROGRAM="vscode")
        self.matching_tmux(process)
        backend = SystemBackend(
            self.bin_dir,
            environment={
                **os.environ,
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
                "TERMNAV_TEST_LOG": str(self.log),
            },
        )

        result = backend.terminal(client(process.pid), "tab-move", "right")

        self.assertEqual(result, Outcome.HANDLED)
        self.assertFalse(self.log.exists())

    def test_direct_wezterm_tab_selection_targets_the_selected_tty(self) -> None:
        tty = self.bin_dir / "selected.tty"
        tty.touch()
        process = self.spawn_client(TERM_PROGRAM="WezTerm")
        self.matching_tmux(process, tty=str(tty))
        backend = SystemBackend(
            self.bin_dir,
            environment={
                **os.environ,
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
            },
        )

        result = backend.terminal(
            replace(
                client(process.pid, termtype="xterm-256color"),
                tty=str(tty),
            ),
            "tab-select",
            "previous",
        )

        self.assertEqual(result, Outcome.HANDLED)
        payload = tty.read_bytes()
        prefix = b"\x1b]1337;SetUserVar=DOT_SWITCH_TAB="
        self.assertTrue(payload.startswith(prefix))
        encoded = payload[len(prefix) :].removesuffix(b"\x07")
        self.assertTrue(base64.b64decode(encoded).startswith(b"previous:"))

    def test_nested_terminal_without_a_relay_does_not_guess_a_parent(self) -> None:
        tty = self.bin_dir / "nested.tty"
        tty.touch()
        process = self.spawn_client(TERM_PROGRAM="WezTerm")
        self.matching_tmux(process, tty=str(tty))
        backend = SystemBackend(
            self.bin_dir,
            environment={
                **os.environ,
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
            },
        )

        selected = replace(client(process.pid), tty=str(tty))

        self.assertEqual(
            backend.terminal(selected, "tab-select", "next"),
            Outcome.HANDLED,
        )
        self.assertEqual(
            backend.terminal(selected, "pane-select", "right"),
            Outcome.HANDLED,
        )
        self.assertEqual(tty.read_bytes(), b"")


class SystemBackendCapabilityTest(unittest.TestCase):
    def backend_with(self, *outputs: str) -> SystemBackend:
        """Return a backend whose tmux probes consume fixed stdout values."""

        backend = SystemBackend("/tmp")
        responses = iter(outputs)

        def fake_tmux(_scope: Scope, *_arguments: str) -> subprocess.CompletedProcess[str]:
            return subprocess.CompletedProcess([], 0, next(responses), "")

        backend._tmux = fake_tmux  # type: ignore[method-assign]
        return backend

    def test_pane_capability_requires_active_non_edge_pane(self) -> None:
        current = Scope("/tmp/tmux.sock", "%1", "$1")

        self.assertTrue(self.backend_with("1|1|0\n").can_execute(current, "pane-select", "right"))
        self.assertFalse(self.backend_with("1|1|1\n").can_execute(current, "pane-select", "right"))
        self.assertFalse(self.backend_with("0|1|0\n").can_execute(current, "pane-select", "right"))

    def test_tab_capability_is_owned_only_by_multi_window_scope(self) -> None:
        current = Scope("/tmp/tmux.sock", "%1", "$1")

        self.assertTrue(self.backend_with("1|1|2\n").can_execute(current, "tab-select", "next"))
        self.assertTrue(self.backend_with("1|1|2\n").can_execute(current, "tab-move", "right"))
        self.assertFalse(self.backend_with("1|1|1\n").can_execute(current, "tab-select", "next"))

    def test_tab_move_targets_stable_window_ids(self) -> None:
        backend = SystemBackend("/tmp")
        calls: list[tuple[str, ...]] = []
        outputs = iter(("2|@0\n", "@0\n@1\n", ""))

        def fake_tmux(_scope: Scope, *arguments: str) -> subprocess.CompletedProcess[str]:
            calls.append(arguments)
            return subprocess.CompletedProcess([], 0, next(outputs), "")

        backend._tmux = fake_tmux  # type: ignore[method-assign]
        result = backend.execute(
            Scope("/tmp/tmux.sock", "%1", "$1"),
            "tab-move",
            "right",
        )

        self.assertEqual(result, Outcome.HANDLED)
        self.assertIn("-s '$1:@0'", calls[-1][-2])
        self.assertIn("-t '$1:@1'", calls[-1][-2])


if __name__ == "__main__":
    unittest.main(verbosity=2)
