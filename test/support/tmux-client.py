"""Attach a real tmux client with controlled parent-scope ancestry."""

from __future__ import annotations

import os
import pty
import select
import signal
import subprocess
import sys
import time


def run_client(arguments: list[str]) -> int:
    """Run tmux under a PTY with TMUX hidden only from the child client."""

    child, descriptor = pty.fork()
    if child == 0:
        os.environ.pop("TMUX", None)
        os.environ.pop("TMUX_PANE", None)
        os.environ.setdefault("TERM", "xterm-256color")
        os.execvp("tmux", ["tmux", *arguments])

    def terminate(_signum: int, _frame: object) -> None:
        # forkpty makes the child a session and process-group leader. tmux
        # intentionally handles ordinary termination signals, so force this
        # isolated test client down; otherwise its wrapper can hold the test
        # runner open after every assertion has completed.
        try:
            os.killpg(child, signal.SIGKILL)
        except ProcessLookupError:
            pass

    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGINT, terminate)
    pending = b""
    transcript = b""
    try:
        while True:
            waited, status = os.waitpid(child, os.WNOHANG)
            if waited == child:
                code = os.waitstatus_to_exitcode(status)
                if code != 0 and transcript:
                    # tmux writes startup diagnostics to its controlling PTY,
                    # not the wrapper's stderr. Preserve the tail when a test
                    # client dies early so CI explains why it never attached.
                    os.write(2, transcript)
                return code

            ready, _, _ = select.select([descriptor], [], [], 0.05)
            if not ready:
                continue
            try:
                payload = os.read(descriptor, 65536)
            except OSError:
                # PTY teardown can race the nonblocking child-status check.
                time.sleep(0.01)
                continue
            if not payload:
                time.sleep(0.01)
                continue

            # Drain the synthetic terminal continuously so tmux can finish
            # redraws on platforms with small PTY buffers. Also answer tmux's
            # terminal-capability probe; leaving a test client half-initialized
            # makes its visible pane lag later session changes.
            transcript = (transcript + payload)[-4096:]
            pending = (pending + payload)[-64:]
            if b"\x1b[?996n" in pending:
                os.write(descriptor, b"\x1b[?997;1n")
                pending = b""
    finally:
        os.close(descriptor)


def process_running(pid: int) -> bool:
    """Return whether a detached fixture process can still execute."""

    # A minimal container's PID 1 may retain an orphaned zombie indefinitely.
    # `kill(pid, 0)` reports that already-dead process as present, which would
    # make the launcher wait forever during suite cleanup.
    try:
        with open(f"/proc/{pid}/stat", encoding="utf-8") as stream:
            stat = stream.read()
        state = stat[stat.rfind(")") + 2 :].split(maxsplit=1)[0]
        return state != "Z"
    except (FileNotFoundError, OSError, IndexError):
        pass

    # macOS has no procfs but provides the process state through BSD ps.
    try:
        result = subprocess.run(
            ["ps", "-o", "stat=", "-p", str(pid)],
            check=False,
            capture_output=True,
            text=True,
            timeout=1,
        )
        state = result.stdout.strip()
        return result.returncode == 0 and bool(state) and not state.startswith("Z")
    except (OSError, subprocess.TimeoutExpired):
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False


def run_without_parent_scope(arguments: list[str]) -> int:
    """Run a client whose process ancestry cannot inherit the test runner's tmux."""

    read_descriptor, write_descriptor = os.pipe()
    intermediate = os.fork()
    if intermediate == 0:
        os.close(read_descriptor)
        # Capture our identity before the second fork. If the short-lived
        # intermediate exits before the daemon first runs, asking getppid()
        # there would already return the platform reaper and the daemon would
        # wait forever for that stable PID to change instead of attaching.
        intermediate_pid = os.getpid()
        daemon = os.fork()
        if daemon != 0:
            os.write(write_descriptor, str(daemon).encode())
            os._exit(0)

        # Wait until the short-lived intermediate is gone before creating the
        # tmux client. This makes the daemon's parent the platform reaper rather
        # than the shell running the suite, whose own TMUX may describe an
        # unrelated developer session even after the variable is unset.
        while os.getppid() == intermediate_pid:
            time.sleep(0.001)
        os.close(write_descriptor)
        raise SystemExit(run_client(arguments))

    os.close(write_descriptor)
    payload = os.read(read_descriptor, 32)
    os.close(read_descriptor)
    os.waitpid(intermediate, 0)
    daemon = int(payload)

    def terminate(_signum: int, _frame: object) -> None:
        try:
            os.kill(daemon, signal.SIGTERM)
        except ProcessLookupError:
            pass

    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGINT, terminate)
    while process_running(daemon):
        time.sleep(0.05)
    return 0


def main() -> int:
    """Run one synthetic tmux client."""

    arguments = sys.argv[1:]
    orphan_parent = arguments[:1] == ["--without-parent-scope"]
    if orphan_parent:
        arguments = arguments[1:]
    if not arguments:
        print(
            "usage: tmux-client.py [--without-parent-scope] TMUX_ARGS...",
            file=sys.stderr,
        )
        return 2
    if orphan_parent:
        return run_without_parent_scope(arguments)
    return run_client(arguments)


if __name__ == "__main__":
    raise SystemExit(main())
