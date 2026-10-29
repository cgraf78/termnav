"""Observe native attention output and backpressure on invocation-owned PTYs."""

import base64
import fcntl
import json
import os
import pathlib
import select
import subprocess
import sys
import tempfile
import termios
import time


def scenario(term: str, malformed_parent: bool = False) -> None:
    """Keep the client foreground while the native owner writes its real tty."""
    with tempfile.TemporaryDirectory(prefix="tn-direct-") as directory:
        root = pathlib.Path(directory)
        sock = str(root / "t")
        env = os.environ.copy()
        for key in list(env):
            if key.startswith(("TMUX", "SSH_", "TERMNAV_", "WEZTERM_", "NVIM")) or key in (
                "BASH_ENV",
                "ENV",
            ):
                env.pop(key, None)
        env.update(
            HOME=directory,
            XDG_CONFIG_HOME=directory,
            XDG_STATE_HOME=directory,
            XDG_DATA_HOME=directory,
            XDG_CACHE_HOME=directory,
            TERM=term,
        )
        if malformed_parent:
            env["TERMNAV_PARENT_RELAY"] = "relative-not-a-route"
        master, slave = os.openpty()
        os.set_blocking(master, False)
        tty = os.ttyname(slave)
        client = None

        def tmux(*args: str) -> subprocess.CompletedProcess[bytes]:
            return subprocess.run(
                ["tmux", "-S", sock, *args], env=env, capture_output=True, timeout=3, check=False
            )

        def drain() -> bytes:
            data = bytearray()
            while True:
                try:
                    chunk = os.read(master, 65536)
                except (BlockingIOError, OSError):
                    break
                if not chunk:
                    break
                data.extend(chunk)
            return bytes(data)

        def controlling() -> None:
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            os.tcsetpgrp(0, os.getpgrp())

        try:
            config = root / "tmux.conf"
            config.write_text("set -g status off\n")
            client = subprocess.Popen(
                [
                    "tmux",
                    "-S",
                    sock,
                    "-f",
                    str(config),
                    "new-session",
                    "-s",
                    "direct",
                    "exec sleep 60",
                ],
                env=env,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                preexec_fn=controlling,  # noqa: PLW1509 - single-threaded controlling-tty fixture
            )
            deadline = time.monotonic() + 5
            listing = b""
            while time.monotonic() < deadline:
                drain()
                listing = tmux(
                    "list-clients", "-F", "#{client_name}|#{client_pid}|#{client_created}"
                ).stdout.strip()
                if listing:
                    break
                time.sleep(0.01)
            assert listing, "private terminal client did not attach"
            name, pid, created = listing.decode().split("|")
            args = [
                sys.argv[1],
                "tmux",
                "attention",
                "--tmux-socket",
                sock,
                "--client-name",
                name,
                "--client-pid",
                pid,
                "--client-created",
                created,
                "--working",
                "2",
                "--pending",
                "1",
                "--failed",
                "1",
            ]
            drain()
            sent = subprocess.run(args, env=env, capture_output=True, timeout=3, check=False)
            if malformed_parent:
                assert sent.returncode == 1, (
                    "malformed inherited parent must not become a direct tty route"
                )
                assert not drain(), "malformed parent emitted terminal output"
                print("PASS malformed inherited parent fails closed")
                return
            assert sent.returncode == 0, (term, sent.returncode, sent.stderr)
            output = bytearray()
            deadline = time.monotonic() + 1
            while time.monotonic() < deadline:
                output.extend(drain())
                if b"\x07" in output:
                    break
                select.select([master], [], [], 0.01)
            prefix = b"\x18\x1b]1337;SetUserVar=TERM_PANE_STATE="
            suffix = b"\x07"
            if term.startswith("tmux"):
                prefix = b"\x18\x1bPtmux;\x18\x1b\x1b]1337;SetUserVar=TERM_PANE_STATE="
                suffix = b"\x07\x1b\\"
            start = output.find(prefix)
            assert start >= 0, (term, output)
            end = output.index(suffix, start)
            payload = json.loads(base64.b64decode(output[start + len(prefix) : end]))
            assert {key: payload[key] for key in ("v", "working", "pending", "failed")} == {
                "v": 1,
                "working": 2,
                "pending": 1,
                "failed": 1,
            }
            assert isinstance(payload["heartbeat"], int)
            args[args.index("--client-created") + 1] = str(int(created) + 1)
            stale = subprocess.run(args, env=env, capture_output=True, timeout=3, check=False)
            assert stale.returncode == 1
            assert not drain(), "stale client emitted terminal output"
            args[args.index("--client-created") + 1] = created
            writer = os.open(tty, os.O_WRONLY | os.O_NONBLOCK | os.O_NOCTTY)
            try:
                deadline = time.monotonic() + 2
                while True:
                    try:
                        os.write(writer, b"x" * 4096)
                    except BlockingIOError:
                        break
                    assert time.monotonic() < deadline, "private terminal did not fill"
                before = fcntl.fcntl(slave, fcntl.F_GETFL)
                blocked = subprocess.run(args, env=env, capture_output=True, timeout=3, check=False)
                assert blocked.returncode == 1, (term, blocked.returncode, blocked.stderr)
                assert fcntl.fcntl(slave, fcntl.F_GETFL) == before, "shared tty flags changed"
            finally:
                os.close(writer)
            print(
                "PASS native direct framing, stale identity, and bounded PTY backpressure: " + term
            )
        finally:
            if client is not None:
                client.terminate()
                deadline = time.monotonic() + 3
                while client.poll() is None and time.monotonic() < deadline:
                    drain()
                    time.sleep(0.01)
                if client.poll() is None:
                    client.kill()
                    deadline = time.monotonic() + 3
                    while client.poll() is None and time.monotonic() < deadline:
                        drain()
                        time.sleep(0.01)
                assert client.poll() is not None, "owned terminal client did not reap"
            tmux("kill-server")
            os.close(slave)
            os.close(master)


for terminal in ("xterm-256color", "tmux-256color"):
    scenario(terminal)

scenario("xterm-256color", malformed_parent=True)
