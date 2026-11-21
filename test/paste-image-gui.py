"""Exercise the documented paste integration in real WezTerm on a private X server."""

from __future__ import annotations

import os
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import BinaryIO, TypeVar

from paste_preview import PROMPT, SIXEL_FRAME, documented_binding, sample_png, write_zshrc
from paste_preview_ssh import ssh_command

T = TypeVar("T")
HOME_DIRS = ("home", "config", "data", "cache", "state", "run", "tmp")


def until(description: str, probe: Callable[[], T], seconds: float = 10) -> T:
    """Poll a test-owned resource while it catches up with the client."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if result := probe():
            return result
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {description}")


def run(args: list[str], env: dict[str, str], *, check: bool = True) -> bytes:
    """Run one bounded fixture command and preserve its diagnostic on failure."""
    result = subprocess.run(args, env=env, capture_output=True, timeout=5, check=False)
    if check:
        assert result.returncode == 0, (args, result.stderr.decode(errors="replace"))
    return result.stdout


@contextmanager
def process(
    args: list[str], env: dict[str, str], log: Path, display: BinaryIO | None = None
) -> Iterator[subprocess.Popen[bytes]]:
    """Reap a fixture process and its process group even if a check fails."""
    fds = (display.fileno(),) if display is not None else ()
    with log.open("wb") as output:
        child = subprocess.Popen(
            args,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=output,
            stderr=output,
            start_new_session=True,
            pass_fds=fds,
        )
        try:
            yield child
        except Exception:
            print(f"{log.name}: {log.read_text(errors='replace')[-4000:]}", file=sys.stderr)
            raise
        finally:
            try:
                os.killpg(child.pid, signal.SIGTERM)
                child.wait(timeout=5)
            except ProcessLookupError:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=5)


def environment(root: Path, binary: Path) -> dict[str, str]:
    """Give one endpoint its own writable state and installed helper lookup."""
    root.mkdir(mode=0o700)
    for name in HOME_DIRS:
        (root / name).mkdir(mode=0o700)
    home = root / "home"
    (home / ".local" / "bin").mkdir(parents=True)
    (home / ".local" / "bin" / "termnav").symlink_to(binary)
    return {
        "HOME": str(home),
        "ZDOTDIR": str(home),
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_CACHE_HOME": str(root / "cache"),
        "XDG_STATE_HOME": str(root / "state"),
        "XDG_RUNTIME_DIR": str(root / "run"),
        "TMPDIR": str(root / "tmp"),
        "PATH": f"{home / '.local' / 'bin'}:{os.environ['PATH']}",
        "LANG": "C.UTF-8",
        "TERM": "xterm-256color",
        "SHELL": shutil.which("zsh") or "zsh",
    }


def write_config(root: Path, mode: str) -> tuple[Path, Path]:
    """Load production Lua and use the documented tmux trigger binding."""
    # WezTerm watches the config's parent directory. Continuous PTY log
    # writes elsewhere in the fixture must not reload the active Lua sender.
    lua = root / "client/config/wezterm.lua"
    lua.write_text(
        """local wezterm = require 'wezterm'
