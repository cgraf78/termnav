"""Drive `termnav paste-image` through real private tmux servers.

A fake WezTerm owns the pty of an attached tmux client. It decodes the
receiver's TERMNAV_PASTE_IMAGE announcements from the client's terminal output
and types the staged transfer back with the same flow control as
lib/termnav/wezterm/paste-image.lua. Every server uses a private `-S` socket
and an isolated environment; the user's tmux is never contacted.
"""

from __future__ import annotations

import base64
import hashlib
import os
import pty
import re
import select
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import traceback
from pathlib import Path

WINDOW = 512 * 1024
CHUNK = 32 * 1024
ACK_INTERVAL = 128 * 1024
USER_VAR = re.compile(rb"\x1b\]1337;SetUserVar=TERMNAV_PASTE_IMAGE=([A-Za-z0-9+/=]*)\x07")
XTVERSION_QUERY = b"\x1b[>q"
TRIGGER = b"\x1b[777010u"
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"

# Pane program that records everything it receives, with bracketed paste on.
CAPTURE = r"""
import os, sys, termios, tty
tty.setraw(0)
os.write(1, b"\x1b[?2004h")
with open(sys.argv[1], "ab", buffering=0) as log:
    while True:
        data = os.read(0, 65536)
        if not data:
            break
        log.write(data)
"""

FAILURES: list[str] = []


class Unsupported(Exception):
    """The platform cannot run the receiver's terminal path."""


def check(name: str, condition: bool, detail: object = "") -> None:
    """Record one assertion in the suite's PASS/FAIL vocabulary."""
    if condition:
        print(f"  PASS: {name}")
    else:
        print(f"  FAIL: {name} {detail}")
        FAILURES.append(name)


def png(size: int, seed: int) -> bytes:
    """Return deterministic PNG-signed bytes of exactly `size`."""
    body = bytearray()
    counter = 0
    while len(body) < size - len(PNG_SIGNATURE):
        body += hashlib.sha256(f"{seed}:{counter}".encode()).digest()
        counter += 1
    return PNG_SIGNATURE + bytes(body[: size - len(PNG_SIGNATURE)])


class Harness:
    """Own one scratch root, its environment, and every process started."""

    def __init__(self, binary: str, root: Path) -> None:
        self.binary = binary
        self.root = root
        self.servers: list[str] = []
        self.clients: list[int] = []
        self.popups: list[subprocess.Popen] = []
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("TMUX", "NVIM", "SSH_", "WEZTERM_", "TERMNAV_", "XDG_"))
            and key
            not in ("TERM_PROGRAM", "TERM_PROGRAM_VERSION", "STY", "DISPLAY", "WAYLAND_DISPLAY")
        }
        env.update(
            HOME=str(root / "home"),
            TMPDIR=str(root / "tmp"),
            XDG_CACHE_HOME=str(root / "cache"),
            XDG_CONFIG_HOME=str(root / "config"),
            XDG_RUNTIME_DIR=str(root / "run"),
            TERM="xterm-256color",
        )
        for name in ("home", "tmp", "cache", "config", "run"):
            (root / name).mkdir(mode=0o700)
        self.env = env

    def tmux(self, socket: str, *arguments: str) -> str:
        """Run one bounded command against a private server."""
        assert socket.startswith(str(self.root)), socket
        result = subprocess.run(
            ["tmux", "-S", socket, *arguments],
            env=self.env,
            capture_output=True,
            check=False,
            text=True,
            timeout=10,
        )
        assert result.returncode == 0, (arguments, result.stderr)
        return result.stdout

    def server(self, name: str, command: str) -> str:
        """Start a private server whose first pane runs `command`."""
        socket = str(self.root / f"{name}.sock")
        self.tmux(
            socket,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "100",
            "-y",
            "30",
            command,
        )
        self.servers.append(socket)
        self.tmux(socket, "set-option", "-g", "status", "off")
        return socket

    def pane(self, socket: str) -> str:
        """Return the first pane id, refusing an empty answer."""
        pane = self.tmux(
            socket, "display-message", "-p", "-t", f"{Path(socket).stem}:", "#{pane_id}"
        ).strip()
        assert re.fullmatch(r"%\d+", pane), pane
        return pane

    def capture_command(self, log: Path) -> str:
        """Shell text for a pane that records its raw input."""
        script = self.root / "capture.py"
        script.write_text(CAPTURE)
        return f"{shlex.quote(sys.executable)} {shlex.quote(str(script))} {shlex.quote(str(log))}"

    def attach(self, socket: str, session: str) -> FakeWezTerm:
        """Attach a client on a pty that impersonates WezTerm."""
        child, master = pty.fork()
        if child == 0:
            environment = dict(self.env, TERM_PROGRAM="WezTerm")
            os.execvpe("tmux", ["tmux", "-S", socket, "attach-session", "-t", session], environment)
        self.clients.append(child)
        os.set_blocking(master, False)
        terminal = FakeWezTerm(master)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            terminal.pump(0.05)
            clients = self.tmux(
                socket, "list-clients", "-F", "#{client_pid} #{client_tty} #{client_name}"
            )
            for line in clients.splitlines():
                pid, tty_name, name = line.split(" ", 2)
                if int(pid) == child:
                    terminal.client = (pid, tty_name, name)
                    try:
                        os.close(os.open(tty_name, os.O_WRONLY | os.O_NOCTTY))
                    except PermissionError as error:
                        # Android can deny reopening another process's PTY,
                        # which every outward announcement requires.
                        raise Unsupported(str(error)) from error
                    return terminal
        raise AssertionError("tmux client never attached")

    def close(self) -> None:
        """Kill only the servers and clients this harness started."""
        for popup in self.popups:
            if popup.poll() is None:
                popup.kill()
            popup.wait(timeout=10)
        for socket in self.servers:
            subprocess.run(
                ["tmux", "-S", socket, "kill-server"],
                env=self.env,
                capture_output=True,
                check=False,
                timeout=10,
            )
        for child in self.clients:
            try:
                os.kill(child, signal.SIGKILL)
                os.waitpid(child, 0)
            except (ProcessLookupError, ChildProcessError):
                pass


