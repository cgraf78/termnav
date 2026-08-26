"""Portable, side-effect-free process ancestry and environment queries."""

from __future__ import annotations

import re


def parse_environment(output: str, name: str) -> str | None:
    """Extract one variable from ``ps eww`` output without truncating spaces."""

    pattern = rf"(?:^|\s){re.escape(name)}=(.*?)(?=\s[A-Za-z_][A-Za-z0-9_]*=|$)"
    match = re.search(pattern, output)
    return match.group(1) if match else None


def environment(pid: int, name: str) -> str | None:
    """Read one process variable through procfs or the macOS ``ps`` fallback."""

    if pid <= 0:
        return None
    proc_readable = True
    try:
        with open(f"/proc/{pid}/environ", "rb") as process_environment:
            data = process_environment.read()
    except OSError:
        proc_readable = False
        data = b""
    prefix = name.encode() + b"="
    for entry in data.split(b"\0"):
        if entry.startswith(prefix):
            return entry[len(prefix) :].decode(errors="strict")
    if proc_readable:
        return None

    # Keep subprocess lazy: termnav-relay imports this module on every send,
    # while Linux normally satisfies the lookup through procfs above.
    import subprocess

    try:
        output = subprocess.run(
            ["ps", "eww", "-p", str(pid), "-o", "command="],
            text=True,
            capture_output=True,
            check=False,
            timeout=2,
        ).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    return parse_environment(output, name)


def parent(pid: int) -> int | None:
    """Return a live process's parent PID without assuming procfs exists."""

    if pid <= 0:
        return None
    try:
        with open(f"/proc/{pid}/status", encoding="utf-8") as status:
            for line in status:
                if line.startswith("PPid:"):
                    value = line.split(":", 1)[1].strip()
                    return int(value) if value.isdigit() else None
    except OSError:
        pass

    import subprocess

    try:
        value = subprocess.run(
            ["ps", "-p", str(pid), "-o", "ppid="],
            text=True,
            capture_output=True,
            check=False,
            timeout=2,
        ).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        return None
    return int(value) if value.isdigit() else None
