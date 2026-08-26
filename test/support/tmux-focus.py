#!/usr/bin/env python3
"""Exercise tmux leaf-focus ownership against isolated real servers."""

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
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock


def wait_until(predicate, description: str, timeout: float = 3.0) -> None:
    """Poll an observable condition so timing variance cannot make tests flaky."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {description}")


def send_message(path: pathlib.Path, message: dict[str, object]) -> dict[str, object]:
    """Exchange one bounded line-delimited message with a relay server."""
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(2)
        client.connect(str(path))
        client.sendall(json.dumps(message, separators=(",", ":")).encode() + b"\n")
        reply = b""
        while b"\n" not in reply and len(reply) <= 512:
            chunk = client.recv(513 - len(reply))
            if not chunk:
                break
            reply += chunk
    decoded = json.loads(reply)
    assert isinstance(decoded, dict), f"relay returned non-object reply: {decoded!r}"
    return decoded


class FocusClaimTest(unittest.TestCase):
    """Validate leased pane claims through the provider's public CLI."""

    focus: pathlib.Path

    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory(prefix="termnav-focus-test-")
        self.socket = pathlib.Path(self.tempdir.name) / "tmux.sock"
        # A custom socket isolates server state, but tmux still reads the
        # caller's default configuration unless told otherwise. Keep provider
        # tests independent of the developer's colors, hooks, and plugins.
        self.run_tmux(
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "focus",
            "sleep",
            "30",
        )
        self.pane = self.tmux("display-message", "-p", "#{pane_id}")

    def tearDown(self) -> None:
        subprocess.run(
            ["tmux", "-S", str(self.socket), "kill-server"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        self.tempdir.cleanup()

    def run_tmux(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["tmux", "-S", str(self.socket), *arguments],
            text=True,
            capture_output=True,
            check=True,
        )

    def tmux(self, *arguments: str) -> str:
        return self.run_tmux(*arguments).stdout.strip()

    def pane_claim(self) -> str:
        return self.tmux("show-options", "-pqv", "-t", self.pane, "@termnav_child_focus")

    def pane_claim_owner(self) -> str:
        return self.pane_claim().partition(":")[0]

    def pane_active_style(self) -> str:
        return self.tmux(
            "show-options",
            "-pqv",
            "-t",
            self.pane,
            "window-active-style",
        )

    def pane_restore_style(self) -> str:
        return self.tmux(
            "show-options",
            "-pqv",
            "-t",
            self.pane,
            "@termnav_child_focus_restore_active_style",
        )

    def configure_inactive_style(self) -> None:
        self.run_tmux(
            "set-option",
            "-g",
            "@termnav_inactive_style",
            "bg=#010d17",
        )

    def focus_result(
        self,
        command: str,
        token: str,
        lease_ms: int | None = None,
        *,
        check: bool,
    ) -> subprocess.CompletedProcess[str]:
        """Run the public focus CLI so tests share one exact invocation shape."""
        arguments = [
            str(self.focus),
            command,
            "--parent-tmux",
            str(self.socket),
            "--parent-pane",
            self.pane,
            "--token",
            token,
        ]
        if lease_ms is not None:
            arguments.extend(("--lease-ms", str(lease_ms)))
        return subprocess.run(
            arguments,
            text=True,
            capture_output=True,
            check=check,
        )

    def focus_command(self, command: str, token: str, lease_ms: int | None = None) -> None:
        self.focus_result(command, token, lease_ms, check=True)

    def test_claim_expires_without_a_release(self) -> None:
        self.configure_inactive_style()
        token = "aaaaaaaaaaaaaaaaaaaaaaaa"
        self.focus_command("claim", token, 250)
        self.assertEqual(token, self.pane_claim_owner())
        self.assertEqual("bg=#010d17", self.pane_active_style())
        wait_until(
            lambda: self.pane_claim() == "" and self.pane_active_style() == "",
            "expired claim and pane-style cleanup",
        )

    def test_claim_applies_parent_style_and_release_restores_inheritance(self) -> None:
        self.configure_inactive_style()
        token = "444444444444444444444444"
        self.assertEqual("", self.pane_active_style())
        self.focus_command("claim", token, 500)
        self.assertEqual("bg=#010d17", self.pane_active_style())
        self.focus_command("release", token)
        self.assertEqual("", self.pane_active_style())

    def test_claim_without_inactive_style_keeps_existing_behavior(self) -> None:
        token = "666666666666666666666666"
        self.focus_command("claim", token, 500)
        self.assertEqual(token, self.pane_claim_owner())
        self.assertEqual("", self.pane_active_style())
        self.assertEqual("", self.pane_restore_style())
        self.focus_command("release", token)

    def test_invalid_inactive_style_preserves_the_core_focus_claim(self) -> None:
        self.run_tmux(
            "set-option",
            "-g",
            "@termnav_inactive_style",
            "not-a-tmux-style",
        )
        token = "777777777777777777777777"
        result = self.focus_result("claim", token, 500, check=False)
        self.assertEqual(0, result.returncode)
        self.assertEqual(token, self.pane_claim_owner())
        self.assertEqual("", self.pane_restore_style())
        self.assertEqual("", self.pane_active_style())
        self.focus_command("release", token)

    def test_invalid_inactive_style_restores_an_existing_override(self) -> None:
        self.run_tmux(
            "set-option",
            "-p",
            "-t",
            self.pane,
            "window-active-style",
            "bg=#123456",
        )
        self.run_tmux(
            "set-option",
            "-g",
            "@termnav_inactive_style",
            "not-a-tmux-style",
        )
        token = "888888888888888888888888"
        result = self.focus_result("claim", token, 500, check=False)
        self.assertEqual(0, result.returncode)
        self.assertEqual(token, self.pane_claim_owner())
        self.assertEqual("", self.pane_restore_style())
        self.assertEqual("bg=#123456", self.pane_active_style())
        self.focus_command("release", token)

    def test_concurrent_replacement_restores_the_original_pane_style(self) -> None:
        self.configure_inactive_style()
        self.run_tmux(
            "set-option",
            "-p",
            "-t",
            self.pane,
            "window-active-style",
            "bg=#123456",
        )
        tokens = (
            "bbbbbbbbbbbbbbbbbbbbbbbb",
            "cccccccccccccccccccccccc",
        )
        barrier = threading.Barrier(3)
        results: list[subprocess.CompletedProcess[str]] = []

        def publish(token: str) -> None:
            barrier.wait()
            results.append(self.focus_result("claim", token, 1000, check=False))

        threads = [threading.Thread(target=publish, args=(token,)) for token in tokens]
        for thread in threads:
            thread.start()
        barrier.wait()
        for thread in threads:
            thread.join(timeout=3)

        self.assertEqual(2, len(results))
        self.assertTrue(all(result.returncode == 0 for result in results))
        owner = self.pane_claim_owner()
        self.assertIn(owner, tokens)
        self.assertEqual("bg=#010d17", self.pane_active_style())
        loser = next(token for token in tokens if token != owner)
        self.focus_command("release", loser)
        self.assertEqual(owner, self.pane_claim_owner())
        self.assertEqual("bg=#010d17", self.pane_active_style())
        self.focus_command("release", owner)
        self.assertEqual("", self.pane_claim())
        self.assertEqual("bg=#123456", self.pane_active_style())

    def test_malformed_restore_marker_preserves_the_current_pane_style(self) -> None:
        token = "999999999999999999999999"
        self.run_tmux(
            "set-option",
            "-p",
            "-t",
            self.pane,
            "window-active-style",
            "bg=#abcdef",
        )
        self.run_tmux(
            "set-option",
            "-p",
            "-t",
            self.pane,
            "@termnav_child_focus_restore_active_style",
            "not-json",
        )
        self.run_tmux(
            "set-option",
            "-p",
            "-t",
            self.pane,
            "@termnav_child_focus",
            f"{token}:9999999999999999",
        )
        self.focus_command("release", token)
        self.assertEqual("", self.pane_claim())
        self.assertEqual("", self.pane_restore_style())
        self.assertEqual("bg=#abcdef", self.pane_active_style())

    def test_invalid_saved_style_cannot_trap_an_expired_claim(self) -> None:
        token = "dddddddddddddddddddddddd"
        self.run_tmux(
            "set-option",
            "-p",
            "-t",
            self.pane,
            "@termnav_child_focus_restore_active_style",
            '{"had_override":true,"value":"not-a-tmux-style"}',
        )
        self.run_tmux(
            "set-option",
            "-p",
            "-t",
            self.pane,
            "@termnav_child_focus",
            f"{token}:1",
        )
        result = subprocess.run(
            [
                str(self.focus),
                "expire",
                "--parent-tmux",
                str(self.socket),
                "--parent-pane",
                self.pane,
            ],
            text=True,
            capture_output=True,
            check=False,
            timeout=2,
        )
        self.assertEqual(0, result.returncode)
        self.assertEqual("", self.pane_claim())
        self.assertEqual("", self.pane_restore_style())
        self.assertEqual("", self.pane_active_style())

    def test_release_restores_an_existing_pane_active_style(self) -> None:
        self.configure_inactive_style()
        self.run_tmux(
            "set-option",
            "-p",
            "-t",
            self.pane,
            "window-active-style",
            "bg=#123456,italics",
        )
        token = "555555555555555555555555"
        self.focus_command("claim", token, 500)
        self.assertEqual("bg=#010d17", self.pane_active_style())
        self.focus_command("release", token)
        self.assertEqual("bg=#123456,italics", self.pane_active_style())

    def test_renewal_extends_the_same_claim(self) -> None:
        token = "111111111111111111111111"
        self.focus_command("claim", token, 250)
        time.sleep(0.15)
        self.focus_command("claim", token, 500)
        time.sleep(0.2)
        self.assertEqual(token, self.pane_claim_owner())
        wait_until(lambda: self.pane_claim() == "", "renewed claim cleanup")

    def test_stale_release_cannot_clear_a_replacement(self) -> None:
        self.configure_inactive_style()
        old_token = "222222222222222222222222"
        new_token = "333333333333333333333333"
        self.focus_command("claim", old_token, 500)
        self.focus_command("claim", new_token, 500)
        self.focus_command("release", old_token)
        self.assertEqual(new_token, self.pane_claim_owner())
        self.assertEqual("bg=#010d17", self.pane_active_style())
        self.focus_command("release", new_token)
        self.assertEqual("", self.pane_claim())
        self.assertEqual("", self.pane_active_style())


class FocusLibraryTest(unittest.TestCase):
    """Cover portable parsing and owner-only runtime state boundaries."""

    module: object
    process_info: object

    def test_macos_process_environment_parser_preserves_values_with_spaces(
        self,
    ) -> None:
        output = (
            "tmux attach TMUX=/tmp/outer.sock,123,0 TMUX_PANE=%42 "
            "LABEL=one value with spaces PATH=/usr/bin:/bin"
        )
        self.assertEqual(
            "/tmp/outer.sock,123,0",
            self.process_info.parse_environment(output, "TMUX"),
        )
        self.assertEqual(
            "one value with spaces",
            self.process_info.parse_environment(output, "LABEL"),
        )
        self.assertIsNone(self.process_info.parse_environment(output, "MUX"))

    def test_relative_runtime_directory_falls_back_to_private_tmp_state(self) -> None:
        with mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": "relative"}):
            path = self.module.runtime_dir()
        self.assertEqual(pathlib.Path(tempfile.gettempdir()), path.parents[1])
        self.assertEqual(0o700, path.stat().st_mode & 0o777)

    def test_runtime_directory_rejects_a_symlinked_focus_leaf(self) -> None:
        with tempfile.TemporaryDirectory(prefix="termnav-focus-runtime-") as root:
            base = pathlib.Path(root)
            owner_root = base / f"termnav-{os.getuid()}"
            owner_root.mkdir(mode=0o700)
            target = base / "target"
            target.mkdir(mode=0o700)
            (owner_root / "focus").symlink_to(target, target_is_directory=True)
            with (
                mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(base)}),
                self.assertRaises(OSError),
            ):
                self.module.runtime_dir()

    def test_lock_names_hash_unbounded_socket_and_tty_values(self) -> None:
        with (
            tempfile.TemporaryDirectory(prefix="termnav-focus-lock-") as root,
            mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": root}),
        ):
            path = self.module.lock_path("watch", "/" + "s" * 500, "/dev/pts/1")
        self.assertRegex(path.name, r"^watch-[0-9a-f]{24}\.lock$")

    def test_expirer_spawn_failure_is_reported_for_fail_closed_cleanup(self) -> None:
        with mock.patch.object(self.module.subprocess, "Popen", side_effect=OSError):
            started = self.module.start_expirer(
                pathlib.Path("/tmp/termnav-tmux-focus"), "/tmp/tmux.sock", "%1"
            )
        self.assertFalse(started)

    def test_boolean_lease_is_rejected_before_any_tmux_mutation(self) -> None:
        with mock.patch.object(self.module, "run_tmux") as run_tmux:
            published = self.module.claim(
                "/tmp/tmux.sock",
                "%1",
                "999999999999999999999999",
                True,
            )
        self.assertIsNone(published)
        run_tmux.assert_not_called()

    def test_invalid_watch_timing_is_rejected_before_parent_discovery(self) -> None:
        with mock.patch.object(self.module, "parent_for_client") as parent:
            status = self.module.watch(
                pathlib.Path("/tmp/termnav-tmux-focus"),
                "/tmp/tmux.sock",
                123,
                "/dev/pts/1",
                100,
                100,
            )
        self.assertEqual(2, status)
        parent.assert_not_called()

    def test_refocus_during_style_sync_cannot_stop_the_current_watcher(self) -> None:
        with (
            mock.patch.object(
                self.module,
                "client_focused",
                side_effect=(False, True),
            ),
            mock.patch.object(
                self.module,
                "sync_client_style",
                return_value=True,
            ) as sync_client_style,
            mock.patch.object(self.module, "lock_path") as lock_path,
        ):
            status = self.module.stop_watch(
                pathlib.Path("/tmp/termnav-tmux-focus"),
                "/tmp/tmux.sock",
                123,
                "/dev/pts/1",
            )
        self.assertEqual(0, status)
        self.assertEqual(2, sync_client_style.call_count)
        lock_path.assert_not_called()


