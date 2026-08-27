"""Shared navigation policy for applications, tmux layers, and terminals.

The router deliberately knows only how scopes compose. Concrete backends own
tmux commands, SSH relay transport, and terminal-specific actions. Keeping the
state machine independent from those mechanisms makes the important ordering
rule explicit: exhaust local parents before crossing a transport boundary.
"""

from __future__ import annotations

import base64
import json
import os
import shlex
import subprocess
import time
from collections.abc import Callable, Iterable
from dataclasses import dataclass, replace
from enum import IntEnum
from pathlib import Path
from typing import ClassVar, Protocol

from navigation_protocol import WIRE_ACTIONS, validate_request
from process_info import environment as process_env
from process_info import parent as process_parent
from relay_client import new_nonce, send_message, tty_matches

TMUX_FIELD_SEPARATOR = "|"


def _tmux_format(*fields: str) -> str:
    """Build a locale-independent tmux format for constrained machine fields.

    tmux sanitizes literal control characters in format strings under some
    locale/runtime combinations, turning a tab separator into an underscore.
    The fields used by this module are numeric IDs, fixed paths, terminal names,
    and tmux flags; a printable pipe therefore stays intact, and any unexpected
    pipe in a value makes the strict field-count checks fail closed.
    """

    return TMUX_FIELD_SEPARATOR.join(f"#{{{field}}}" for field in fields)


_CLIENT_FORMAT = _tmux_format(
    "client_activity",
    "client_pid",
    "client_tty",
    "client_termtype",
    "session_id",
    "pane_id",
    "client_flags",
    "client_control_mode",
    "client_created",
)


class Outcome(IntEnum):
    """Stable routing outcomes shared by CLI and relay adapters."""

    HANDLED = 0
    ERROR = 1
    DECLINED = 3


@dataclass(frozen=True)
class Scope:
    """One tmux pane together with the session that gives windows meaning."""

    socket: str
    pane: str
    session: str | None = None

    @property
    def identity(self) -> str:
        """Return the cycle-detection identity shared across linked sessions."""
        return f"{self.socket}:{self.pane}"

    @property
    def target(self) -> str:
        """Return a session-qualified pane target when session identity matters."""

        if self.session:
            return f"{self.session}:.{self.pane}"
        return self.pane


@dataclass(frozen=True)
class Client:
    """An attached tmux client eligible to carry a request outward."""

    activity: int
    pid: int
    tty: str
    termtype: str
    session: str
    pane: str
    focused: bool = False
    control: bool = False
    socket: str = ""
    exact: bool = False
    created: int = 0


@dataclass(frozen=True)
class RouteResult:
    """Outcome plus logical and physical provenance safe for a successor."""

    outcome: Outcome
    client: Client | None
    scope: Scope | None = None


class Backend(Protocol):
    """Side-effect boundary used by the topology-independent router."""

    def current_scope(self) -> Scope | None: ...

    def execute(self, scope: Scope, action: str, direction: str) -> Outcome: ...

    def resolve_client(self, scope: Scope, started_at: int) -> Client | None: ...

    def refresh_client(self, client: Client) -> Client | None: ...

    def inspect_scope(
        self, scope: Scope, started_at: int
    ) -> tuple[Scope | None, Client | None]: ...

    def refresh_scope(self, scope: Scope) -> Scope | None: ...

    def validate_client(self, client: Client, started_at: int) -> bool: ...

    def parent_scope(self, client: Client) -> Scope | None: ...

    def relay(self, client: Client, action: str, direction: str) -> Outcome: ...

    def terminal(self, client: Client | None, action: str, direction: str) -> Outcome: ...


def choose_client(
    clients: Iterable[Client],
    *,
    started_at: int,
    freshness_seconds: int = 2,
) -> Client | None:
    """Choose one safe application-origin client or decline ambiguity.

    A tmux binding can provide exact identity, but an application receives only
    key bytes. For that weaker boundary, a sole client is unambiguous and a
    unique focused client is the strongest available signal. Recency is a
    bounded compatibility fallback for terminals that temporarily fail to
    report focus; ties and stale observations deliberately produce no route.
    """

    eligible = [client for client in clients if not client.control]
    if len(eligible) == 1:
        return eligible[0]
    if not eligible:
        return None

    focused = [client for client in eligible if client.focused]
    if len(focused) == 1:
        return focused[0]
    if focused:
        return None

    newest_activity = max(client.activity for client in eligible)
    newest = [client for client in eligible if client.activity == newest_activity]
    if len(newest) != 1:
        return None
    if newest_activity > started_at or started_at - newest_activity > freshness_seconds:
        return None
    return newest[0]