class FakeWezTerm:
    """The terminal side of one attached client."""

    def __init__(self, master: int) -> None:
        self.master = master
        self.output = b""
        self.client: tuple[str, str, str] = ("", "", "")
        self.announcements: list[str] = []

    def pump(self, timeout: float, outgoing: bytes = b"") -> int:
        """Drain terminal output, answer probes, and write what fits."""
        writers = [self.master] if outgoing else []
        readable, writable, _ = select.select([self.master], writers, [], timeout)
        written = 0
        if writable:
            try:
                written = os.write(self.master, outgoing)
            except BlockingIOError:
                written = 0
        if readable:
            try:
                data = os.read(self.master, 1 << 20)
            except (BlockingIOError, OSError):
                data = b""
            self.output = (self.output + data)[-(1 << 20) :]
            if XTVERSION_QUERY in self.output:
                self.output = self.output.replace(XTVERSION_QUERY, b"")
                os.write(self.master, b"\x1bP>|WezTerm 20240203-110809-5046fc22\x1b\\")
            for match in USER_VAR.finditer(self.output):
                self.announcements.append(base64.b64decode(match.group(1)).decode())
            self.output = USER_VAR.sub(b"", self.output)
        return written

    def type(self, data: bytes, timeout: float = 5) -> None:
        """Type bytes into the client."""
        deadline = time.monotonic() + timeout
        while data:
            assert time.monotonic() < deadline, "typing timed out"
            data = data[self.pump(0.05, data) :]

    def transfer(
        self,
        staged: bytes,
        *,
        armed: bool = True,
        mutate=None,
        limit: float = 60,
        cancel_delay: float = 0,
    ) -> dict:
        """Answer `ready` like the WezTerm module and return what happened."""
        stream = None
        header_length = 0
        proof = ""
        sent = 0
        acked = None
        acks: list[int] = []
        max_in_flight = 0
        result = {"verdict": None, "acks": acks, "ready_at": None, "done_at": None}
        deadline = time.monotonic() + limit
        seen = 0
        while time.monotonic() < deadline:
            outgoing = b""
            if stream is not None and result["verdict"] is None:
                # Header only until an acknowledgement proves the receiver
                # read it; then a 512 KiB window.
                window_end = acked + WINDOW if acked is not None else header_length
                end = min(len(stream), window_end, sent + CHUNK)
                outgoing = stream[sent:end]
            written = self.pump(0.02, outgoing)
            sent += written
            if acked is None:
                max_in_flight = max(max_in_flight, sent)
            for value in self.announcements[seen:]:
                kind, _, rest = value.partition(":")
                announced_id, _, rest = rest.partition(":")
                offset, _, announced_proof = rest.partition(":")
                if kind == "ready" and armed and stream is None:
                    stream = announced_id.encode() + b":" + staged + b"!"
                    if mutate:
                        stream = mutate(stream)
                    # Like the module, take the header and proof from the
                    # bytes actually sent.
                    fields = stream.split(b":", 3)
                    header_length = sum(len(field) + 1 for field in fields[:3])
                    proof = fields[2][:16].decode()
                    result["ready_at"] = time.monotonic()
                    result["id"] = announced_id
                elif kind == "ready":
                    result["ready_ignored"] = True
                elif kind == "cancel" and stream is not None:
                    # Any cancel from this terminal ends the transfer, even
                    # under another id.
                    result["verdict"] = kind
                    result["done_at"] = time.monotonic()
                    if sent < len(stream):
                        pause = time.monotonic() + cancel_delay
                        while time.monotonic() < pause:
                            self.pump(0.02)
                        self.type(b"!")
                elif stream is not None and announced_id != result["id"]:
                    result["foreign"] = value
                elif kind == "ack" and stream is not None:
                    acks.append(int(offset))
                    result["proofs_valid"] = result.get("proofs_valid", True) and (
                        announced_proof == proof
                    )
                    if announced_proof == proof:
                        acked = int(offset)
                elif kind in ("done", "cancel") and stream is not None:
                    result["verdict"] = kind
                    result["done_at"] = time.monotonic()
                    if kind == "cancel" and sent < len(stream):
                        self.type(b"!")
                elif kind == "cancel":
                    result["verdict"] = kind
            seen = len(self.announcements)
            if result["verdict"] is not None:
                break
        result["sent"] = sent
        result["first_window_peak"] = max_in_flight
        result["stream_length"] = len(stream) if stream is not None else 0
        return result