local assets = assert(os.getenv('TERMNAV_TEST_ASSETS')) .. '/lib/termnav/wezterm/'
local routes = dofile(assets .. 'link-routes.lua').new(wezterm)
local paste = dofile(assets .. 'paste-image.lua').new(wezterm, { routes = routes })
wezterm.on('user-var-changed', function(window, pane, name, value)
  paste:user_var_changed(window, pane, name, value)
end)
return {
  enable_wayland = false,
  front_end = 'Software',
  font = wezterm.font('DejaVu Sans Mono'),
  enable_tab_bar = false,
  initial_cols = 110,
  initial_rows = 30,
  keys = { { key = 'v', mods = 'CTRL', action = paste:paste_action() } },
}
""",
        encoding="utf-8",
    )
    tmux = root / "tmux.conf"
    tmux.write_text(
        "set -g status off\n"
        "set -g allow-passthrough on\n"
        "set -as terminal-features 'xterm-256color:RGB:sixel'\n"
        f"set-environment -g TERMNAV_PASTE_IMAGE_PREVIEW {mode}\n" + documented_binding(),
        encoding="utf-8",
    )
    return lua, tmux


def window_draws_preview(window_id: str, env: dict[str, str]) -> bool:
    """Recognize the fixture's two colors in the actual Xvfb window pixels."""
    dump = run(["xwd", "-silent", "-id", window_id], env)
    header = struct.unpack_from(">25I", dump)
    # XWD writes its header in network byte order, independently of the
    # ZPixmap pixel order. WezTerm can use an ARGB window on a 24-bit Xvfb screen.
    assert header[1:3] == (7, 2) and header[3] in (24, 32) and header[11] == 32, header
    palette_entry_size = 12
    width, height = header[4:6]
    stride = header[12]
    start = header[0] + header[19] * palette_entry_size
    red, green, blue = header[14:17]
    mask = red | green | blue
    order = "<" if header[7] == 0 else ">"
    rows = 0
    for offset in range(start, start + stride * height, stride):
        pixels = dump[offset : offset + width * 4]
        red_columns, blue_columns = [], []
        for column, (pixel,) in enumerate(struct.iter_unpack(order + "I", pixels)):
            if pixel & mask == red:
                red_columns.append(column)
            elif pixel & mask == blue:
                blue_columns.append(column)
        # The 120×80 fixture has solid red and blue halves. Tolerate font
        # rounding and antialiasing, while requiring both regions in order.
        if min(len(red_columns), len(blue_columns)) >= 32 and red_columns[-1] < blue_columns[0]:
            rows += 1
            if rows >= 20:
                return True
        else:
            rows = 0
    return False