def parse_clients(socket: str, output: str) -> list[Client]:
    """Parse the machine-oriented tmux client format used by the router."""

    clients = []
    for line in output.splitlines():
        fields = line.split(TMUX_FIELD_SEPARATOR)
        if (
            len(fields) != 9
            or not fields[0].isdigit()
            or not fields[1].isdigit()
            or not fields[8].isdigit()
        ):
            continue
        activity, pid, tty, termtype, session, pane, flags, control, created = fields
        if not tty or not session or not pane.startswith("%"):
            continue
        clients.append(
            Client(
                activity=int(activity),
                pid=int(pid),
                tty=tty,
                termtype=termtype,
                session=session,
                pane=pane,
                focused="focused" in flags.split(","),
                control=control == "1",
                socket=socket,
                created=int(created),
            )
        )
    return clients


def process_tmux_parent(pid: int) -> tuple[str, str] | None:
    """Find the nearest complete parent tmux identity in a process lineage."""

    visited: set[int] = set()
    while True:
        if pid in visited or pid <= 1:
            return None
        visited.add(pid)
        tmux_value = process_env(pid, "TMUX") or ""
        pane = process_env(pid, "TMUX_PANE") or ""
        parts = tmux_value.rsplit(",", 2)
        if len(parts) == 3 and parts[0] and pane.startswith("%") and pane[1:].isdigit():
            return parts[0], pane
        parent = process_parent(pid)
        if parent is None:
            return None
        pid = parent


