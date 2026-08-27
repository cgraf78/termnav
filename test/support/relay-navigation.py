"""Regression tests for navigation decisions made by the SSH relay endpoint."""

from __future__ import annotations

import importlib.util
import os
import pathlib
import sys
import unittest
from importlib.machinery import SourceFileLoader
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[2]
RELAY = ROOT / "lib" / "termnav" / "relay.py"

spec = importlib.util.spec_from_loader(
    "termnav_relay",
    SourceFileLoader("termnav_relay", str(RELAY)),
)
if spec is None or spec.loader is None:
    raise RuntimeError(f"cannot import {RELAY}")
relay = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = relay
spec.loader.exec_module(relay)
relay.load_server_runtime()
relay.load_navigation_runtime()


class RelayNavigationTest(unittest.TestCase):
    """Keep relay traversal on the shared Termnav policy and live client state."""

    @staticmethod
    def request(scope: str = "window", direction: str = "next") -> dict:
        return {
            "v": 2,
            "op": "navigate",
            "scope": scope,
            "direction": direction,
            "nonce": "aaaaaaaaaaaa",
        }

    def test_top_hop_continues_into_shared_terminal_dispatch(self) -> None:
        client = relay.navigation_client(
            activity=100,
            pid=10,
            tty="/dev/pts/10",
            termtype="xterm.js(6.1)",
            session="$1",
            pane="%1",
            focused=True,
            socket="/tmp/tmux.sock",
        )
        info = {
            "tmux_socket": "/tmp/tmux.sock",
            "pane": "%1",
            "session": "$1",
            "target_pane": "$1:.%1",
            "client": client,
        }

        with (
            mock.patch.object(relay, "local_scope", return_value=info),
            mock.patch.object(relay, "can_handle", return_value=False),
            mock.patch.object(relay, "parent_relay", return_value=""),
            mock.patch.object(
                relay.navigation_backend,
                "validate_client",
                return_value=True,
            ) as validate,
            mock.patch.object(
                relay.navigation_backend,
                "terminal",
                return_value=relay.navigation_outcome.HANDLED,
            ) as terminal,
        ):
            reply = relay.handle_navigate(self.request())

        self.assertEqual({"v": 2, "result": "armed"}, reply)
        validate.assert_called_once()
        terminal.assert_called_once_with(client, "tab-select", "next")

    def test_local_scope_resolves_the_current_client_on_every_request(self) -> None:
        scope = relay.navigation_scope(socket="/tmp/tmux.sock", pane="%1")
        first = relay.navigation_client(
            activity=100,
            pid=10,
            tty="/dev/pts/10",
            termtype="xterm-256color",
            session="$1",
            pane="%1",
            focused=True,
            socket=scope.socket,
        )
        second = relay.navigation_client(
            activity=101,
            pid=11,
            tty="/dev/pts/11",
            termtype="xterm.js(6.1)",
            session="$1",
            pane="%1",
            focused=True,
            socket=scope.socket,
        )

        with (
            mock.patch.object(relay.navigation_backend, "current_scope", return_value=scope),
            mock.patch.object(
                relay.navigation_backend,
                "resolve_client",
                side_effect=(first, second),
            ) as resolve,
            mock.patch.object(relay.time, "time", return_value=100),
        ):
            first_info = relay.local_scope()
            second_info = relay.local_scope()

        self.assertEqual(first, first_info["client"])
        self.assertEqual(second, second_info["client"])
        self.assertEqual(2, resolve.call_count)

    def test_parent_relay_follows_the_selected_live_client(self) -> None:
        client = relay.navigation_client(
            activity=100,
            pid=10,
            tty="/dev/pts/10",
            termtype="tmux-256color",
            session="$1",
            pane="%1",
            socket="/tmp/tmux.sock",
        )
        info = {"client": client}

        with (
            mock.patch.object(
                relay,
                "process_env",
                return_value="/tmp/current-parent.sock",
            ) as environment,
            mock.patch.dict(os.environ, {"TERMNAV_PARENT_RELAY": "/tmp/stale.sock"}),
        ):
            parent = relay.parent_relay(info)

        self.assertEqual("/tmp/current-parent.sock", parent)
        environment.assert_called_once_with(client.pid, "TERMNAV_PARENT_RELAY")

    def test_commit_rejects_a_focus_handoff_after_path_preparation(self) -> None:
        directive = {
            "v": 2,
            "nonce": "aaaaaaaaaaaa",
            "action": "forward",
            "ready": False,
            "claimed": False,
            "prepared_at": 100.0,
            "tmux_socket": "/tmp/tmux.sock",
            "session": "$linked-session",
            "forward_pane": "%1",
            "local_client_tty": "/dev/pts/10",
            "local_client_pid": 10,
            "local_client_created": 100,
            "started_at": 200,
        }
        scope = relay.navigation_scope(socket="/tmp/tmux.sock", pane="%1")

        with (
            mock.patch.object(relay.navigation_backend, "current_scope", return_value=scope),
            mock.patch.object(relay, "_read_directive", return_value=directive),
            mock.patch.object(relay, "validate_directive_client", return_value=False) as validate,
            mock.patch.object(relay, "discard_directive") as discard,
            mock.patch.object(relay, "mark_committed") as commit,
        ):
            reply = relay.handle_commit_path(
                {"v": 2, "op": "commit-path", "nonce": directive["nonce"]}
            )

        self.assertEqual({"v": 2, "result": "error"}, reply)
        validate.assert_called_once_with(directive)
        discard.assert_called_once_with("/tmp/tmux.sock", directive["nonce"])
        commit.assert_not_called()

    def test_lost_commit_reply_waits_for_receipt_instead_of_discarding(self) -> None:
        client = relay.navigation_client(
            activity=100,
            pid=10,
            tty="/dev/pts/10",
            termtype="tmux-256color",
            session="$1",
            pane="%1",
            focused=True,
            socket="/tmp/tmux.sock",
            created=80,
        )
        info = {
            "tmux_socket": client.socket,
            "pane": client.pane,
            "session": client.session,
            "target_pane": "$1:.%1",
            "client": client,
            "started_at": 100,
        }

        with (
            mock.patch.object(relay, "local_scope", return_value=info),
            mock.patch.object(relay, "can_handle", return_value=True),
            mock.patch.object(relay, "parent_relay", return_value="/tmp/parent"),
            mock.patch.object(relay, "_arm_directive_and_seq"),
            mock.patch.object(relay, "validate_directive_client", return_value=True),
            mock.patch.object(relay, "mark_committed", return_value=True),
            mock.patch.object(
                relay,
                "send_message",
                side_effect=(
                    {"v": 2, "result": "prepared"},
                    {"v": 2, "result": "error"},
                ),
            ),
            mock.patch.object(relay, "wait_for_consumption", return_value=False) as wait,
            mock.patch.object(relay, "discard_directive") as discard,
        ):
            reply = relay.handle_navigate(self.request())

        self.assertEqual({"v": 2, "result": "error"}, reply)
        wait.assert_called_once_with(client.socket, "aaaaaaaaaaaa")
        discard.assert_not_called()

    def test_abort_removes_local_hop_before_propagating_outward(self) -> None:
        directive = {
            "v": 2,
            "nonce": "aaaaaaaaaaaa",
            "action": "forward",
            "ready": False,
            "claimed": False,
            "prepared_at": 100.0,
            "tmux_socket": "/tmp/tmux.sock",
            "session": "$1",
            "forward_pane": "%1",
            "local_client_tty": "/dev/pts/10",
            "local_client_pid": 10,
            "local_client_created": 80,
            "started_at": 100,
        }
        current = relay.navigation_scope(socket=directive["tmux_socket"], pane="%1")

        with (
            mock.patch.object(relay.navigation_backend, "current_scope", return_value=current),
            mock.patch.object(relay, "_read_directive", return_value=directive),
            mock.patch.object(relay, "process_env", return_value="/tmp/parent.sock"),
            mock.patch.object(relay, "discard_directive") as discard,
            mock.patch.object(
                relay, "send_message", return_value={"v": 2, "result": "aborted"}
            ) as send,
        ):
            reply = relay.handle_abort_path(
                {"v": 2, "op": "abort-path", "nonce": directive["nonce"]}
            )

        self.assertEqual({"v": 2, "result": "aborted"}, reply)
        discard.assert_called_once_with(directive["tmux_socket"], directive["nonce"])
        send.assert_called_once_with(
            "/tmp/parent.sock",
            {"v": 2, "op": "abort-path", "nonce": directive["nonce"]},
        )

    def test_failed_prepare_aborts_every_already_prepared_parent(self) -> None:
        client = relay.navigation_client(
            activity=100,
            pid=10,
            tty="/dev/pts/10",
            termtype="tmux-256color",
            session="$1",
            pane="%1",
            focused=True,
            socket="/tmp/tmux.sock",
            created=80,
        )
        info = {
            "tmux_socket": client.socket,
            "pane": client.pane,
            "session": client.session,
            "target_pane": "$1:.%1",
            "client": client,
            "started_at": 100,
        }

        with (
            mock.patch.object(relay, "local_scope", return_value=info),
            mock.patch.object(relay, "parent_relay", return_value="/tmp/parent"),
            mock.patch.object(relay, "_arm_directive_and_seq"),
            mock.patch.object(
                relay,
                "send_message",
                side_effect=(
                    {"v": 2, "result": "error"},
                    {"v": 2, "result": "aborted"},
                ),
            ) as send,
            mock.patch.object(relay, "discard_directive") as discard,
        ):
            reply = relay.handle_prepare_path(
                {"v": 2, "op": "prepare-path", "nonce": "aaaaaaaaaaaa"}
            )

        self.assertEqual({"v": 2, "result": "error"}, reply)
        self.assertEqual(
            send.call_args_list,
            [
                mock.call(
                    "/tmp/parent",
                    {"v": 2, "op": "prepare-path", "nonce": "aaaaaaaaaaaa"},
                ),
                mock.call(
                    "/tmp/parent",
                    {"v": 2, "op": "abort-path", "nonce": "aaaaaaaaaaaa"},
                ),
            ],
        )
        discard.assert_called_once_with(client.socket, "aaaaaaaaaaaa")


if __name__ == "__main__":
    unittest.main(verbosity=2)