def documented_binding() -> str:
    """Return the innermost-layer tmux example from docs/reference.md."""
    reference = Path(__file__).resolve().parent.parent / "docs" / "reference.md"
    for block in re.findall(r"```tmux\n(.*?)```", reference.read_text(), re.DOTALL):
        if "user-keys[14]" in block:
            return block
    raise AssertionError("docs/reference.md lacks the paste-image tmux example")


def wait_for(path: Path, terminal: FakeWezTerm, timeout: float = 10) -> str:
    """Wait for a status file written by the popup wrapper.

    The fake terminal keeps draining its pty meanwhile: macOS ptys buffer
    little, and a receiver announcing `cancel` to an unread client tty would
    block before it could exit.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists() and path.read_text().endswith("\n"):
            return path.read_text().strip()
        terminal.pump(0.02)
    return ""


def images(harness: Harness) -> list[Path]:
    """Saved images in the receiver's private directory."""
    directory = harness.root / "cache" / "termnav" / "paste-images"
    return sorted(directory.glob("clip-*.png")) if directory.is_dir() else []


def stage(harness: Harness, image: bytes) -> bytes:
    """Stage `image` through the fake clipboard adapter and return the text."""
    clipboard = harness.root / "clipboard.png"
    clipboard.write_bytes(image)
    adapter = harness.root / "adapter"
    adapter.write_text(
        "#!/bin/sh\n"
        'case "$1" in\n'
        "  types) printf 'image/png\\n' ;;\n"
        f"  png) cat {shlex.quote(str(clipboard))} ;;\n"
        "esac\n"
    )
    adapter.chmod(0o700)
    result = subprocess.run(
        [harness.binary, "paste-image", "stage"],
        env=dict(harness.env, TERMNAV_PASTE_IMAGE_CLIPBOARD=str(adapter)),
        capture_output=True,
        check=False,
        timeout=10,
    )
    assert result.returncode == 0, (result.returncode, result.stderr)
    path = Path(result.stdout.decode().strip())
    staged = path.read_bytes()
    path.unlink()
    return staged


