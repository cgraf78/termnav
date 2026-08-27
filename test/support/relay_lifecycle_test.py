#!/usr/bin/env python3
"""Integration tests for per-SSH relay process ownership."""

from __future__ import annotations

import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
RELAY = ROOT / "bin" / "termnav-relay"
RELAY_SSH = ROOT / "test" / "support" / "relay-ssh.py"


def wait_for(predicate, description: str, timeout: float = 3.0) -> None:
    """Poll an observable process condition until its bounded deadline."""

    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for {description}")
        time.sleep(0.02)


def process_snapshot(pid: int) -> tuple[str, int, str] | None:
    """Return live process state, parent, and argv without Linux procps."""

    proc = pathlib.Path("/proc") / str(pid)
    if pathlib.Path("/proc").is_dir():
        try:
            stat_text = (proc / "stat").read_text(encoding="utf-8")
            _prefix, separator, suffix = stat_text.rpartition(") ")
            if not separator:
                return None
            fields = suffix.split(maxsplit=2)
            if len(fields) < 2:
                return None
            state = fields[0]
            parent = int(fields[1])
            arguments = (
                (proc / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
            )
            return state, parent, arguments
        except (OSError, ValueError):
            return None

    # macOS has no procfs but always provides the native BSD ps used here.
    result = subprocess.run(
        ["ps", "-o", "stat=,ppid=,args=", "-p", str(pid)],
        check=False,
        capture_output=True,
        text=True,
    )
    line = result.stdout.strip()
    if result.returncode != 0 or not line:
        return None
    fields = line.split(maxsplit=2)
    if len(fields) < 2:
        return None
    return fields[0], int(fields[1]), fields[2] if len(fields) == 3 else ""


def pid_alive(pid: int) -> bool:
    """Return whether a process is still executable rather than a zombie."""

    snapshot = process_snapshot(pid)
    return snapshot is not None and not snapshot[0].startswith("Z")


def process_matches(pid: int, marker: str) -> bool:
    """Return whether the same live process still owns its unique identity."""

    snapshot = process_snapshot(pid)
    return snapshot is not None and not snapshot[0].startswith("Z") and marker in snapshot[2]


def socket_accepts(path: pathlib.Path) -> bool:
    """Return whether a Unix listener accepts a real client connection."""

    import socket

    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(0.1)
            client.connect(str(path))
        return True
    except OSError:
        return False


def terminate(pid: int | None) -> None:
    """Best-effort cleanup for a test-owned process that may be orphaned."""

    if pid is None:
        return
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        wait_for(lambda: not pid_alive(pid), f"process {pid} to terminate", 1.0)
        return
    except AssertionError:
        pass
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def matching_children(parent: int, marker: str) -> set[int]:
    """Find exact direct children without relying on platform lsof behavior."""

    proc = pathlib.Path("/proc")
    if proc.is_dir():
        children_file = proc / str(parent) / "task" / str(parent) / "children"
        try:
            candidates = [int(value) for value in children_file.read_text().split()]
        except (FileNotFoundError, PermissionError, ValueError):
            candidates = [int(entry.name) for entry in proc.iterdir() if entry.name.isdigit()]
        return {
            pid
            for pid in candidates
            if (snapshot := process_snapshot(pid)) is not None
            and snapshot[1] == parent
            and marker in snapshot[2]
        }

    result = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,stat=,args="],
        check=True,
        capture_output=True,
        text=True,
    )
    children = set()
    for line in result.stdout.splitlines():
        fields = line.split(maxsplit=3)
        if len(fields) == 4 and int(fields[1]) == parent and marker in fields[3]:
            children.add(int(fields[0]))
    return children


