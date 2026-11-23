"""Exercise the Bash fixture timeout supervisor without signalling live processes."""

import itertools
import os
import signal
import subprocess
import sys
from unittest.mock import Mock, patch


def main() -> None:
    """Run the real supervisor body with an owned child and group in known states."""
    scenario = sys.argv[2]
    darwin = scenario in ("zombie-only", "live-denial")
    child = Mock(pid=42, returncode=None)
    child.poll.side_effect = AssertionError("group leader was reaped before final signal")
    clock = itertools.count(step=0.5)
    stopping = False

    def signal_group(group: int, signum: int) -> None:
        nonlocal stopping
        assert group == child.pid
        assert scenario != "completed-wait", "signalled a group after its leader was reaped"
        if signum != 0:
            stopping = True
        if signum == signal.SIGKILL:
            print("attempted final signal to owned group")
        if darwin:
            raise PermissionError("group has no authority for requested signal")

    def snapshot(args, **_kwargs):
        assert args == ["/bin/ps", "-axo", "pgid=,stat="]
        state = "Z" if scenario == "zombie-only" else "R"
        return subprocess.CompletedProcess(args, 0, f"{child.pid} {state}\n")

    def completed_wait(timeout: float) -> int:
        # Reproduce a cancellation in the interval between waitpid reaping
        # the command and Popen publishing its return code.
        os.kill(os.getpid(), signal.SIGTERM)
        child.returncode = 7
        return child.returncode

    def timed_wait(timeout: float) -> int:
        if stopping:
            return 0
        raise subprocess.TimeoutExpired("fixture", timeout)

    child.wait.side_effect = completed_wait if scenario == "completed-wait" else timed_wait

    code = compile(sys.stdin.read(), "<fixture timeout supervisor>", "exec")
    with (
        patch.object(sys, "platform", "darwin" if darwin else "linux"),
        patch("subprocess.Popen", return_value=child),
        patch("subprocess.run", side_effect=snapshot),
        patch("os.killpg", side_effect=signal_group),
        patch("time.monotonic", side_effect=lambda: next(clock)),
        patch("time.sleep"),
    ):
        # The Bash helper above supplies its committed supervisor source.
        exec(code, {"__name__": "__main__"})  # noqa: S102


if __name__ == "__main__":
    main()