class RelayFocusTest(unittest.TestCase):
    """Validate one-hop focus transport without involving a real SSH host."""

    relay: pathlib.Path

    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory(prefix="termnav-focus-relay-")
        self.root = pathlib.Path(self.tempdir.name)
        self.tmux_socket = self.root / "tmux.sock"
        subprocess.run(
            [
                "tmux",
                "-S",
                str(self.tmux_socket),
                "new-session",
                "-d",
                "-s",
                "focus",
                "sleep",
                "30",
            ],
            check=True,
        )
        self.pane = self.tmux("display-message", "-p", "#{pane_id}")
        self.session = self.tmux("display-message", "-p", "#{session_id}")
        self.processes: list[subprocess.Popen[bytes]] = []
        environment = os.environ.copy()
        environment.pop("TMUX", None)
        environment.pop("TMUX_PANE", None)
        # The devserver test runner itself uses TERM=dumb. A real tmux client
        # exits immediately under that value, so give this synthetic terminal a
        # stable capability set just as the other PTY integration harnesses do.
        environment["TERM"] = "xterm-256color"
        client = subprocess.Popen(
            [
                sys.executable,
                str(pathlib.Path(__file__).with_name("tmux-client.py")),
                "-S",
                str(self.tmux_socket),
                "attach-session",
                "-t",
                "focus",
            ],
            env=environment,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.processes.append(client)

        def client_attached() -> bool:
            listed = subprocess.run(
                [
                    "tmux",
                    "-S",
                    str(self.tmux_socket),
                    "list-clients",
                    "-F",
                    "#{client_pid}",
                ],
                text=True,
                capture_output=True,
                check=False,
            )
            return listed.returncode == 0 and bool(listed.stdout.strip())

        wait_until(
            client_attached,
            "focus relay tmux client",
        )

    def tearDown(self) -> None:
        for process in self.processes:
            process.terminate()
        for process in self.processes:
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
        subprocess.run(
            ["tmux", "-S", str(self.tmux_socket), "kill-server"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        self.tempdir.cleanup()

    def tmux(self, *arguments: str) -> str:
        return subprocess.run(
            ["tmux", "-S", str(self.tmux_socket), *arguments],
            text=True,
            capture_output=True,
            check=True,
        ).stdout.strip()

    def pane_claim_owner(self) -> str:
        value = self.tmux("show-options", "-pqv", "-t", self.pane, "@termnav_child_focus")
        return value.partition(":")[0]

    def start_relay(
        self,
        path: pathlib.Path,
        parent: pathlib.Path | None = None,
        owns_tmux: bool = False,
        extra_environment: dict[str, str] | None = None,
        navigation_log: pathlib.Path | None = None,
    ) -> None:
        environment = os.environ.copy()
        environment.pop("TMUX", None)
        environment.pop("TMUX_PANE", None)
        environment.pop("TERMNAV_PARENT_RELAY", None)
        if parent is not None:
            environment["TERMNAV_PARENT_RELAY"] = str(parent)
        if owns_tmux:
            environment["TMUX"] = f"{self.tmux_socket},1,0"
            environment["TMUX_PANE"] = self.pane
        if extra_environment is not None:
            environment.update(extra_environment)
        command = [str(self.relay), "serve", "--socket", str(path)]
        if navigation_log is not None:
            command = [
                sys.executable,
                str(pathlib.Path(__file__).resolve().with_name("relay-server.py")),
                "--socket",
                str(path),
                "--log",
                str(navigation_log),
            ]
        process = subprocess.Popen(
            command,
            env=environment,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.processes.append(process)
        wait_until(path.is_socket, f"relay listener {path}")

    def test_relay_claim_is_scoped_to_its_immediate_parent_pane(self) -> None:
        relay_socket = self.root / "relay.sock"
        self.start_relay(relay_socket, owns_tmux=True)
        token = "444444444444444444444444"
        claim = send_message(
            relay_socket,
            {"v": 2, "op": "focus", "state": "claim", "token": token, "lease_ms": 500},
        )
        self.assertEqual({"v": 2, "result": "claimed"}, claim)
        self.assertEqual(token, self.pane_claim_owner())
        release = send_message(
            relay_socket,
            {"v": 2, "op": "focus", "state": "release", "token": token},
        )
        self.assertEqual({"v": 2, "result": "released"}, release)
        self.assertEqual("", self.pane_claim_owner())

    def test_tmuxless_relay_forwards_to_the_nearest_parent_scope(self) -> None:
        outer_socket = self.root / "outer-relay.sock"
        inner_socket = self.root / "inner-relay.sock"
        self.start_relay(outer_socket, owns_tmux=True)
        self.start_relay(inner_socket, parent=outer_socket)
        token = "555555555555555555555555"
        reply = send_message(
            inner_socket,
            {"v": 2, "op": "focus", "state": "claim", "token": token, "lease_ms": 500},
        )
        self.assertEqual({"v": 2, "result": "claimed"}, reply)
        self.assertEqual(token, self.pane_claim_owner())

    def test_focus_heartbeat_does_not_block_navigation_admission(self) -> None:
        mock_bin = self.root / "mock-bin"
        mock_bin.mkdir()
        fake_tmux = mock_bin / "tmux"
        fake_tmux.write_text(
            "#!/usr/bin/env bash\nsleep 0.35\nexit 0\n",
            encoding="utf-8",
        )
        fake_tmux.chmod(0o755)
        relay_socket = self.root / "concurrent-relay.sock"
        log = self.root / "navigation.log"
        self.start_relay(
            relay_socket,
            owns_tmux=True,
            extra_environment={
                "PATH": f"{mock_bin}:{os.environ['PATH']}",
            },
            navigation_log=log,
        )
        focus_reply: list[dict[str, object]] = []
        request = {
            "v": 2,
            "op": "focus",
            "state": "claim",
            "token": "666666666666666666666666",
            "lease_ms": 500,
        }
        thread = threading.Thread(
            target=lambda: focus_reply.append(send_message(relay_socket, request))
        )
        thread.start()
        time.sleep(0.1)
        navigation = send_message(
            relay_socket,
            {
                "v": 2,
                "op": "navigate",
                "scope": "pane",
                "direction": "left",
                "nonce": "abcdefabcdef",
            },
        )
        thread.join(timeout=3)
        self.assertEqual("armed", navigation.get("result"))
        self.assertTrue(focus_reply)

    def test_invalid_focus_messages_fail_without_mutating_tmux(self) -> None:
        relay_socket = self.root / "invalid-relay.sock"
        self.start_relay(relay_socket, owns_tmux=True)
        invalid_requests = (
            {"v": 2, "op": "focus", "state": "claim", "token": "short"},
            {
                "v": 2,
                "op": "focus",
                "state": "claim",
                "token": "777777777777777777777777",
                "lease_ms": 49,
            },
            {
                "v": 2,
                "op": "focus",
                "state": "replace",
                "token": "777777777777777777777777",
            },
            {
                "v": 2,
                "op": "focus",
                "state": "claim",
                "token": "777777777777777777777777",
                "lease_ms": True,
            },
        )
        for request in invalid_requests:
            with self.subTest(request=request):
                self.assertEqual("error", send_message(relay_socket, request)["result"])
                self.assertEqual("", self.pane_claim_owner())

    def test_focus_without_a_parent_scope_is_declined(self) -> None:
        relay_socket = self.root / "top-relay.sock"
        self.start_relay(relay_socket)
        reply = send_message(
            relay_socket,
            {
                "v": 2,
                "op": "focus",
                "state": "claim",
                "token": "888888888888888888888888",
                "lease_ms": 500,
            },
        )
        self.assertEqual({"v": 2, "result": "declined"}, reply)


class PtyClient:
    """Attach one isolated tmux client through a controllable pseudo-terminal."""

    def __init__(self, socket_path: pathlib.Path, environment: dict[str, str]):
        self.socket_path = socket_path
        self.pid, self.master = pty.fork()
        if self.pid == 0:
            os.environ.clear()
            os.environ.update(environment)
            os.environ["TERM"] = "xterm-256color"
            os.execvp(
                "tmux",
                ["tmux", "-S", str(socket_path), "attach-session", "-t", "focus"],
            )
        wait_until(self._attached, "outer tmux client attachment")
        self.pump(0.3)

    def _attached(self) -> bool:
        clients = subprocess.run(
            [
                "tmux",
                "-S",
                str(self.socket_path),
                "list-clients",
                "-F",
                "#{client_pid}",
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        return str(self.pid) in clients.stdout.splitlines()

    def pump(self, duration: float) -> None:
        """Drain output and answer tmux's terminal-capability readiness query."""
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.master], [], [], 0.02)
            if not ready:
                continue
            payload = os.read(self.master, 65536)
            if b"\x1b[?996n" in payload:
                os.write(self.master, b"\x1b[?997;1n")

    def focus(self) -> None:
        os.write(self.master, b"\x1b[I")
        self.pump(0.1)

    def blur(self) -> None:
        os.write(self.master, b"\x1b[O")
        self.pump(0.1)

    def close(self) -> None:
        try:
            os.kill(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 2
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


class NestedFocusTest(unittest.TestCase):
    """Exercise real focus propagation between locally nested tmux servers."""

    focus: pathlib.Path
    module: object

    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory(prefix="termnav-focus-nested-")
        self.root = pathlib.Path(self.tempdir.name)
        self.runtime = self.root / "runtime"
        self.runtime.mkdir(mode=0o700)
        self.environment = os.environ.copy()
        # The harness must create a genuine top-level terminal. Inheriting the
        # developer's own tmux or SSH relay would turn the fixture into another
        # child of the live session and invalidate both isolation and counts.
        self.environment.pop("TMUX", None)
        self.environment.pop("TMUX_PANE", None)
        self.environment.pop("TERMNAV_PARENT_RELAY", None)
        self.environment.pop("TERMNAV_TMUX_SESSION", None)
        self.environment["XDG_RUNTIME_DIR"] = str(self.runtime)
        self.environment["PATH"] = f"{self.focus.parent}:{self.environment['PATH']}"
        command = (
            f"{shlex.quote(str(self.focus))} watch "
            "--tmux-socket #{q:socket_path} --client-pid #{client_pid} "
            "--client-tty #{q:client_tty} --lease-ms 600 --interval-ms 150"
        )
        stop_command = (
            f"{shlex.quote(str(self.focus))} stop "
            "--tmux-socket #{q:socket_path} --client-pid #{client_pid} "
            "--client-tty #{q:client_tty}"
        )
        sync_command = (
            f"{shlex.quote(str(self.focus))} sync "
            "--tmux-socket #{q:socket_path} --client-pid #{client_pid} "
            "--client-tty #{q:client_tty}"
        )
        self.config = self.root / "tmux.conf"
        self.config.write_text(
            "set -g focus-events on\n"
            "set -g status off\n"
            "set -g window-active-style 'bg=#111111'\n"
            "set -g @termnav_inactive_style 'bg=#222222'\n"
            f"set-hook -g client-attached[110] {{ run-shell -b '{command}' }}\n"
            f"set-hook -g client-focus-in[110] {{ run-shell -b '{command}' }}\n"
            f"set-hook -g client-focus-out[110] {{ run-shell -b '{stop_command}' }}\n"
            f"set-hook -g client-detached[110] {{ run-shell -b '{stop_command}' }}\n"
            f"set-hook -g after-select-pane[110] {{ run-shell -b '{sync_command}' }}\n"
            f"set-hook -g after-select-window[110] {{ run-shell -b '{sync_command}' }}\n"
            f"set-hook -g client-session-changed[110] {{ run-shell -b '{sync_command}' }}\n",
            encoding="utf-8",
        )
        self.sockets: list[pathlib.Path] = []
        self.clients: list[PtyClient] = []

    def tearDown(self) -> None:
        for client in reversed(self.clients):
            client.close()
        for socket_path in reversed(self.sockets):
            subprocess.run(
                ["tmux", "-S", str(socket_path), "kill-server"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )

        # tmux runs focus hooks in background processes. Killing the isolated
        # servers requests shutdown, but deleting their runtime directory can
        # race a final watcher/expirer write under load. Wait for the observable
        # lifecycle boundary instead of guessing how long those children need.
        def helpers_stopped() -> bool:
            return all(
                not self.helper_processes(command, socket_path)
                for socket_path in self.sockets
                for command in ("watch", "expire")
            )

        try:
            wait_until(helpers_stopped, "all isolated focus helpers to exit")
        except AssertionError as error:
            remaining = {
                f"{socket_path.name}:{command}": self.helper_process_details(command, socket_path)
                for socket_path in self.sockets
                for command in ("watch", "expire")
                if self.helper_processes(command, socket_path)
            }
            raise AssertionError(f"{error}; remaining helpers: {remaining}") from error
        self.tempdir.cleanup()

    def tmux(self, socket_path: pathlib.Path, *arguments: str) -> str:
        return subprocess.run(
            ["tmux", "-S", str(socket_path), *arguments],
            env=self.environment,
            text=True,
            capture_output=True,
            check=True,
        ).stdout.strip()

    def new_server(self, name: str, command: str) -> pathlib.Path:
        socket_path = self.root / f"{name}.sock"
        self.sockets.append(socket_path)
        self.tmux(
            socket_path,
            "-f",
            str(self.config),
            "new-session",
            "-d",
            "-s",
            "focus",
            command,
        )
        return socket_path

    def pane_claim(self, socket_path: pathlib.Path, pane: str) -> str:
        return self.tmux(
            socket_path,
            "show-options",
            "-pqv",
            "-t",
            pane,
            "@termnav_child_focus",
        )

    def pane_active_style(self, socket_path: pathlib.Path, pane: str) -> str:
        """Return only a pane-local active-style override, not inheritance."""
        return self.tmux(
            socket_path,
            "show-options",
            "-pqv",
            "-t",
            pane,
            "window-active-style",
        )

    def pane_blurred(self, socket_path: pathlib.Path, pane: str) -> str:
        return self.tmux(
            socket_path,
            "show-options",
            "-pqv",
            "-t",
            pane,
            "@termnav_client_unfocused",
        )

    def client_tty(self, socket_path: pathlib.Path) -> str:
        clients = self.tmux(socket_path, "list-clients", "-F", "#{client_tty}")
        values = clients.splitlines()
        self.assertEqual(1, len(values), f"expected one client on {socket_path}")
        return values[0]

    def client_identity(self, socket_path: pathlib.Path) -> tuple[int, str]:
        """Return the sole real client identity for one nested test server."""
        value = self.tmux(
            socket_path,
            "list-clients",
            "-F",
            "#{client_pid} #{client_tty}",
        )
        fields = value.split(maxsplit=1)
        self.assertEqual(2, len(fields), value)
        pid, tty = fields
        self.assertTrue(pid.isdigit() and tty, value)
        return int(pid), tty

    def visual_leaf_count(self, *socket_paths: pathlib.Path) -> int:
        # This is the consumer-side invariant in its smallest form. It contains
        # no nesting heuristic: each server knows only whether its own client is
        # focused and whether its active pane has accepted a child claim.
        leaf_format = (
            "#{&&:#{pane_active},"
            "#{&&:#{m:*focused*,#{client_flags}},"
            "#{==:#{@termnav_child_focus},}}}"
        )
        count = 0
        for socket_path in socket_paths:
            client_tty = self.client_tty(socket_path)
            panes = self.tmux(socket_path, "list-panes", "-a", "-F", "#{pane_id}")
            for pane in panes.splitlines():
                value = self.tmux(
                    socket_path,
                    "display-message",
                    "-c",
                    client_tty,
                    "-p",
                    "-t",
                    pane,
                    leaf_format,
                )
                count += int(value or "0")
        return count

    def focus_snapshot(self, *socket_paths: pathlib.Path) -> str:
        rows = []
        for socket_path in socket_paths:
            clients = self.tmux(
                socket_path,
                "list-clients",
                "-F",
                "client=#{client_tty} flags=#{client_flags} pane=#{pane_id}",
            )
            panes = self.tmux(
                socket_path,
                "list-panes",
                "-a",
                "-F",
                "pane=#{pane_id} active=#{pane_active} claim=#{@termnav_child_focus}",
            )
            rows.append(f"{socket_path.name}: {clients}; {panes}")
        return " | ".join(rows)

    def helper_processes(self, command: str, socket_path: pathlib.Path) -> list[int]:
        proc_root = pathlib.Path("/proc")
        if proc_root.is_dir():
            processes = []
            for process_dir in proc_root.iterdir():
                if not process_dir.name.isdigit():
                    continue
                try:
                    arguments = [
                        value.decode(errors="replace")
                        for value in (process_dir / "cmdline").read_bytes().split(b"\0")
                        if value
                    ]
                except OSError:
                    continue
                for index, value in enumerate(arguments[:-1]):
                    if value == str(self.focus) and arguments[index + 1] == command:
                        if str(socket_path) in arguments:
                            processes.append(int(process_dir.name))
                        break
            return processes

        listing = subprocess.run(
            ["ps", "ax", "-o", "pid=", "-o", "command="],
            text=True,
            capture_output=True,
            check=True,
        ).stdout
        marker = f"{self.focus} {command} "
        socket_marker = str(socket_path)
        processes = []
        for line in listing.splitlines():
            pid, separator, arguments = line.strip().partition(" ")
            if separator and pid.isdigit() and marker in arguments and socket_marker in arguments:
                processes.append(int(pid))
        return processes

    def helper_process_details(self, command: str, socket_path: pathlib.Path) -> list[str]:
        details = []
        for pid in self.helper_processes(command, socket_path):
            try:
                arguments = (pathlib.Path("/proc") / str(pid) / "cmdline").read_bytes()
                status_lines = (
                    (pathlib.Path("/proc") / str(pid) / "status").read_text().splitlines()
                )
                status = " ".join(
                    line for line in status_lines if line.startswith(("State:", "PPid:"))
                )
                locks = []
                for descriptor in (pathlib.Path("/proc") / str(pid) / "fd").iterdir():
                    try:
                        target = os.readlink(descriptor)
                    except OSError:
                        continue
                    if "termnav-" in target and target.endswith(".lock"):
                        locks.append(target)
                details.append(
                    f"{status} locks={locks} "
                    + arguments.replace(b"\0", b" ").decode(errors="replace")
                )
            except OSError:
                details.append(str(pid))
        return details

    def helper_lock_holders(self, command: str, socket_path: pathlib.Path) -> list[int]:
        """Count logical workers, ignoring an interpreter-launcher parent process."""
        holders = []
        for pid in self.helper_processes(command, socket_path):
            descriptor_root = pathlib.Path("/proc") / str(pid) / "fd"
            if not descriptor_root.is_dir():
                # macOS does not expose /proc. Its normal Python executable does
                # not add the devserver's interpreter-launcher parent process.
                holders.append(pid)
                continue
            for descriptor in descriptor_root.iterdir():
                try:
                    target = os.readlink(descriptor)
                except OSError:
                    continue
                if f"/{command}-" in target and target.endswith(".lock"):
                    holders.append(pid)
                    break
        return holders

    def test_local_nested_client_claims_and_releases_its_exact_parent_pane(
        self,
    ) -> None:
        inner = self.new_server("inner", "sleep 30")
        inner_active = self.tmux(inner, "display-message", "-p", "#{pane_id}")
        self.tmux(inner, "split-window", "-d", "sleep 30")
        outer = self.new_server(
            "outer",
            f"env TERM=tmux-256color tmux -S {shlex.quote(str(inner))} attach-session -t focus",
        )
        nested_pane = self.tmux(outer, "display-message", "-p", "#{pane_id}")
        sibling_pane = self.tmux(
            outer,
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "sleep 30",
        )
        client = PtyClient(outer, self.environment)
        self.clients.append(client)
        client.focus()

        wait_until(
            lambda: bool(self.pane_claim(outer, nested_pane)),
            "focused child claim on its parent pane",
        )
        self.assertEqual("", self.pane_claim(outer, sibling_pane))

        self.tmux(outer, "select-pane", "-t", sibling_pane)
        wait_until(
            lambda: self.pane_claim(outer, nested_pane) == "",
            "child release after parent pane loses focus",
        )
        wait_until(
            lambda: self.pane_active_style(inner, inner_active) == "bg=#222222",
            "nested leaf dimming after its client loses focus",
        )

        self.tmux(outer, "select-pane", "-t", nested_pane)
        wait_until(
            lambda: (
                bool(self.pane_claim(outer, nested_pane))
                and self.pane_active_style(inner, inner_active) == ""
            ),
            "nested leaf restoring when its client regains focus",
        )

    def test_direct_client_focus_changes_repaint_its_active_leaf(self) -> None:
        direct = self.new_server("direct", "sleep 30")
        pane = self.tmux(direct, "display-message", "-p", "#{pane_id}")
        client = PtyClient(direct, self.environment)
        self.clients.append(client)
        client.focus()
        wait_until(
            lambda: self.pane_active_style(direct, pane) == "",
            "focused direct leaf style",
        )

        client.blur()
        wait_until(
            lambda: (
                self.pane_blurred(direct, pane) == "1"
                and self.pane_active_style(direct, pane) == "bg=#222222"
            ),
            "unfocused direct leaf style",
        )

        client.focus()
        wait_until(
            lambda: (
                self.pane_blurred(direct, pane) == "" and self.pane_active_style(direct, pane) == ""
            ),
            "refocused direct leaf style",
        )

    def test_selecting_a_stale_dimmed_pane_repairs_its_style(self) -> None:
        direct = self.new_server("selection-repair", "sleep 30")
        stale = self.tmux(
            direct,
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "sleep 30",
        )
        client = PtyClient(direct, self.environment)
        self.clients.append(client)
        client.focus()
        self.tmux(
            direct,
            "set-option",
            "-p",
            "-t",
            stale,
            "window-active-style",
            "bg=#222222",
        )
        self.tmux(
            direct,
            "set-option",
            "-p",
            "-t",
            stale,
            "@termnav_child_focus_restore_active_style",
            '{"had_override":false,"value":""}',
        )
        self.tmux(
            direct,
            "set-option",
            "-p",
            "-t",
            stale,
            "@termnav_client_unfocused",
            "1",
        )

        self.tmux(direct, "select-pane", "-t", stale)
        wait_until(
            lambda: (
                self.pane_blurred(direct, stale) == ""
                and self.pane_active_style(direct, stale) == ""
            ),
            "selection hook repairing a stale dim override",
        )

    def test_nested_client_process_exposes_its_exact_parent_route(self) -> None:
        inner = self.new_server("route-inner", "sleep 30")
        outer = self.new_server(
            "route-outer",
            f"env TERM=tmux-256color tmux -S {shlex.quote(str(inner))} attach-session -t focus",
        )
        parent_pane = self.tmux(outer, "display-message", "-p", "#{pane_id}")
        client = PtyClient(outer, self.environment)
        self.clients.append(client)
        client_pid, _client_tty = self.client_identity(inner)
        parent = self.module.parent_for_client(client_pid, str(inner))
        process_scope = {
            name: self.module.process_env(client_pid, name)
            for name in ("TMUX", "TMUX_PANE", "TERMNAV_PARENT_RELAY")
        }
        self.assertEqual(
            self.module.Parent(tmux_socket=str(outer), pane=parent_pane),
            parent,
            f"client_pid={client_pid} process_scope={process_scope!r}",
        )

    def test_three_levels_keep_exactly_one_highlighted_leaf(self) -> None:
        deepest = self.new_server("deepest", "sleep 30")
        deepest_other = self.tmux(
            deepest,
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "sleep 30",
        )
        middle = self.new_server(
            "middle",
            f"env TERM=tmux-256color tmux -S {shlex.quote(str(deepest))} attach-session -t focus",
        )
        middle_nested = self.tmux(middle, "display-message", "-p", "#{pane_id}")
        middle_leaf = self.tmux(
            middle,
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "sleep 30",
        )
        outer = self.new_server(
            "outer-three",
            f"env TERM=tmux-256color tmux -S {shlex.quote(str(middle))} attach-session -t focus",
        )
        outer_nested = self.tmux(outer, "display-message", "-p", "#{pane_id}")
        outer_leaf = self.tmux(
            outer,
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "sleep 30",
        )
        client = PtyClient(outer, self.environment)
        self.clients.append(client)
        client.focus()

        wait_until(
            lambda: (
                bool(self.pane_claim(outer, outer_nested))
                and bool(self.pane_claim(middle, middle_nested))
            ),
            "claims through three focused tmux levels",
        )
        self.assertEqual("bg=#222222", self.pane_active_style(outer, outer_nested))
        self.assertEqual("bg=#222222", self.pane_active_style(middle, middle_nested))
        self.assertEqual(
            1,
            self.visual_leaf_count(outer, middle, deepest),
            self.focus_snapshot(outer, middle, deepest),
        )

        # Selecting another pane at the deepest level changes the leaf without
        # changing either ancestor claim.
        self.tmux(deepest, "select-pane", "-t", deepest_other)
        self.assertEqual(1, self.visual_leaf_count(outer, middle, deepest))

        # Selecting a normal pane in the middle releases only its child claim;
        # the middle server remains the focused child claimed by the outer one.
        self.tmux(middle, "select-pane", "-t", middle_leaf)
        wait_until(
            lambda: self.pane_claim(middle, middle_nested) == "",
            "deep child release while its parent remains focused",
        )
        wait_until(
            lambda: self.pane_active_style(deepest, deepest_other) == "bg=#222222",
            "deepest leaf dimming when its client loses focus",
        )
        self.assertEqual("", self.pane_active_style(middle, middle_nested))
        self.assertEqual("bg=#222222", self.pane_active_style(outer, outer_nested))
        self.assertTrue(self.pane_claim(outer, outer_nested))
        self.assertEqual(1, self.visual_leaf_count(outer, middle, deepest))

        # Moving to a normal outer pane makes the outer server itself the leaf
        # and leaves every now-unfocused nested pane unhighlighted.
        self.tmux(outer, "select-pane", "-t", outer_leaf)
        wait_until(
            lambda: self.pane_claim(outer, outer_nested) == "",
            "middle child release after leaving the nested outer pane",
        )
        wait_until(
            lambda: self.pane_active_style(middle, middle_leaf) == "bg=#222222",
            "middle leaf dimming when its client loses focus",
        )
        self.assertEqual("", self.pane_active_style(outer, outer_nested))
        self.assertEqual(1, self.visual_leaf_count(outer, middle, deepest))

        client.blur()
        wait_until(
            lambda: self.visual_leaf_count(outer, middle, deepest) == 0,
            "all leaves dim after the top-level terminal loses focus",
        )

    def test_shared_inner_session_keeps_outer_attachments_independent(self) -> None:
        inner = self.new_server("shared-inner", "sleep 30")
        self.tmux(inner, "split-window", "-d", "sleep 30")
        attach = f"env TERM=tmux-256color tmux -S {shlex.quote(str(inner))} attach-session -t focus"
        outer_one = self.new_server("outer-one", attach)
        pane_one = self.tmux(outer_one, "display-message", "-p", "#{pane_id}")
        outer_two = self.new_server("outer-two", attach)
        pane_two = self.tmux(outer_two, "display-message", "-p", "#{pane_id}")
        client_one = PtyClient(outer_one, self.environment)
        client_two = PtyClient(outer_two, self.environment)
        self.clients.extend((client_one, client_two))

        client_one.focus()
        client_two.focus()
        wait_until(
            lambda: (
                bool(self.pane_claim(outer_one, pane_one))
                and bool(self.pane_claim(outer_two, pane_two))
            ),
            "independent claims from two clients of one inner session",
        )
        self.assertEqual("bg=#222222", self.pane_active_style(outer_one, pane_one))
        self.assertEqual("bg=#222222", self.pane_active_style(outer_two, pane_two))
        inner_flags = self.tmux(inner, "list-clients", "-F", "#{client_flags}")
        focused_clients = sum("focused" in flags.split(",") for flags in inner_flags.splitlines())
        self.assertEqual(2, focused_clients)

        client_one.blur()
        wait_until(
            lambda: (
                self.pane_claim(outer_one, pane_one) == ""
                and bool(self.pane_claim(outer_two, pane_two))
                and self.pane_active_style(outer_one, pane_one) == "bg=#222222"
            ),
            "one attachment becoming inactive without disturbing the other",
        )
        inner_pane = self.tmux(inner, "display-message", "-p", "#{pane_id}")
        self.assertEqual("", self.pane_blurred(inner, inner_pane))
        self.assertEqual("", self.pane_active_style(inner, inner_pane))
        self.assertEqual("bg=#222222", self.pane_active_style(outer_two, pane_two))
        inner_flags = self.tmux(inner, "list-clients", "-F", "#{client_flags}")
        focused_clients = sum("focused" in flags.split(",") for flags in inner_flags.splitlines())
        self.assertEqual(1, focused_clients)

        client_one.focus()
        wait_until(
            lambda: (
                bool(self.pane_claim(outer_one, pane_one))
                and bool(self.pane_claim(outer_two, pane_two))
            ),
            "released attachment reclaiming independently",
        )

    def test_publisher_crash_expires_then_next_focus_recovers(self) -> None:
        inner = self.new_server("crash-inner", "sleep 30")
        outer = self.new_server(
            "crash-outer",
            f"env TERM=tmux-256color tmux -S {shlex.quote(str(inner))} attach-session -t focus",
        )
        nested_pane = self.tmux(outer, "display-message", "-p", "#{pane_id}")
        client = PtyClient(outer, self.environment)
        self.clients.append(client)
        client.focus()
        wait_until(
            lambda: bool(self.pane_claim(outer, nested_pane)),
            "claim before publisher crash",
        )

        wait_until(
            lambda: len(self.helper_lock_holders("watch", inner)) == 1,
            "one focused publisher process",
        )
        publishers = self.helper_lock_holders("watch", inner)
        os.kill(publishers[0], signal.SIGKILL)
        wait_until(
            lambda: (
                self.pane_claim(outer, nested_pane) == ""
                and self.pane_active_style(outer, nested_pane) == ""
            ),
            "lease and pane-style cleanup after a killed publisher",
            timeout=2,
        )

        client.blur()
        client.focus()
        wait_until(
            lambda: bool(self.pane_claim(outer, nested_pane)),
            "fresh focus event restarts a publisher",
        )
        self.assertEqual("bg=#222222", self.pane_active_style(outer, nested_pane))

    def test_delayed_focus_out_cannot_stop_a_refocused_client(self) -> None:
        inner = self.new_server("bounce-inner", "sleep 30")
        outer = self.new_server(
            "bounce-outer",
            f"env TERM=tmux-256color tmux -S {shlex.quote(str(inner))} attach-session -t focus",
        )
        nested_pane = self.tmux(outer, "display-message", "-p", "#{pane_id}")
        client = PtyClient(outer, self.environment)
        self.clients.append(client)
        client.focus()
        wait_until(
            lambda: bool(self.pane_claim(outer, nested_pane)),
            "claim before stale stop simulation",
        )
        wait_until(
            lambda: len(self.helper_lock_holders("watch", inner)) == 1,
            "publisher before stale stop simulation",
        )
        publisher = self.helper_lock_holders("watch", inner)[0]
        client_pid, client_tty = self.client_identity(inner)

        # A focus-out hook can be queued just before a newer focus-in hook. The
        # stop command must re-read current client state and leave the current
        # publisher alone when that delayed hook finally executes.
        subprocess.run(
            [
                str(self.focus),
                "stop",
                "--tmux-socket",
                str(inner),
                "--client-pid",
                str(client_pid),
                "--client-tty",
                client_tty,
            ],
            env=self.environment,
            check=True,
        )
        self.assertTrue(self.pane_claim(outer, nested_pane))
        self.assertEqual([publisher], self.helper_lock_holders("watch", inner))

        # Also exercise the narrower race where the old stop already resolved
        # the watcher PID before focus returned and its signal arrives late.
        previous_claim = self.pane_claim(outer, nested_pane)
        os.kill(publisher, signal.SIGTERM)
        wait_until(
            lambda: (
                self.pane_claim(outer, nested_pane) != previous_claim
                and bool(self.pane_claim(outer, nested_pane))
            ),
            "renewal after a stale stop signal",
        )
        self.assertEqual([publisher], self.helper_lock_holders("watch", inner))

    def test_renewals_keep_one_expirer_and_direct_clients_keep_no_watcher(self) -> None:
        inner = self.new_server("efficient-inner", "sleep 30")
        outer = self.new_server(
            "efficient-outer",
            f"env TERM=tmux-256color tmux -S {shlex.quote(str(inner))} attach-session -t focus",
        )
        nested_pane = self.tmux(outer, "display-message", "-p", "#{pane_id}")
        client = PtyClient(outer, self.environment)
        self.clients.append(client)
        client.focus()
        wait_until(
            lambda: bool(self.pane_claim(outer, nested_pane)),
            "claim before process-count check",
        )
        # Allow several 150 ms renewals. They update one lease in place rather
        # than creating a timer process for every heartbeat.
        time.sleep(0.55)
        self.assertEqual(
            1,
            len(self.helper_lock_holders("watch", inner)),
            self.helper_process_details("watch", inner),
        )
        self.assertEqual(
            1,
            len(self.helper_lock_holders("expire", outer)),
            self.helper_process_details("expire", outer),
        )
        self.assertEqual(0, len(self.helper_processes("watch", outer)))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--focus", required=True, type=pathlib.Path)
    parser.add_argument("--relay", required=True, type=pathlib.Path)
    arguments, unittest_arguments = parser.parse_known_args()
    library = arguments.focus.parent.parent / "lib" / "termnav"
    sys.path.insert(0, str(library))
    import process_info
    import tmux_focus

    FocusClaimTest.focus = arguments.focus
    FocusLibraryTest.module = tmux_focus
    FocusLibraryTest.process_info = process_info
    RelayFocusTest.relay = arguments.relay
    NestedFocusTest.focus = arguments.focus
    NestedFocusTest.module = tmux_focus
    os.environ.setdefault("XDG_RUNTIME_DIR", tempfile.gettempdir())
    program = unittest.main(argv=[__file__, *unittest_arguments], verbosity=2, exit=False)
    return 0 if program.result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