class Navigator:
    """Walk navigation scopes from the leaf outward exactly once."""

    def __init__(self, backend: Backend, *, now: Callable[[], int]) -> None:
        self.backend = backend
        self.now = now

    def navigate(
        self,
        action: str,
        direction: str,
        *,
        include_current: bool,
        exact_client: Client | None = None,
    ) -> Outcome:
        """Route one semantic action without guessing through a cycle."""

        return self.route(
            action,
            direction,
            include_current=include_current,
            exact_client=exact_client,
        ).outcome

    def route(
        self,
        action: str,
        direction: str,
        *,
        include_current: bool,
        exact_client: Client | None = None,
        continuing_client: Client | None = None,
        continuing_scope: Scope | None = None,
    ) -> RouteResult:
        """Route one action and retain safe provenance for a queued successor."""

        validate_request(action, direction)
        started_at = self.now()
        visited: set[str] = set()
        client = exact_client

        if continuing_scope is not None:
            current = self.backend.refresh_scope(continuing_scope)
            if current is None:
                return RouteResult(Outcome.ERROR, None)
            if continuing_client is not None:
                client = self.backend.refresh_client(continuing_client)
                if client is None or not self._client_displays(client, current):
                    return RouteResult(Outcome.ERROR, None)
            result, current, client = self._enter_scope(
                current,
                action,
                direction,
                started_at,
                visited,
                client=client,
                preserve_client=True,
            )
            if result is not Outcome.DECLINED:
                return RouteResult(result, client, current)
            if client is None:
                return RouteResult(Outcome.ERROR, None)
        elif continuing_client is not None:
            client = self.backend.refresh_client(continuing_client)
            if client is None:
                return RouteResult(Outcome.ERROR, None)
            current = Scope(
                socket=client.socket,
                pane=client.pane,
                session=client.session,
            )
            result, current, client = self._enter_scope(
                current,
                action,
                direction,
                started_at,
                visited,
                client=client,
            )
            if result is not Outcome.DECLINED:
                return RouteResult(result, client, current)
        elif include_current:
            current = self.backend.current_scope()
            if current is not None:
                result, current, client = self._enter_scope(
                    current,
                    action,
                    direction,
                    started_at,
                    visited,
                    preserve_client=True,
                )
                if result is not Outcome.DECLINED:
                    return RouteResult(result, client, current)
                if client is None:
                    return RouteResult(Outcome.ERROR, None)

        if client is None:
            return RouteResult(
                self.backend.terminal(None, action, direction),
                None,
            )

        while True:
            # Selection is only a snapshot. Revalidate before consulting that
            # client's ancestry so a pane/session change cannot redirect a
            # delayed application-origin request into another topology.
            if not self.backend.validate_client(client, started_at):
                return RouteResult(Outcome.ERROR, None)
            parent = self.backend.parent_scope(client)
            if parent is not None:
                result, parent, parent_client = self._enter_scope(
                    parent,
                    action,
                    direction,
                    started_at,
                    visited,
                )
                if result is not Outcome.DECLINED:
                    return RouteResult(result, None, parent)
                if parent_client is None:
                    return RouteResult(Outcome.ERROR, None)
                client = parent_client
                continue

            # Parent discovery can cross a process tree. Revalidate at the
            # dispatch boundary so a focus handoff cannot redirect an inferred
            # application-origin request after its initial selection.
            if not self.backend.validate_client(client, started_at):
                return RouteResult(Outcome.ERROR, None)
            relay = self.backend.relay(client, action, direction)
            if relay is not Outcome.DECLINED:
                return RouteResult(relay, client)
            if not self.backend.validate_client(client, started_at):
                return RouteResult(Outcome.ERROR, None)
            return RouteResult(
                self.backend.terminal(client, action, direction),
                client,
            )

    def _enter_scope(
        self,
        scope: Scope,
        action: str,
        direction: str,
        started_at: int,
        visited: set[str],
        *,
        client: Client | None = None,
        preserve_client: bool = False,
    ) -> tuple[Outcome, Scope, Client | None]:
        """Try one tmux scope before selecting an outward physical route.

        Pane selection is window-local, so it never needs a session or client
        merely to execute. Tab actions are session-relative: first use a
        session shared by every attachment on the pane, and choose one physical
        client only when linked sessions make that logical scope ambiguous.
        If the local action declines, client identity then becomes mandatory
        because only a physical attachment has one well-defined ancestry.
        """

        current = scope
        selected = client
        inspected = False
        if action != "pane-select" and current.session is None:
            resolved, discovered = self.backend.inspect_scope(current, started_at)
            inspected = True
            if resolved is None:
                return Outcome.ERROR, current, None
            if selected is None:
                selected = discovered
            current = resolved
        elif preserve_client and selected is None:
            _resolved, selected = self.backend.inspect_scope(current, started_at)
            inspected = True

        result = self._execute_once(current, action, direction, visited)
        if result is not Outcome.DECLINED or selected is not None:
            return result, current, selected

        # Client identity is deliberately deferred until the action must leave
        # this scope. This keeps direct multi-client tmux sessions fully usable
        # while still failing closed before choosing one ambiguous ancestry.
        if inspected:
            return result, current, None
        return result, current, self.backend.resolve_client(current, started_at)

    @staticmethod
    def _client_displays(client: Client, scope: Scope) -> bool:
        """Confirm retained physical provenance still displays this scope."""

        return (
            client.socket == scope.socket
            and client.pane == scope.pane
            and (scope.session is None or client.session == scope.session)
        )

    def _execute_once(
        self,
        scope: Scope,
        action: str,
        direction: str,
        visited: set[str],
    ) -> Outcome:
        """Execute a scope once and turn malformed ancestry into a safe error."""

        if scope.identity in visited:
            return Outcome.ERROR
        visited.add(scope.identity)
        return self.backend.execute(scope, action, direction)