def open_receiver(
    harness: Harness, socket: str, terminal: FakeWezTerm, pane: str, status: Path
) -> None:
    """Open the popup the tmux binding would open, recording its status."""
    pid, tty_name, name = terminal.client
    receive = shlex.join(
        [
            harness.binary,
            "paste-image",
            "receive",
            "--target-pane",
            pane,
            "--client-name",
            name,
            "--client-pid",
            pid,
            "--client-tty",
            tty_name,
            "--tmux-socket",
            socket,
        ]
    )
    # tmux shows one popup per client: let the previous one close, and drain
    # its final announcements, before the next receiver starts.
    for popup in harness.popups:
        deadline = time.monotonic() + 10
        while popup.poll() is None and time.monotonic() < deadline:
            terminal.pump(0.02)
    terminal.pump(0.05)
    status.unlink(missing_ok=True)
    terminal.announcements.clear()
    command = f"{receive}; echo $? > {shlex.quote(str(status))}"
    # A command client's display-popup waits for the popup to close, so it
    # runs beside the fake terminal; close() reaps it.
    harness.popups.append(
        subprocess.Popen(
            [
                "tmux",
                "-S",
                socket,
                "display-popup",
                "-c",
                name,
                "-w",
                "70",
                "-h",
                "5",
                "-E",
                command,
            ],
            env=harness.env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    )


def single_layer(harness: Harness, large: int) -> None:
    """Cover delivery, validation failures, and timing on one server."""
    log = harness.root / "pane-a.log"
    socket = harness.server("a", harness.capture_command(log))
    pane = harness.pane(socket)
    status = harness.root / "status"
    # The binding documented in docs/reference.md, recording the receiver's
    # status.
    binding = documented_binding().replace(
        "termnav paste-image receive", f"{harness.binary} paste-image receive"
    )
    binding = binding.replace(
        "--tmux-socket #{q:socket_path}'", f"--tmux-socket #{{q:socket_path}}; echo $? > {status}'"
    )
    assert harness.binary in binding and str(status) in binding, binding
    (harness.root / "binding.conf").write_text(binding)
    harness.tmux(socket, "source-file", str(harness.root / "binding.conf"))
    terminal = harness.attach(socket, "a")

    print("=== paste-image: single tmux layer ===")
    image = png(70_000, 1)
    staged = stage(harness, image)
    check("stage writes the documented header", staged.startswith(f"{len(image)}:".encode()))
    terminal.announcements.clear()
    terminal.type(TRIGGER)
    result = terminal.transfer(staged)
    check("trigger key opens a receiver that announces done", result["verdict"] == "done", result)
    check("receive exits 0", wait_for(status, terminal) == "0")
    saved = images(harness)
    check("one image is saved", len(saved) == 1, saved)
    if saved:
        check("saved PNG is byte-identical", saved[0].read_bytes() == image)
        check("saved PNG is owner-only", stat.S_IMODE(saved[0].stat().st_mode) == 0o600)
        check(
            "image directory is owner-only",
            stat.S_IMODE(saved[0].parent.stat().st_mode) == 0o700,
        )
        deadline = time.monotonic() + 5
        expected = b"\x1b[200~" + str(saved[0]).encode() + b"\x1b[201~"
        while time.monotonic() < deadline and expected not in log.read_bytes():
            terminal.pump(0.05)
        check(
            "path is bracket-pasted into the pane once",
            log.read_bytes() == expected,
            log.read_bytes(),
        )
    header_length = len(result["id"]) + 1 + staged.index(b":", staged.index(b":") + 1) + 1
    check(
        "first acknowledgement is exactly the header",
        bool(result["acks"]) and result["acks"][0] == header_length,
        result["acks"][:1],
    )
    check("acknowledgements carry the digest proof", result.get("proofs_valid") is True)
    check(
        "only the header is typed before the first acknowledgement",
        result["first_window_peak"] <= header_length,
    )
    buffers = harness.tmux(socket, "list-buffers", "-F", "#{buffer_name}")
    check("no paste buffer is left behind", "termnav" not in buffers, buffers)

    print("=== paste-image: acknowledgements ===")
    log.write_bytes(b"")
    image = png(1_200_000, 2)
    staged = stage(harness, image)
    open_receiver(harness, socket, terminal, pane, status)
    result = terminal.transfer(staged)
    acks = result["acks"]
    check(
        "larger transfer completes",
        result["verdict"] == "done" and wait_for(status, terminal) == "0",
        result["verdict"],
    )
    check(
        "acknowledgements are distinct and increasing",
        acks == sorted(set(acks)) and len(acks) >= 8,
        acks,
    )
    check(
        "acknowledgements are spaced by 128 KiB",
        # Indexing rather than itertools.pairwise keeps Python 3.9 working.
        all(acks[i + 1] - acks[i] >= ACK_INTERVAL for i in range(len(acks) - 1)),
        acks,
    )
    check("two images saved", len(images(harness)) == 2)

    def failure(name: str, image_bytes: bytes, mutate, cancel_delay: float = 0) -> None:
        log.write_bytes(b"")
        before = images(harness)
        staged_bytes = stage(harness, image_bytes)
        open_receiver(harness, socket, terminal, pane, status)
        outcome = terminal.transfer(staged_bytes, mutate=mutate, cancel_delay=cancel_delay)
        check(f"{name}: receiver cancels", outcome["verdict"] == "cancel", outcome["verdict"])
        # WezTerm's terminator ends the receiver's discard promptly.
        check(f"{name}: receive exits 1 promptly", wait_for(status, terminal, 5) == "1")
        check(f"{name}: no image is written", images(harness) == before)
        time.sleep(0.3)
        terminal.pump(0.1)
        check(
            f"{name}: no transfer byte reaches the pane",
            log.read_bytes() == b"",
            log.read_bytes()[:80],
        )

    print("=== paste-image: rejected transfers ===")

    def corrupt_digest(stream: bytes) -> bytes:
        header = stream.index(b":", stream.index(b":") + 1) + 1
        flipped = b"0" if stream[header : header + 1] != b"0" else b"1"
        return stream[:header] + flipped + stream[header + 1 :]

    failure("checksum mismatch", png(300_000, 3), corrupt_digest)
    failure("invalid byte mid-stream", png(600_000, 4), lambda s: s[:200_000] + b"\n" + s[200_001:])

    def gif_signature(stream: bytes) -> bytes:
        data = stream.index(b":", stream.index(b":", stream.index(b":") + 1) + 1) + 1
        return stream[:data] + b"R0lGODlh" + stream[data + 8 :]

    failure("non-PNG payload", png(300_000, 5), gif_signature)
    failure("truncated payload", png(300_000, 6), lambda s: s[:150_000] + b"!")

    def foreign_id(stream: bytes) -> bytes:
        # A forged ready won the race: the real receiver sees another id.
        return (b"f" if stream[:1] != b"f" else b"e") + stream[1:]

    # The terminator arrives later than a keystroke's quiet period: the
    # receiver must keep discarding a foreign transfer until it does.
    failure("transfer for another receiver", png(300_000, 11), foreign_id, cancel_delay=2.5)
    # Small enough to be in flight whole, terminator included, before the
    # receiver rejects it.
    failure(
        "invalid byte in a small payload", png(2_000, 10), lambda s: s[:500] + b"\x7f" + s[501:]
    )

    print("=== paste-image: unsafe output directory ===")
    log.write_bytes(b"")
    directory = harness.root / "cache" / "termnav" / "paste-images"
    moved = harness.root / "cache" / "moved-images"
    directory.rename(moved)
    directory.symlink_to(moved)
    staged = stage(harness, png(5000, 7))
    open_receiver(harness, socket, terminal, pane, status)
    result = terminal.transfer(staged)
    check(
        "symlinked image directory: receiver cancels",
        result["verdict"] == "cancel",
        result["verdict"],
    )
    check("symlinked image directory: receive exits 1", wait_for(status, terminal) == "1")
    check("symlinked image directory: nothing written", len(list(moved.glob("clip-*"))) == 2)
    directory.unlink()
    moved.rename(directory)

    print("=== paste-image: cancelled before transfer ===")
    log.write_bytes(b"")
    open_receiver(harness, socket, terminal, pane, status)
    started = time.monotonic()
    result = terminal.transfer(b"", armed=False, limit=1.5)
    check("unarmed terminal ignores ready", result.get("ready_ignored") is True)
    terminal.type(b"\x1b")
    code = wait_for(status, terminal, 5)
    check("Esc closes a waiting receiver", code == "1", code)
    check("Esc closes it within the quiet period", time.monotonic() - started < 5)
    check(
        "Esc reaches neither the pane nor a file",
        log.read_bytes() == b"" and len(images(harness)) == 2,
    )

    if large:
        print("=== paste-image: 16 MiB timing ===")
        log.write_bytes(b"")
        image = png(large, 8)
        staged = stage(harness, image)
        open_receiver(harness, socket, terminal, pane, status)
        result = terminal.transfer(staged, limit=120)
        elapsed = (result["done_at"] or 0) - (result["ready_at"] or 0)
        check(
            "16 MiB transfer completes",
            result["verdict"] == "done" and wait_for(status, terminal, 30) == "0",
            (result["verdict"], terminal.announcements, result["sent"]),
        )
        newest = max(images(harness), key=lambda path: path.stat().st_mtime_ns)
        check("16 MiB image is byte-identical", newest.read_bytes() == image)
        print(f"  TIMING: {large} bytes ({len(staged)} staged) in {elapsed:.2f} s")


def nested_layer(harness: Harness) -> None:
    """Deliver through a local tmux client running inside another tmux."""
    print("=== paste-image: nested tmux layer ===")
    log = harness.root / "pane-inner.log"
    inner = harness.server("inner", harness.capture_command(log))
    inner_pane = harness.pane(inner)
    # An empty TMUX lets the inner client attach from a pane. A Python parent
    # keeps the outer pane identity in an inspectable environment, as an
    # interactive shell does: macOS hides the environment of protected
    # platform binaries such as /bin/sh from `ps`.
    attach_inner = (
        "import os, subprocess, sys; "
        "sys.exit(subprocess.call(['tmux', '-S', sys.argv[1], 'attach-session', '-t', 'inner'], "
        "env=dict(os.environ, TMUX='')))"
    )
    outer = harness.server(
        "outer",
        f"{shlex.quote(sys.executable)} -c {shlex.quote(attach_inner)} {shlex.quote(inner)}",
    )
    # Where the inner client's ancestry cannot be inspected, the route falls
    # back to one passthrough frame, which tmux 3.3 and later gate behind
    # this option; tmux 3.2 has no such option and always forwards.
    subprocess.run(
        ["tmux", "-S", outer, "set-option", "-g", "allow-passthrough", "on"],
        env=harness.env,
        capture_output=True,
        check=False,
        timeout=10,
    )
    terminal = harness.attach(outer, "outer")
    deadline = time.monotonic() + 5
    inner_client = None
    while time.monotonic() < deadline and inner_client is None:
        terminal.pump(0.05)
        for line in harness.tmux(
            inner, "list-clients", "-F", "#{client_pid} #{client_tty} #{client_name}"
        ).splitlines():
            inner_client = tuple(line.split(" ", 2))
    assert inner_client, "inner client never attached"
    staged = stage(harness, png(400_000, 9))
    proxy = FakeWezTerm(terminal.master)
    proxy.client = inner_client  # the popup belongs to the inner client
    status = harness.root / "status-nested"
    before = images(harness)
    open_receiver(harness, inner, proxy, inner_pane, status)
    result = terminal.transfer(staged)
    check("nested: ready reaches the outermost terminal", result["ready_at"] is not None, result)
    check(
        "nested: transfer completes",
        result["verdict"] == "done" and wait_for(status, terminal) == "0",
        result["verdict"],
    )
    saved = [path for path in images(harness) if path not in before]
    check("nested: one image saved", len(saved) == 1)
    if saved:
        deadline = time.monotonic() + 5
        expected = b"\x1b[200~" + str(saved[0]).encode() + b"\x1b[201~"
        while time.monotonic() < deadline and expected not in log.read_bytes():
            terminal.pump(0.05)
        check(
            "nested: path is pasted into the inner pane",
            log.read_bytes() == expected,
            log.read_bytes(),
        )


def main() -> int:
    """Run every scenario under one isolated scratch root."""
    binary = str(Path(sys.argv[1]).resolve())
    large = int(os.environ.get("TERMNAV_PASTE_IMAGE_LARGE_BYTES", str(16 * 1024 * 1024)))
    if not shutil.which("tmux"):
        print("SKIP: paste-image tests require tmux")
        return 0
    parent = "/tmp" if Path("/tmp").is_dir() and os.access("/tmp", os.W_OK) else None
    with tempfile.TemporaryDirectory(prefix="tn-paste-", dir=parent) as root:
        harness = Harness(binary, Path(root).resolve())
        try:
            single_layer(harness, large)
            nested_layer(harness)
        except Unsupported as error:
            print(f"SKIP: platform forbids reopening the client PTY: {error}")
            return 0
        except Exception:  # noqa: BLE001 - report any harness error as a failed scenario
            # The suite keys on FAIL lines; name the crash so CI logs say
            # which scenario stopped and why.
            print(f"  FAIL: scenario raised:\n{traceback.format_exc()}")
            FAILURES.append("scenario raised")
        finally:
            harness.close()
    if FAILURES:
        print(f"FAILED: {len(FAILURES)} paste-image checks")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
