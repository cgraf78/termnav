"""Low-overhead client operations shared by navigation and relay entry points."""

from __future__ import annotations

import json
import os
import secrets
import socket
import subprocess


def new_nonce() -> str:
    """Return the fixed-width request identity used by relay navigation."""

    return secrets.token_hex(6)


def tty_matches(pid: int, expected: str) -> bool:
    """Confirm that a live process still owns the expected controlling tty.

    Linux normally resolves this through procfs without a subprocess. The
    bounded ``ps`` fallback keeps the same PID-reuse guard on macOS, where
    procfs is unavailable.
    """

    try:
        actual = os.readlink(f"/proc/{pid}/fd/0")
    except OSError:
        try:
            actual = subprocess.run(
                ["ps", "-p", str(pid), "-o", "tty="],
                text=True,
                capture_output=True,
                check=False,
                timeout=2,
            ).stdout.strip()
        except (OSError, subprocess.TimeoutExpired):
            actual = ""
        if actual and not actual.startswith("/"):
            actual = f"/dev/{actual}"
    return actual == expected


def send_message(path: str, message: dict, timeout: float = 8) -> dict:
    """Exchange one bounded JSON message with a Termnav relay server."""

    payload = json.dumps(message, separators=(",", ":")) + "\n"
    try:
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(timeout)
            client.connect(path)
            client.sendall(payload.encode())
            raw = b""
            while b"\n" not in raw and len(raw) <= 512:
                chunk = client.recv(513 - len(raw))
                if not chunk:
                    break
                raw += chunk
            reply = json.loads(raw)
            if isinstance(reply, dict) and reply.get("v") == 2:
                return reply
    except (OSError, ValueError):
        pass
    return {"result": "error"}
