"""Attach a real tmux client while preserving parent-scope ancestry."""

from __future__ import annotations

import os
import pty
import select
import signal
import sys
import time


def main() -> int:
    """Run tmux under a PTY with TMUX hidden only from the child client."""

    if len(sys.argv) < 2:
        print("usage: tmux-client.py TMUX_ARGS...", file=sys.stderr)
        return 2

    child, descriptor = pty.fork()
    if child == 0:
        os.environ.pop("TMUX", None)
        os.environ.pop("TMUX_PANE", None)
        os.environ.setdefault("TERM", "xterm-256color")
        os.execvp("tmux", ["tmux", *sys.argv[1:]])

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
    try:
        while True:
            waited, status = os.waitpid(child, os.WNOHANG)
            if waited == child:
                return os.waitstatus_to_exitcode(status)

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
            pending = (pending + payload)[-64:]
            if b"\x1b[?996n" in pending:
                os.write(descriptor, b"\x1b[?997;1n")
                pending = b""
    finally:
        os.close(descriptor)


if __name__ == "__main__":
    raise SystemExit(main())