def descriptor_target(pid: int, descriptor: int) -> str:
    """Return the portable lsof name for one process descriptor."""

    proc_descriptor = pathlib.Path("/proc") / str(pid) / "fd" / str(descriptor)
    if pathlib.Path("/proc").is_dir():
        return os.readlink(proc_descriptor)

    result = subprocess.run(
        ["lsof", "-a", "-p", str(pid), "-d", str(descriptor), "-Fn"],
        check=True,
        capture_output=True,
        text=True,
    )
    names = [line[1:] for line in result.stdout.splitlines() if line.startswith("n")]
    if len(names) != 1:
        raise AssertionError(
            f"expected one target for process {pid} fd {descriptor}, found {names}"
        )
    return names[0]


def wait_for_selector(pid: int) -> None:
    """Wait until the relay is actually sleeping in its selector."""

    wait_channel = pathlib.Path("/proc") / str(pid) / "wchan"
    if wait_channel.exists():
        wait_for(
            lambda: any(
                marker in wait_channel.read_text(encoding="utf-8")
                for marker in ("epoll", "ep_poll")
            ),
            f"relay {pid} to block in epoll",
        )
        return
    # macOS does not expose procfs wait channels. Readiness is established
    # first; this bounded settling interval then targets the kqueue-blocked
    # state whose signal wakeup contract the test intentionally exercises.
    time.sleep(0.1)


