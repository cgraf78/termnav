"""Shared PNG and interactive Zsh fixtures for the image paste tests."""

from __future__ import annotations

import re
import shlex
import struct
import zlib
from pathlib import Path

PROMPT = "preview-ci> "
# A complete sixel DCS carries raster attributes, a colour definition, and
# pixel data. Other DCS traffic (such as the terminal version reply) is unrelated.
SIXEL_FRAME = re.compile(rb'\x1bP[0-9;]*q"1;1;\d+;\d+#\d+;2;[0-9;#$!?-]*[@-~][0-9;#$!?-~-]*\x1b\\')


def documented_binding() -> str:
    """Return the innermost-layer tmux example from docs/reference.md."""
    reference = Path(__file__).resolve().parent.parent / "docs" / "reference.md"
    for block in re.findall(r"```tmux\n(.*?)```", reference.read_text(), re.DOTALL):
        if "user-keys[14]" in block:
            return block
    raise AssertionError("docs/reference.md lacks the paste-image tmux example")


def sample_png() -> bytes:
    """Build a 120×80 truecolor PNG with a red left half and a blue right half."""

    def chunk(kind: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
        )

    width, height = 120, 80
    header = struct.pack(">2I5B", width, height, 8, 2, 0, 0, 0)
    row = b"\0" + b"\xff\0\0" * (width // 2) + b"\0\0\xff" * (width // 2)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(row * height))
        + chunk(b"IEND", b"")
    )


def write_zshrc(home: Path, asset_root: Path) -> None:
    """Load the shipped widget and bind Ctrl-X Ctrl-D to snapshot the live buffer."""
    loader = shlex.quote(str(asset_root / "share/termnav/shell.sh"))
    (home / ".zshrc").write_text(
        f"source {loader} || exit 1\n"
        f"PS1={shlex.quote(PROMPT)}\n"
        "bindkey -e\n"
        "preview_snapshot() {\n"
        '  print -rn -- "$BUFFER" >"$HOME/buffer.tmp" || return 1\n'
        '  command mv "$HOME/buffer.tmp" "$HOME/buffer"\n'
        "}\n"
        "zle -N preview_snapshot\n"
        "bindkey '^X^D' preview_snapshot\n"
    )
