"""Exercise real nested tmux clients without touching the user's server."""

from __future__ import annotations

import json
import os
import pathlib
import shlex
import socket
import subprocess
import sys
import tempfile
import time


def run(*args: str, **kwargs) -> subprocess.CompletedProcess[str]:
    """Keep every diagnostic command bounded, including failed setup."""
    return subprocess.run(args, capture_output=True, text=True, timeout=8, check=False, **kwargs)


def diagnose_client(pid: str, inner: str, outer: str, env: dict[str, str]) -> None:
    """Report only fixture identity fields when Linux and macOS admission differ."""
    diagnostic: dict[str, object] = {"platform": sys.platform, "client_pid": pid}
    probes = {
        "clients": [
            "tmux",
            "-S",
            inner,
            "list-clients",
            "-F",
            "#{client_pid}|#{client_created}|#{client_control_mode}|#{client_tty}|#{client_name}",
        ],
        "process": ["ps", "-p", pid, "-o", "pid=", "-o", "pgid=", "-o", "tpgid=", "-o", "tty="],
        "parent": [
            "tmux",
            "-S",
            outer,
            "display-message",
            "-p",
            "-t",
            "%0",
            "#{pane_id}|#{pane_tty}|#{pane_pid}",
        ],
    }
    for name, arguments in probes.items():
        try:
            result = subprocess.run(
                arguments, env=env, capture_output=True, text=True, timeout=2, check=False
            )
            diagnostic[name] = {
                "status": result.returncode,
                "out": result.stdout[:4096],
                "err": result.stderr[:4096],
            }
        except (OSError, subprocess.TimeoutExpired) as error:
            diagnostic[name] = {"error": type(error).__name__}
    proc = pathlib.Path("/proc") / pid
    try:
        diagnostic["fd0"] = os.readlink(proc / "fd/0")
    except OSError as error:
        diagnostic["fd0"] = {"error": type(error).__name__}
    try:
        fields = (proc / "stat").read_text().rsplit(")", 1)[1].split()
        diagnostic["proc"] = {
            "state": fields[0],
            "pgrp": fields[2],
            "tty_nr": fields[4],
            "tpgid": fields[5],
        }
    except (OSError, IndexError, ValueError) as error:
        diagnostic["proc"] = {"error": type(error).__name__}
    try:
        # Only the two routing fields leave this helper. Never print an entire
        # environment: a hosted runner can inherit unrelated credentials.
        with (proc / "environ").open("rb") as stream:
            entries = stream.read(1024 * 1024).split(b"\0")
        diagnostic["inherited"] = {
            key.decode(): value.decode(errors="replace")
            for entry in entries
            if b"=" in entry
            for key, value in [entry.split(b"=", 1)]
            if key in (b"TMUX", b"TMUX_PANE")
        }
    except OSError as error:
        diagnostic["inherited"] = {"error": type(error).__name__}
    print(
        "attention admission diagnostic: " + json.dumps(diagnostic, sort_keys=True), file=sys.stderr
    )