class SystemBackend:
    """Production adapter for the current process's local tmux scope."""

    DECLINED_MARKER = "__TERMNAV_DECLINED__"
    ERROR_MARKER = "__TERMNAV_ERROR__"
    PANE_FLAGS: ClassVar[dict[str, tuple[str, str]]] = {
        "left": ("pane_at_left", "L"),
        "down": ("pane_at_bottom", "D"),
        "up": ("pane_at_top", "U"),
        "right": ("pane_at_right", "R"),
    }

    def __init__(self, environment: dict[str, str] | None = None) -> None:
        self.environment = dict(os.environ if environment is None else environment)

    def _tmux(self, scope: Scope, *arguments: str) -> subprocess.CompletedProcess[str]:
        environment = self.environment.copy()
        environment.pop("TMUX", None)
        try:
            return subprocess.run(
                ["tmux", "-S", scope.socket, *arguments],
                env=environment,
                text=True,
                capture_output=True,
                check=False,
                timeout=2,
            )
        except (OSError, subprocess.TimeoutExpired):
            return subprocess.CompletedProcess(arguments, 1, "", "")

    def current_scope(self) -> Scope | None:
        tmux_value = self.environment.get("TMUX", "")
        pane = self.environment.get("TMUX_PANE", "")
        if not tmux_value or not pane.startswith("%") or not pane[1:].isdigit():
            return None
        parts = tmux_value.rsplit(",", 2)
        if len(parts) != 3 or not parts[0]:
            return None
        return Scope(socket=parts[0], pane=pane)

    @staticmethod
    def _outcome(result: subprocess.CompletedProcess[str]) -> Outcome:
        output = result.stdout.strip()
        if output == SystemBackend.DECLINED_MARKER:
            return Outcome.DECLINED
        if output == SystemBackend.ERROR_MARKER:
            return Outcome.ERROR
        if result.returncode == 0:
            return Outcome.HANDLED
        return Outcome.ERROR

    def execute(self, scope: Scope, action: str, direction: str) -> Outcome:
        if action == "pane-select":
            edge, flag = self.PANE_FLAGS[direction]
            select = shlex.join(("select-pane", "-t", scope.pane, f"-{flag}"))
            at_edge = shlex.join(("display-message", "-p", self.DECLINED_MARKER))
            inactive = shlex.join(("display-message", "-p", self.ERROR_MARKER))
            inner = shlex.join(
                (
                    "if-shell",
                    "-F",
                    "-t",
                    scope.pane,
                    f"#{{!=:#{{{edge}}},1}}",
                    select,
                    at_edge,
                )
            )
            result = self._tmux(
                scope,
                "if-shell",
                "-F",
                "-t",
                scope.target,
                # A window can be linked into several sessions, where the
                # session-relative window_active value is ambiguous without a
                # client. window_active_clients expresses the actual safety
                # condition: somebody is viewing this window, and this is its
                # active pane.
                "#{&&:#{>:#{window_active_clients},0},#{pane_active}}",
                inner,
                inactive,
            )
            return self._outcome(result)

        if scope.session is None:
            # A linked window has session-relative tab semantics. Routing it
            # without first selecting the originating client could mutate a
            # different attachment's session.
            return Outcome.ERROR

        if action == "tab-select":
            command = "previous-window" if direction == "previous" else "next-window"
            owned = shlex.join(
                (
                    "if-shell",
                    "-F",
                    "#{>:#{session_windows},1}",
                    shlex.join((command, "-t", scope.session)),
                    shlex.join(("display-message", "-p", self.DECLINED_MARKER)),
                )
            )
            result = self._tmux(
                scope,
                "if-shell",
                "-F",
                "-t",
                scope.target,
                "#{&&:#{window_active},#{pane_active}}",
                owned,
                shlex.join(("display-message", "-p", self.ERROR_MARKER)),
            )
            return self._outcome(result)

        return self._move_tab(scope, direction)

    def can_execute(self, scope: Scope, action: str, direction: str) -> bool:
        """Return whether this scope owns an action without mutating it."""

        if action == "pane-select":
            edge, _flag = self.PANE_FLAGS[direction]
            probe = self._tmux(
                scope,
                "display-message",
                "-p",
                "-t",
                scope.target,
                _tmux_format("window_active_clients", "pane_active", edge),
            )
            fields = probe.stdout.strip().split(TMUX_FIELD_SEPARATOR)
            return (
                probe.returncode == 0
                and len(fields) == 3
                and fields[0].isdigit()
                and int(fields[0]) > 0
                and fields[1:] == ["1", "0"]
            )

        if scope.session is None:
            return False
        probe = self._tmux(
            scope,
            "display-message",
            "-p",
            "-t",
            scope.target,
            _tmux_format("window_active", "pane_active", "session_windows"),
        )
        fields = probe.stdout.strip().split(TMUX_FIELD_SEPARATOR)
        return (
            probe.returncode == 0
            and len(fields) == 3
            and fields[0] == "1"
            and fields[1] == "1"
            and fields[2].isdigit()
            and int(fields[2]) > 1
        )

    def _move_tab(self, scope: Scope, direction: str) -> Outcome:
        fields = self._tmux(
            scope,
            "display-message",
            "-p",
            "-t",
            scope.target,
            _tmux_format("session_windows", "window_id"),
        )
        parts = fields.stdout.strip().split(TMUX_FIELD_SEPARATOR)
        if fields.returncode != 0 or len(parts) != 2 or not parts[0].isdigit():
            return Outcome.ERROR
        if int(parts[0]) <= 1:
            return Outcome.DECLINED

        listed = self._tmux(
            scope,
            "list-windows",
            "-t",
            scope.session or "",
            "-F",
            "#{window_id}",
        )
        windows = listed.stdout.splitlines()
        if listed.returncode != 0 or parts[1] not in windows:
            return Outcome.ERROR
        source_index = windows.index(parts[1])
        target_index = source_index + (-1 if direction == "left" else 1)
        if target_index < 0 or target_index >= len(windows):
            return Outcome.HANDLED

        swap = shlex.join(
            (
                "swap-window",
                "-d",
                "-s",
                f"{scope.session}:{parts[1]}",
                "-t",
                f"{scope.session}:{windows[target_index]}",
            )
        )
        result = self._tmux(
            scope,
            "if-shell",
            "-F",
            "-t",
            scope.target,
            "#{&&:#{window_active},#{pane_active}}",
            swap,
            shlex.join(("display-message", "-p", self.ERROR_MARKER)),
        )
        return self._outcome(result)

    def resolve_client(self, scope: Scope, started_at: int) -> Client | None:
        listed = self._tmux(
            scope,
            "list-clients",
            "-F",
            _CLIENT_FORMAT,
        )
        if listed.returncode != 0:
            return None
        candidates = [
            client
            for client in parse_clients(scope.socket, listed.stdout)
            if client.pane == scope.pane
            and (scope.session is None or client.session == scope.session)
        ]
        return choose_client(candidates, started_at=started_at)

    def refresh_client(self, client: Client) -> Client | None:
        """Follow one previously selected physical client to its live scope."""

        if not client.socket:
            return client
        matches = [
            current
            for current in self._all_clients(client.socket)
            if current.pid == client.pid
            and current.tty == client.tty
            and (not client.created or current.created == client.created)
        ]
        return matches[0] if len(matches) == 1 else None

    def inspect_scope(self, scope: Scope, started_at: int) -> tuple[Scope | None, Client | None]:
        """Resolve logical ownership and optional provenance from one snapshot."""

        candidates = [
            client
            for client in self._all_clients(scope.socket)
            if client.pane == scope.pane
            and (scope.session is None or client.session == scope.session)
        ]
        selected = choose_client(candidates, started_at=started_at)
        sessions = {client.session for client in candidates}
        if scope.session is not None:
            resolved = scope
        elif len(sessions) == 1:
            resolved = replace(scope, session=sessions.pop())
        elif selected is not None:
            resolved = replace(scope, session=selected.session)
        else:
            resolved = None
        return resolved, selected

    def refresh_scope(self, scope: Scope) -> Scope | None:
        """Follow a queued action within its stable logical tmux container.

        Session identity follows tab actions across windows. Pane actions need
        only their globally stable window identity, which avoids inventing a
        client dependency for linked or multiply attached sessions.
        """

        target = scope.session
        if target is None:
            window = self._tmux(
                scope,
                "display-message",
                "-p",
                "-t",
                scope.pane,
                "#{window_id}",
            )
            target = window.stdout.strip()
            if window.returncode != 0 or not target.startswith("@"):
                return None
        current = self._tmux(
            scope,
            "display-message",
            "-p",
            "-t",
            target,
            "#{pane_id}",
        )
        pane = current.stdout.strip()
        if current.returncode != 0 or not pane.startswith("%"):
            return None
        return Scope(socket=scope.socket, pane=pane, session=scope.session)

    def parent_scope(self, client: Client) -> Scope | None:
        context = process_tmux_parent(client.pid)
        if context is None:
            return None
        socket, pane = context
        return Scope(socket=socket, pane=pane)

    def validate_client(self, client: Client, started_at: int) -> bool:
        """Revalidate exact identity or inferred ownership before dispatch."""

        current = self._matching_clients(client)
        if client.exact:
            return any(self._same_route(item, client) for item in current)
        selected = choose_client(current, started_at=started_at)
        return selected is not None and self._same_route(selected, client)

    def relay(self, client: Client, action: str, direction: str) -> Outcome:
        parent = process_env(client.pid, "TERMNAV_PARENT_RELAY") or ""
        if not parent:
            return Outcome.DECLINED
        # The router is already long-lived in Neovim and in the tmux hot
        # service. Contact the relay directly instead of paying for a second
        # Python interpreter on every remote-boundary gesture. Keep the final
        # tty check here so PID reuse cannot redirect a delayed request.
        if not tty_matches(client.pid, client.tty):
            return Outcome.ERROR
        reply = send_message(
            parent,
            {
                "v": 2,
                "op": "navigate",
                "scope": WIRE_ACTIONS[action],
                "direction": direction,
                "nonce": new_nonce(),
            },
        )
        result = reply.get("result")
        if result in {"armed", "emitted"}:
            return Outcome.HANDLED
        if result == "declined":
            return Outcome.DECLINED
        return Outcome.ERROR

    @staticmethod
    def _same_route(current: Client, expected: Client) -> bool:
        """Compare stable route fields, allowing an omitted exact session."""

        return (
            current.pid == expected.pid
            and current.tty == expected.tty
            and current.pane == expected.pane
            and current.socket == expected.socket
            and (not expected.created or current.created == expected.created)
            and (not expected.session or current.session == expected.session)
        )

    def _matching_clients(self, expected: Client) -> list[Client]:
        """Read eligible clients still displaying one source pane and session."""

        if not expected.socket:
            return [expected]
        return [
            current
            for current in self._all_clients(expected.socket)
            if current.pane == expected.pane
            and (not expected.session or current.session == expected.session)
        ]

    def _all_clients(self, socket: str) -> list[Client]:
        """Read every eligible attached client from one tmux server."""

        listed = self._tmux(
            Scope(socket=socket, pane=""),
            "list-clients",
            "-F",
            _CLIENT_FORMAT,
        )
        if listed.returncode != 0:
            return []
        return [current for current in parse_clients(socket, listed.stdout) if not current.control]

    def terminal(self, client: Client | None, action: str, direction: str) -> Outcome:
        if client is None:
            pid = os.getpid()
            tty = "/dev/tty"
            termtype = self.environment.get("TERM", "")
        else:
            pid = client.pid
            tty = client.tty
            termtype = client.termtype

        term_program = process_env(pid, "TERM_PROGRAM") or self.environment.get("TERM_PROGRAM", "")
        if termtype.startswith("xterm.js") or term_program == "vscode":
            if action != "tab-select":
                # VS Code has no directional terminal-pane or tab-reordering
                # contract that matches these semantic operations.
                return Outcome.HANDLED
            socket = process_env(pid, "TERMNAV_VSCODE_SOCKET") or ""
            token = process_env(pid, "TERMNAV_VSCODE_TOKEN") or ""
            if socket:
                handled = self._vscode_socket(socket, token, direction)
            else:
                # A process-global backend cannot identify a VS Code window.
                # Keep it opt-in for remote/devserver installations where the
                # window-scoped extension cannot run.
                fallback = process_env(pid, "TERMNAV_VSCODE_FALLBACK_BACKEND") or (
                    self.environment.get("TERMNAV_VSCODE_FALLBACK_BACKEND", "")
                )
                handled = fallback == "mcp" and self._vscode_mcp(direction)
            return Outcome.HANDLED if handled else Outcome.ERROR

        if termtype.startswith(("tmux", "screen")):
            # A nested client with neither a discoverable local parent nor an
            # SSH relay has no trustworthy outer destination. Old WezTerm-only
            # passthrough guessed at that topology; the unified router consumes
            # the request instead of risking a move in an unrelated layer.
            return Outcome.HANDLED
        if action == "pane-select":
            return Outcome.HANDLED

        name = "DOT_SWITCH_TAB" if action == "tab-select" else "DOT_MOVE_TAB"
        return Outcome.HANDLED if self._write_wezterm_var(tty, name, direction) else Outcome.ERROR

    def _vscode_socket(self, socket: str, token: str, direction: str) -> bool:
        """Call the window-scoped VS Code adapter selected by the client."""

        if len(token) != 64 or any(character not in "0123456789abcdef" for character in token):
            return False
        payload = json.dumps({"direction": direction, "token": token}, separators=(",", ":"))
        result = self._curl(
            "--silent",
            "--show-error",
            "--fail",
            "--max-time",
            "2",
            "--unix-socket",
            socket,
            "--header",
            "Content-Type: application/json",
            "--header",
            "Accept: application/json",
            "--data-binary",
            payload,
            "http://localhost/switch-tab",
        )
        return result.returncode == 0

    def _vscode_mcp(self, direction: str) -> bool:
        """Use the explicit devserver fallback without a second dispatch layer."""

        state = self.environment.get("XDG_STATE_HOME", "")
        if not os.path.isabs(state):
            home = self.environment.get("HOME", "")
            if not home:
                return False
            state = str(Path(home) / ".local" / "state")
        try:
            token = (
                (Path(state) / "dot" / "vscode-mcp-auth-token")
                .read_text(encoding="utf-8")
                .rstrip("\n")
            )
        except OSError:
            return False
        if not token:
            return False
        command = (
            "workbench.action.terminal.focusPrevious"
            if direction == "previous"
            else "workbench.action.terminal.focusNext"
        )
        call = {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "execute_command",
                "arguments": {"command": command},
            },
        }
        response = self._vscode_mcp_post(call, token)
        if response is None:
            return False
        if "error" not in response:
            return True
        initialized = self._vscode_mcp_post(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                },
            },
            token,
        )
        if initialized is None or "error" in initialized:
            return False
        retried = self._vscode_mcp_post(call, token)
        return retried is not None and "error" not in retried

    def _vscode_mcp_post(self, request: dict, token: str) -> dict | None:
        """Issue one bounded MCP request and require a JSON object response."""

        port = self.environment.get("VSCODE_MCP_PORT", "9876")
        result = self._curl(
            "--silent",
            "--show-error",
            "--max-time",
            "2",
            "--request",
            "POST",
            f"http://127.0.0.1:{port}/mcp",
            "--header",
            "Content-Type: application/json",
            "--header",
            f"Authorization: Bearer {token}",
            "--data-binary",
            json.dumps(request, separators=(",", ":")),
            capture_output=True,
        )
        if result.returncode != 0:
            return None
        try:
            response = json.loads(result.stdout)
        except (TypeError, ValueError):
            return None
        return response if isinstance(response, dict) else None

    def _curl(
        self, *arguments: str, capture_output: bool = False
    ) -> subprocess.CompletedProcess[str]:
        """Run the one bounded HTTP dependency used by both VS Code adapters."""

        binary = self.environment.get("TERMNAV_VSCODE_CURL", "curl")
        try:
            return subprocess.run(
                [binary, *arguments],
                env=self.environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE if capture_output else subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                text=True,
                check=False,
                timeout=3,
            )
        except (OSError, subprocess.TimeoutExpired):
            return subprocess.CompletedProcess((binary, *arguments), 1, "", "")

    @staticmethod
    def _write_wezterm_var(tty: str, name: str, direction: str) -> bool:
        """Write one direct WezTerm user variable without a helper process."""

        # time_ns plus the process ID gives repeated equal-direction actions a
        # distinct value, which is required for WezTerm's change notification.
        value = f"{direction}:{time.time_ns()}.{os.getpid()}"
        encoded = base64.b64encode(value.encode()).decode("ascii")
        sequence = f"\x1b]1337;SetUserVar={name}={encoded}\x07".encode()
        try:
            descriptor = os.open(
                tty,
                os.O_WRONLY | os.O_NOCTTY | os.O_APPEND,
            )
        except OSError:
            return False
        try:
            os.write(descriptor, sequence)
        except OSError:
            return False
        finally:
            os.close(descriptor)
        return True