def exercise(root: Path, binary: Path, assets: Path, display: str, image: Path, mode: str) -> None:
    """Send Ctrl-V through WezTerm and real SSH, then inspect the preview and path."""
    root.mkdir(mode=0o700)
    client = root / "client"
    server = root / "server"
    env = environment(server, binary)
    gui_env = environment(client, binary)
    lua, config = write_config(root, mode)
    write_zshrc(server / "home", assets)
    # Only the local GUI receives DISPLAY. Remote Zsh cannot read the clipboard.
    gui_env.update(DISPLAY=display, LIBGL_ALWAYS_SOFTWARE="1", TERMNAV_TEST_ASSETS=str(assets))
    socket = str(root / "tmux.sock")
    tmux = ["tmux", "-S", socket]
    raw = root / "client.raw"
    input_log = root / "client.input"
    remote = [*tmux, "-f", str(config), "new-session", "-s", "preview", env["SHELL"], "-di"]
    # Record the remote client PTY without putting another local terminal
    # emulator between WezTerm and OpenSSH.
    ssh = ssh_command(
        root,
        ["script", "-qefc", shlex.join(remote), "--log-in", str(input_log), "--log-out", str(raw)],
        env,
    )
    gui = [
        "wezterm-gui",
        "--config-file",
        str(lua),
        "start",
        "--always-new-process",
        "--",
        *ssh,
    ]
    pane = ""

    def screen() -> str:
        return run([*tmux, "capture-pane", "-pJ", "-t", pane], env).decode()

    def stream() -> bytes:
        return raw.read_bytes() if raw.is_file() else b""

    clipboard = ["xclip", "-selection", "clipboard", "-t", "image/png"]
    try:
        with (
            process([*clipboard, "-i", "-quiet", str(image)], gui_env, root / "clipboard.log"),
            process(gui, gui_env, root / "wezterm.log") as window,
        ):
            until("the X11 PNG selection", lambda: run([*clipboard, "-o"], gui_env, check=False))
            until("the remote Zsh prompt", lambda: PROMPT.encode() in stream())
            pane = (
                run([*tmux, "display-message", "-p", "-t", "preview:", "#{pane_id}"], env)
                .decode()
                .strip()
            )
            assert pane.startswith("%") and pane[1:].isdigit(), pane
            if mode == "sixel":
                features = run([*tmux, "list-clients", "-F", "#{client_termfeatures}"], env)
                assert b"sixel" in features.splitlines()[0].split(b","), features
                assert (
                    run([*tmux, "display-message", "-p", "#{sixel_support}"], env).strip() == b"1"
                )
            window_ids = until(
                "the WezTerm window",
                lambda: run(
                    ["xdotool", "search", "--onlyvisible", "--pid", str(window.pid)],
                    gui_env,
                    check=False,
                ),
            )
            (window_id,) = window_ids.decode().split()
            assert not window_draws_preview(window_id, gui_env), "fixture colors before Ctrl-V"
            run(
                [
                    "xdotool",
                    "windowfocus",
                    "--sync",
                    window_id,
                    "key",
                    "--clearmodifiers",
                    "ctrl+v",
                ],
                gui_env,
            )
            clips = server / "cache/termnav/paste-images"
            saved = until("a saved clipboard image", lambda: list(clips.glob("clip-*.png")))
            (clip,) = saved
            path = str(clip)
            assert clip.read_bytes() == image.read_bytes(), "saved PNG differs from clipboard"
            until("the pasted path at the prompt", lambda: f"{PROMPT}{path}" in screen())
            if mode == "sixel":
                frame = until(
                    "a complete sixel frame at the attached client",
                    lambda: SIXEL_FRAME.search(stream()),
                )
                assert b";2;100;0;0" in frame[0] and b";2;0;0;100" in frame[0]
                assert "▀" not in screen(), "sixel mode also drew a half-block preview"
            else:
                until(
                    "the half-block preview at the attached client",
                    lambda: "▀".encode() in stream(),
                )
                assert "▀" in screen().split(f"{PROMPT}{path}", 1)[0], (
                    "preview was not above the prompt"
                )
                assert not SIXEL_FRAME.search(stream()), "blocks mode sent sixel"
            until(
                "the preview's colors in the WezTerm window",
                lambda: window_draws_preview(window_id, gui_env),
            )
            # Capture the editable value without submitting the command.
            run([*tmux, "send-keys", "-t", pane, "C-x", "C-d"], env)
            buffer = server / "home" / "buffer"
            until("an editable buffer snapshot", buffer.is_file)
            assert buffer.read_text() == path, "expected one path in the editable buffer"
            assert screen().count(path) == 1, "expected one visible path"
            assert not list((client / "cache/termnav/paste-images").glob("clip-*.png"))
            staged = client / "run" / f"termnav-{os.getuid()}" / "paste-image"
            until(
                "WezTerm to remove the staged PNG",
                lambda: staged.is_dir() and not list(staged.glob("stage-*.b64")),
            )
            print(
                f"PASS: real WezTerm Ctrl-V over SSH renders {mode} and pastes one path", flush=True
            )
    except Exception:
        print(f"{mode} client output: {stream()[-4000:]!r}", file=sys.stderr)
        if input_log.is_file():
            print(f"{mode} client input: {input_log.read_bytes()[-2000:]!r}", file=sys.stderr)
        if pane:
            contents = run([*tmux, "-N", "capture-pane", "-pJ", "-t", pane], env, check=False)
            print(f"{mode} pane contents: {contents!r}", file=sys.stderr)
            details = run(
                [
                    *tmux,
                    "-N",
                    "display-message",
                    "-p",
                    "#{version}, #{client_termname}",
                    ";",
                    "show-options",
                    "-sqv",
                    "user-keys[14]",
                ],
                env,
                check=False,
            )
            print(f"{mode} tmux details: {details!r}", file=sys.stderr)
        for log in root.glob("ssh-*/sshd.log"):
            print(log.read_text(errors="replace")[-2000:], file=sys.stderr)
        raise
    finally:
        run(["tmux", "-N", "-S", socket, "kill-server"], env, check=False)


def main(binary: Path, assets: Path) -> None:
    """Use a new X server, never the user's desktop or clipboard."""
    with tempfile.TemporaryDirectory(prefix="tn-gui-", dir="/tmp") as directory:
        root = Path(directory)
        image = root / "fixture.png"
        image.write_bytes(sample_png())
        display_file = root / "display"
        with display_file.open("wb", buffering=0) as output:
            args = [
                "Xvfb",
                "-displayfd",
                str(output.fileno()),
                "-screen",
                "0",
                "1280x900x24",
                "-nolisten",
                "tcp",
            ]
            with process(args, {"PATH": os.environ["PATH"]}, root / "xvfb.log", output):
                display = ":" + until(
                    "the private Xvfb display", lambda: display_file.read_text().strip()
                )
                for mode in ("blocks", "sixel"):
                    exercise(root / mode, binary, assets, display, image, mode)


if __name__ == "__main__":
    # Let the context managers reap the private display and SSH connection
    # when the CI runner cancels this process.
    def terminate(signum: int, _frame: object) -> None:
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, terminate)
    main(Path(sys.argv[1]).resolve(strict=True), Path(sys.argv[2]).resolve(strict=True))