def main() -> None:
    """Own two private servers and test identity, delivery, and refusal."""
    with tempfile.TemporaryDirectory(prefix="tn-attn-") as directory:
        root = pathlib.Path(directory)
        outer, inner = str(root / "o"), str(root / "i")
        env = os.environ.copy()
        for key in ("TMUX", "TMUX_PANE", "TERMNAV_PARENT_RELAY", "BASH_ENV", "ENV"):
            env.pop(key, None)
        env.update(
            HOME=directory,
            XDG_CONFIG_HOME=directory,
            XDG_STATE_HOME=directory,
            XDG_CACHE_HOME=directory,
            XDG_DATA_HOME=directory,
            TERM="xterm-256color",
        )

        def tmux(server: str, *args: str) -> subprocess.CompletedProcess[str]:
            return run("tmux", "-S", server, *args, env=env)

        def lease(pane: str) -> tuple[str, str]:
            """Read the legacy lease and its separate waiting lease."""
            return tuple(
                tmux(outer, "show-option", "-pqv", "-t", pane, option).stdout.strip()
                for option in ("@termnav_child_attention", "@termnav_child_waiting")
            )

        def assert_lease(pane: str, counts: list[str], waiting: str | None) -> tuple[str, str]:
            """Keep the legacy four-field shape and share one expiry."""
            state, extra = lease(pane)
            fields = state.split(":")
            assert len(fields) == 4 and fields[0].isdigit(), state
            assert fields[1:] == counts, state
            if waiting is None:
                assert extra == "", extra
            else:
                assert extra == fields[0] + ":" + waiting, (state, extra)
            return state, extra

        try:
            # Cleanup is installed before either server starts. The nested
            # client obtains the outer pane as its real controlling terminal.
            script = root / "nested.sh"
            script.write_text(
                "#!/bin/sh\nexec tmux -S '" + inner + "' -f /dev/null new-session -s inner\n"
            )
            script.chmod(0o700)
            started = tmux(
                outer, "-f", "/dev/null", "new-session", "-d", "-s", "outer", str(script)
            )
            assert started.returncode == 0, started.stderr
            deadline = time.monotonic() + 8
            listing = ""
            while time.monotonic() < deadline:
                listing = tmux(
                    inner, "list-clients", "-F", "#{client_name}|#{client_pid}|#{client_created}"
                ).stdout.strip()
                if listing:
                    break
                time.sleep(0.03)
            assert listing, "nested client did not attach before deadline"
            name, pid, created = listing.split("|")
            # Parent configs may forward a changed lease from after-set-option,
            # which fires only for set-option commands a client sends. Record
            # each one's arguments; a hook's own set-option never refires.
            hooked = tmux(
                outer,
                "set-hook",
                "-g",
                "after-set-option",
                "set-option -gaF @lease_writes '#{hook_arguments}|'",
            )
            assert hooked.returncode == 0, hooked.stderr
            args = [
                sys.argv[1],
                "tmux",
                "attention",
                "--tmux-socket",
                inner,
                "--client-name",
                name,
                "--client-pid",
                pid,
                "--client-created",
                created,
                "--working",
                "2",
                "--pending",
                "3",
                "--failed",
                "1",
                "--waiting",
                "2",
            ]
            sent = run(*args, env=env)
            if sent.returncode != 0:
                diagnose_client(pid, inner, outer, env)
            assert sent.returncode == 0, (sent.returncode, sent.stderr)
            leases = assert_lease("%0", ["2", "3", "1"], "2")
            writes = tmux(outer, "show-option", "-gqv", "@lease_writes").stdout.strip()
            # tmux before 3.2 has no hook_arguments and records only separators.
            if writes.strip("|"):
                options = [
                    word
                    for entry in writes.split("|")
                    for word in entry.split()
                    if word.startswith("@termnav_child_")
                ]
                assert options == ["@termnav_child_attention", "@termnav_child_waiting"], writes
            # A changed client generation must not overwrite either lease.
            args[args.index("--client-created") + 1] = str(int(created) + 1)
            refused = run(*args, env=env)
            assert refused.returncode == 1, refused.stderr
            assert lease("%0") == leases
            # An older publisher omits --waiting; that clears the waiting lease.
            args[args.index("--client-created") + 1] = created
            older = args[: args.index("--waiting")]
            older[older.index("--pending") + 1] = "1"
            sent = run(*older, env=env)
            assert sent.returncode == 0, (sent.returncode, sent.stderr)
            assert_lease("%0", ["2", "1", "1"], None)
            # Exercise the real SSH relay dispatch without opening a second
            # authenticated network connection. Its socket remains owner-only.
            relay_path = str(root / "relay")
            launched = tmux(
                outer,
                "new-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                shlex.join([sys.argv[1], "relay", "serve", "--socket", relay_path]),
            )
            assert launched.returncode == 0, launched.stderr
            relay_pane = launched.stdout.strip()
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline and not pathlib.Path(relay_path).exists():
                time.sleep(0.03)

            def relay(request: dict[str, object]) -> None:
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(5)
                    client.connect(relay_path)
                    client.sendall(json.dumps(request).encode() + b"\n")
                    response = json.loads(client.makefile("rb").readline())
                assert response["result"] == "emitted", (request, response)

            request = {"v": 2, "op": "attention", "working": 3, "pending": 2, "failed": 1}
            relay(dict(request, waiting=1))
            assert_lease(relay_pane, ["3", "2", "1"], "1")
            # Older relay peers send no waiting field: accepted as zero.
            relay(request)
            assert_lease(relay_pane, ["3", "2", "1"], None)
            # Invalid inherited context must not silently pick another route.
            bad_path = str(root / "bad-relay")
            bad_env = dict(env, TMUX="malformed", TMUX_PANE="%0", TERMNAV_PARENT_RELAY=relay_path)
            bad = subprocess.Popen(
                [sys.argv[1], "relay", "serve", "--socket", bad_path],
                env=bad_env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            try:
                deadline = time.monotonic() + 8
                while time.monotonic() < deadline and not pathlib.Path(bad_path).exists():
                    time.sleep(0.03)
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(5)
                    client.connect(bad_path)
                    client.sendall(json.dumps(request).encode() + b"\n")
                    response = json.loads(client.makefile("rb").readline())
                assert response["result"] == "error", response
            finally:
                bad.terminate()
                try:
                    bad.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    bad.kill()
                    bad.wait(timeout=5)
        finally:
            tmux(inner, "kill-server")
            tmux(outer, "kill-server")


if __name__ == "__main__":
    main()
