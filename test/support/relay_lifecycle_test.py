#!/usr/bin/env python3
"""Integration tests for per-SSH relay process ownership."""

from __future__ import annotations

import json
import os
import pathlib
import pty
import select
import signal
import socket
import subprocess
import tempfile
import threading
import time
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
RELAY = pathlib.Path(os.environ.get("TERMNAV_TEST_BINARY", ROOT / "target" / "release" / "termnav"))


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


def wait_for_selector(pid: int) -> None:
    """Wait until the relay is actually sleeping in its selector."""

    wait_channel = pathlib.Path("/proc") / str(pid) / "wchan"
    if wait_channel.exists():
        wait_for(
            lambda: any(
                marker in wait_channel.read_text(encoding="utf-8")
                for marker in ("poll", "epoll", "ep_poll")
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
        inherited_runtime = pathlib.Path(os.environ.get("XDG_RUNTIME_DIR", ""))
        # The shell harness owns a unique, mode-0700 XDG root. Reuse that exact
        # short path on Termux: adding this fixture's usual directory layers can
        # exceed sockaddr_un before the lifecycle behavior is exercised.
        self.runtime = (
            inherited_runtime
            if short_root is None
            and os.environ.get("REPO_TEST") == "1"
            and inherited_runtime.is_absolute()
            else self.root / "runtime"
        )
        # XDG_RUNTIME_DIR is an existing, session-owned directory by contract;
        # create the fixture root explicitly instead of relying on Termnav to
        # weaken that boundary with recursive parent creation.
        self.runtime.mkdir(mode=0o700, exist_ok=True)
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

    def start_owned_server(
        self,
        socket_path: pathlib.Path,
        extra_environment: dict[str, str] | None = None,
    ) -> tuple[subprocess.Popen, int]:
        """Start a real relay with an independently owned lifetime pipe."""

        owner_read, owner_write = os.pipe()
        self.descriptors.update((owner_read, owner_write))
        environment = os.environ.copy()
        # Owned-server lifecycle tests model a relay hop outside tmux unless a
        # case opts in explicitly. Never let the developer's live session turn
        # a protocol fixture into real pane discovery or routing.
        environment.pop("TMUX", None)
        environment.pop("TMUX_PANE", None)
        if extra_environment is not None:
            environment.update(extra_environment)
        server = subprocess.Popen(
            [
                str(RELAY),
                "relay",
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

    def test_owner_close_drains_one_active_request_and_cancels_the_queue(self) -> None:
        """Shutdown is bounded by one admitted transaction, not queue length."""

        parent_path = self.root / "blocked-parent.sock"
        parent = socket.socket(socket.AF_UNIX)
        parent.bind(str(parent_path))
        parent.listen(1)
        accepted = threading.Event()
        release = threading.Event()

        def serve_parent() -> None:
            connection, _ = parent.accept()
            with connection:
                connection.recv(513)
                accepted.set()
                # The main thread deliberately fills a 96-client backlog after
                # this request is admitted. Slow or contended CI must not make
                # the parent disappear before that setup reaches its assertion.
                if release.wait(timeout=10):
                    connection.sendall(b'{"v":2,"result":"emitted"}\n')

        parent_thread = threading.Thread(target=serve_parent)
        parent_thread.start()
        socket_path = self.root / "draining.sock"
        server, owner_write = self.start_owned_server(
            socket_path,
            {"TERMNAV_PARENT_RELAY": str(parent_path)},
        )

        clients = []
        for index in range(96):
            client = socket.socket(socket.AF_UNIX)
            client.settimeout(2)
            try:
                client.connect(str(socket_path))
                nonce = f"{index + 1:012x}"
                client.sendall(
                    json.dumps(
                        {"v": 2, "op": "commit-path", "nonce": nonce},
                        separators=(",", ":"),
                    ).encode()
                    + b"\n"
                )
                clients.append(client)
            except OSError:
                client.close()

        wait_for(accepted.is_set, "one admitted relay request")
        os.close(owner_write)
        self.descriptors.discard(owner_write)
        self.assertIsNone(server.poll(), "active transaction was abandoned")

        release.set()
        wait_for(
            lambda: server.poll() is not None,
            "drained relay server to exit",
            timeout=5,
        )
        server.wait()
        wait_for(lambda: not socket_path.exists(), "drained relay socket cleanup")
        self.assertEqual(0, server.returncode)
        for client in clients:
            client.close()
        parent_thread.join(timeout=2)
        parent.close()

    def test_owned_server_exits_when_its_owner_pipe_closes(self) -> None:
        """The private server contract must survive uncatchable owner death."""

        socket_path = self.root / "owned.sock"
        server, owner_write = self.start_owned_server(socket_path)

        os.close(owner_write)
        self.descriptors.discard(owner_write)

        wait_for(lambda: server.poll() is not None, "owned relay to exit on EOF")
        wait_for(lambda: not socket_path.exists(), "owned relay socket cleanup")
        self.assertEqual(0, server.returncode)

    def test_direct_ssh_relay_keeps_its_controlling_terminal_identity(self) -> None:
        """A non-tmux SSH hop must remain able to terminate navigation."""

        reply_path = self.root / "direct-reply.json"
        release_path = self.root / "direct-release"
        direct_bin = self.root / "direct-bin"
        direct_bin.mkdir()
        direct_ssh = direct_bin / "ssh"
        direct_ssh.write_text(
            """#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == -G ]]; then
  printf '%s\\n' 'requesttty auto' 'sessiontype default' \\
    'remotecommand none' 'exitonforwardfailure no'
  exit 0
fi
local_socket=
for argument in "$@"; do
  case "$argument" in
    RemoteForward=*) local_socket=${argument#RemoteForward=}; local_socket=${local_socket#*:} ;;
  esac
done
python3 - "$local_socket" "$TERMNAV_TEST_DIRECT_REPLY" <<'PY'
import json
import os
import socket
import sys
import time

def exchange(request):
    client = socket.socket(socket.AF_UNIX)
    client.connect(sys.argv[1])
    client.sendall(json.dumps(request, separators=(",", ":")).encode() + b'\\n')
    reply = b''
    while not reply.endswith(b'\\n'):
        reply += client.recv(513)
    client.close()
    return json.loads(reply)

nonce = "0123456789ab"
replies = [
    exchange({"v": 2, "op": "prepare-path", "nonce": nonce}),
    exchange({"v": 2, "op": "commit-path", "nonce": nonce}),
]
with open(sys.argv[2], 'wb') as output:
    output.write(json.dumps(replies).encode())
while not os.path.exists(os.environ["TERMNAV_TEST_DIRECT_RELEASE"]):
    time.sleep(0.01)
PY
""",
            encoding="utf-8",
        )
        direct_ssh.chmod(0o700)
        environment = os.environ.copy()
        environment.update(
            {
                "TERMNAV_TEST_DIRECT_REPLY": str(reply_path),
                "TERMNAV_TEST_DIRECT_RELEASE": str(release_path),
                "XDG_RUNTIME_DIR": str(self.runtime),
            }
        )
        environment["PATH"] = f"{direct_bin}:{environment.get('PATH', '')}"
        environment.pop("TMUX", None)
        environment.pop("TMUX_PANE", None)

        child, master = pty.fork()
        if child == 0:
            null = os.open(os.devnull, os.O_RDONLY)
            os.dup2(null, 0)
            os.close(null)
            os.execve(
                str(RELAY),
                [str(RELAY), "ssh", "-t", "direct.example"],
                environment,
            )
        reaped = False
        try:
            wait_for(reply_path.exists, "direct relay protocol replies")
            readable, _, _ = select.select([master], [], [], 2)
            self.assertTrue(readable, "direct relay emitted no terminal barrier")
            terminal_output = os.read(master, 4096)
            self.assertIn(b"\x1b[?2004$p", terminal_output)
            # Keep the slave side alive until the query is consumed. Android's
            # PTY reports EIO as soon as the final slave closes, even when a
            # readable notification raced with those last bytes.
            release_path.touch()
            status = None

            def exited() -> bool:
                # The child has already been consumed once waitpid succeeds.
                # Propagate that fact to cleanup so a recycled PID can never
                # receive the fallback termination signal.
                nonlocal status, reaped
                waited, candidate = os.waitpid(child, os.WNOHANG)
                if waited == child:
                    status = candidate
                    reaped = True
                    return True
                return False

            wait_for(exited, "direct SSH wrapper to exit")
            self.assertIsNotNone(status)
            self.assertEqual(0, os.waitstatus_to_exitcode(status))
            self.assertEqual(
                [
                    {"v": 2, "result": "prepared"},
                    {"v": 2, "result": "emitted"},
                ],
                json.loads(reply_path.read_text(encoding="utf-8")),
            )
        finally:
            os.close(master)
            if not reaped:
                terminate(child)
                try:
                    os.waitpid(child, 0)
                except ChildProcessError:
                    pass

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

    @unittest.skipUnless(hasattr(signal, "SIGTERM"), "requires POSIX process signals")
    def test_ssh_wrapper_signals_stop_child_and_relay(self) -> None:
        """Catchable termination must leave neither child nor listener behind."""

        for signum in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signal=signum):
                environment = os.environ.copy()
                environment.update(
                    {
                        "PATH": f"{self.root}:{environment.get('PATH', '')}",
                        "TERMNAV_TEST_FAKE_SSH_PID": str(self.fake_ssh_pid),
                        "XDG_RUNTIME_DIR": str(self.runtime),
                    }
                )
                wrapper = subprocess.Popen(
                    [str(RELAY), "ssh", "-t", "signal.example"],
                    env=environment,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                self.children.append(wrapper)
                relay_dir = self.runtime / f"termnav-{os.getuid()}"

                # The bounded poll belongs to this signal subtest even if the
                # next loop iteration starts before a late callback is retired.
                def active_socket(relay_dir: pathlib.Path = relay_dir) -> pathlib.Path | None:
                    sockets = list(relay_dir.glob("relay-*.sock"))
                    return sockets[0] if len(sockets) == 1 and self.fake_ssh_pid.exists() else None

                wait_for(lambda: active_socket() is not None, "signalled SSH relay startup")
                socket_path = active_socket()
                self.assertIsNotNone(socket_path)
                fake_pid = int(self.fake_ssh_pid.read_text(encoding="utf-8"))
                self.processes.add(fake_pid)

                os.kill(wrapper.pid, signum)
                wrapper.wait(timeout=3)
                wait_for(
                    lambda fake_pid=fake_pid: not pid_alive(fake_pid),
                    "signalled SSH child to exit",
                )
                self.processes.discard(fake_pid)
                wait_for(
                    lambda socket_path=socket_path: not socket_path.exists(),
                    "signalled relay socket cleanup",
                )
                self.fake_ssh_pid.unlink(missing_ok=True)

    @unittest.skipUnless(hasattr(signal, "SIGTERM"), "requires POSIX process signals")
    def test_ssh_wrapper_kills_a_child_that_ignores_the_forwarded_signal(self) -> None:
        """A hostile wrapper cannot keep the connection-owned relay alive."""

        self.fake_ssh.write_text(
            """#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == -G ]]; then
  printf '%s\\n' 'requesttty auto' 'sessiontype default'
  exit 0
fi
printf '%s\\n' "$$" >"$TERMNAV_TEST_FAKE_SSH_PID"
trap '' INT TERM
exec sleep 300
""",
            encoding="utf-8",
        )
        environment = os.environ.copy()
        environment.update(
            {
                "PATH": f"{self.root}:{environment.get('PATH', '')}",
                "TERMNAV_TEST_FAKE_SSH_PID": str(self.fake_ssh_pid),
                "XDG_RUNTIME_DIR": str(self.runtime),
            }
        )
        wrapper = subprocess.Popen(
            [str(RELAY), "ssh", "-t", "ignores-signal.example"],
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

        wait_for(active_socket, "ignoring SSH relay startup")
        socket_path = active_socket()
        self.assertIsNotNone(socket_path)
        fake_pid = int(self.fake_ssh_pid.read_text(encoding="utf-8"))
        self.processes.add(fake_pid)

        os.kill(wrapper.pid, signal.SIGTERM)
        wrapper.wait(timeout=3)
        wait_for(lambda: not pid_alive(fake_pid), "ignoring SSH child to be killed")
        self.processes.discard(fake_pid)
        wait_for(lambda: not socket_path.exists(), "ignoring SSH relay socket cleanup")

    def test_shared_controlmaster_sessions_cancel_only_their_own_forward(self) -> None:
        """Concurrent wrappers may share a mux but never a relay lifetime."""

        log = self.root / "shared-control.log"
        release_dir = self.root / "release"
        release_dir.mkdir()
        self.fake_ssh.write_text(
            """#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == -G ]]; then
  printf '%s\\n' 'requesttty auto' 'sessiontype default' \\
    'remotecommand none' 'exitonforwardfailure no' \\
    'controlpath /tmp/TermNav-Shared-Control'
  exit 0
fi
forward=
cancel=0
previous=
for argument in "$@"; do
  if [[ $previous == -R ]]; then forward=$argument; fi
  case "$argument" in
    RemoteForward=*) forward=${argument#RemoteForward=} ;;
    cancel) cancel=1 ;;
  esac
  previous=$argument
done
if [[ $cancel == 1 ]]; then
  printf 'CANCEL|%s\\n' "$forward" >>"$TERMNAV_TEST_SHARED_LOG"
  exit 0
fi
printf 'SESSION|%s|%s|%s\\n' "$PPID" "$$" "$forward" >>"$TERMNAV_TEST_SHARED_LOG"
while [[ ! -e "$TERMNAV_TEST_RELEASE_DIR/$$" ]]; do sleep 0.01; done
""",
            encoding="utf-8",
        )
        self.fake_ssh.chmod(0o700)
        environment = os.environ.copy()
        environment.update(
            {
                "PATH": f"{self.root}:{environment.get('PATH', '')}",
                "TERMNAV_TEST_SHARED_LOG": str(log),
                "TERMNAV_TEST_RELEASE_DIR": str(release_dir),
                "XDG_RUNTIME_DIR": str(self.runtime),
            }
        )
        wrappers = [
            subprocess.Popen(
                [str(RELAY), "ssh", "-t", f"shared-{index}.example"],
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            for index in range(2)
        ]
        self.children.extend(wrappers)

        def sessions() -> list[list[str]]:
            if not log.exists():
                return []
            return [
                line.split("|", 3)
                for line in log.read_text(encoding="utf-8").splitlines()
                if line.startswith("SESSION|")
            ]

        wait_for(lambda: len(sessions()) == 2, "two shared-ControlMaster sessions")
        rows = {int(fields[1]): fields for fields in sessions()}
        first, second = wrappers
        first_row = rows[first.pid]
        second_row = rows[second.pid]
        self.assertNotEqual(first_row[3], second_row[3])

        (release_dir / first_row[2]).touch()
        first.wait(timeout=3)
        self.assertIsNone(second.poll(), "first cleanup terminated the sibling session")
        wait_for(
            lambda: log.exists() and f"CANCEL|{first_row[3]}" in log.read_text(),
            "first exact forward cancellation",
        )
        self.assertNotIn(f"CANCEL|{second_row[3]}", log.read_text(encoding="utf-8"))

        (release_dir / second_row[2]).touch()
        second.wait(timeout=3)
        wait_for(
            lambda: f"CANCEL|{second_row[3]}" in log.read_text(encoding="utf-8"),
            "second exact forward cancellation",
        )

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
                "REPO_TEST": "1",
                "PATH": f"{self.root}:{environment.get('PATH', '')}",
                "TERMNAV_TEST_FAKE_SSH_PID": str(self.fake_ssh_pid),
                "XDG_RUNTIME_DIR": str(self.runtime),
            }
        )
        wrapper = subprocess.Popen(
            [str(RELAY), "ssh", "-t", "lifecycle.example"],
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
        fake_pid = int(self.fake_ssh_pid.read_text(encoding="utf-8"))
        self.processes.add(fake_pid)
        self.assertEqual(
            set(),
            matching_children(wrapper.pid, str(socket_path)),
            "the connection relay must be a thread, not a helper process",
        )

        os.kill(wrapper.pid, signal.SIGKILL)
        wrapper.wait(timeout=2)

        wait_for(
            lambda: not socket_accepts(socket_path),
            "killed wrapper's relay listener to close",
        )
        self.assertTrue(
            pid_alive(fake_pid),
            "fixture SSH must outlive the wrapper so relay ownership is tested directly",
        )

        # SIGKILL cannot run userspace unlink cleanup. The next invocation's
        # bounded stale sweep removes the harmless pathname without needing a
        # detached janitor process.
        os.utime(socket_path, (1, 1))
        subprocess.run(
            [str(RELAY), "relay", "sweep"],
            env=environment,
            check=True,
        )
        self.assertFalse(socket_path.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