class RelayLifecycleTest(unittest.TestCase):
    """Exercise real process and socket cleanup rather than mocked calls."""

    def setUp(self) -> None:
        # macOS AF_UNIX paths are limited to 103 bytes, while its default
        # temporary root is already long. Prefer the portable short spelling
        # when available; native Termux falls back to its platform TMPDIR.
        short_root = "/tmp" if os.path.isdir("/tmp") and os.access("/tmp", os.W_OK) else None
        self.tempdir = tempfile.TemporaryDirectory(prefix="tnrl-", dir=short_root)
        self.root = pathlib.Path(self.tempdir.name)
        self.runtime = self.root / "runtime"
        self.fake_ssh_pid = self.root / "fake-ssh.pid"
        self.fake_ssh = self.root / "ssh"
        self.fake_ssh.write_text(
            """#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == -G ]]; then
  printf '%s\\n' 'requesttty auto' 'sessiontype default'
  exit 0
fi
printf '%s\\n' "$$" >"$TERMNAV_TEST_FAKE_SSH_PID"
exec sleep 300
""",
            encoding="utf-8",
        )
        self.fake_ssh.chmod(0o700)
        self.children: list[subprocess.Popen] = []
        self.descriptors: set[int] = set()
        self.processes: set[int] = set()

    def tearDown(self) -> None:
        for descriptor in self.descriptors:
            try:
                os.close(descriptor)
            except OSError:
                pass
        for process in self.children:
            if process.stdin is not None and not process.stdin.closed:
                process.stdin.close()
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=1)
        for pid in self.processes:
            terminate(pid)
        self.tempdir.cleanup()

    def start_owned_server(self, socket_path: pathlib.Path) -> tuple[subprocess.Popen, int]:
        """Start a real relay with an independently owned lifetime pipe."""

        owner_read, owner_write = os.pipe()
        self.descriptors.update((owner_read, owner_write))
        environment = os.environ.copy()
        # The devserver's `python3` may itself be a supervising launcher. Pin
        # the shell facade to this interpreter so Popen.pid is the process
        # whose signal and selector lifecycle the test intends to observe.
        environment["TERMNAV_PYTHON"] = sys.executable
        server = subprocess.Popen(
            [
                str(RELAY),
                "serve",
                "--socket",
                str(socket_path),
                "--owner-fd",
                str(owner_read),
            ],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            pass_fds=(owner_read,),
        )
        self.children.append(server)
        os.close(owner_read)
        self.descriptors.discard(owner_read)
        wait_for(lambda: socket_accepts(socket_path), f"relay {socket_path} to listen")
        return server, owner_write

    def test_owned_server_exits_when_its_owner_pipe_closes(self) -> None:
        """The private server contract must survive uncatchable owner death."""

        socket_path = self.root / "owned.sock"
        server, owner_write = self.start_owned_server(socket_path)

        os.close(owner_write)
        self.descriptors.discard(owner_write)

        wait_for(lambda: server.poll() is not None, "owned relay to exit on EOF")
        wait_for(lambda: not socket_path.exists(), "owned relay socket cleanup")
        self.assertEqual(0, server.returncode)

    def test_owned_servers_have_independent_lifetimes(self) -> None:
        """One connection ending must not disturb another connection relay."""

        first_socket = self.root / "first.sock"
        second_socket = self.root / "second.sock"
        first, first_owner = self.start_owned_server(first_socket)
        second, second_owner = self.start_owned_server(second_socket)

        os.close(first_owner)
        self.descriptors.discard(first_owner)

        wait_for(lambda: first.poll() is not None, "first owned relay to exit")
        wait_for(lambda: not first_socket.exists(), "first owned relay socket cleanup")
        self.assertIsNone(second.poll())
        self.assertTrue(socket_accepts(second_socket))

        os.close(second_owner)
        self.descriptors.discard(second_owner)
        wait_for(lambda: second.poll() is not None, "second owned relay to exit")
        wait_for(lambda: not second_socket.exists(), "second owned relay socket cleanup")

    def test_owned_server_sigterm_does_not_require_owner_eof(self) -> None:
        """Explicit shutdown must wake independently from the ownership pipe."""

        socket_path = self.root / "signalled.sock"
        server, owner_write = self.start_owned_server(socket_path)
        wait_for_selector(server.pid)

        server.terminate()

        wait_for(lambda: server.poll() is not None, "signalled owned relay to exit")
        wait_for(lambda: not socket_path.exists(), "signalled owned relay socket cleanup")
        os.fstat(owner_write)

    @unittest.skipUnless(hasattr(signal, "SIGKILL"), "requires POSIX process signals")
    def test_ssh_wrapper_sigkill_does_not_orphan_its_relay(self) -> None:
        """A killed wrapper must not leave its connection relay or socket."""

        environment = os.environ.copy()
        environment.update(
            {
                "PATH": os.environ.get("PATH", ""),
                "REPO_TEST": "1",
                "TERMNAV_SSH_BINARY": str(self.fake_ssh),
                "TERMNAV_PYTHON": sys.executable,
                "TERMNAV_TEST_FAKE_SSH_PID": str(self.fake_ssh_pid),
                "TERMNAV_TEST_STDIN_TTY": "1",
                "XDG_RUNTIME_DIR": str(self.runtime),
            }
        )
        wrapper = subprocess.Popen(
            [sys.executable, str(RELAY_SSH), "lifecycle.example"],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.children.append(wrapper)

        relay_dir = self.runtime / f"termnav-{os.getuid()}"

        def active_socket() -> pathlib.Path | None:
            sockets = list(relay_dir.glob("relay-*.sock"))
            return sockets[0] if len(sockets) == 1 and self.fake_ssh_pid.exists() else None

        wait_for(lambda: active_socket() is not None, "SSH relay and fake client startup")
        socket_path = active_socket()
        if socket_path is None:
            self.fail("SSH relay socket disappeared during startup")
        relay_pids = matching_children(wrapper.pid, str(socket_path))
        self.assertTrue(relay_pids, "wrapper must own its connection relay")
        fake_pid = int(self.fake_ssh_pid.read_text(encoding="utf-8"))
        self.processes.update((*relay_pids, fake_pid))
        self.assertEqual(
            {"/dev/null"},
            {descriptor_target(pid, 0) for pid in relay_pids},
            "relay helpers must retain the prior noninteractive stdin contract",
        )

        os.kill(wrapper.pid, signal.SIGKILL)
        wrapper.wait(timeout=2)

        wait_for(
            lambda: not any(process_matches(pid, str(socket_path)) for pid in relay_pids),
            "orphaned relay processes to exit",
        )
        wait_for(lambda: not socket_path.exists(), "orphaned relay socket cleanup")
        self.assertTrue(
            pid_alive(fake_pid),
            "fixture SSH must outlive the wrapper so relay ownership is tested directly",
        )
        self.processes.difference_update(relay_pids)


if __name__ == "__main__":
    unittest.main(verbosity=2)
