"""Exercise the documented paste integration in a real, isolated WezTerm GUI."""

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
from contextlib import contextmanager, nullcontext
from pathlib import Path
from typing import BinaryIO, TypeVar

from paste_preview import (
    PROMPT,
    SIXEL_FRAME,
    documented_binding,
    sample_png,
    write_zshrc,
)
from paste_preview_ssh import ssh_command

T = TypeVar("T")
HOME_DIRS = ("home", "config", "data", "cache", "state", "run", "tmp")


def until(
    description: str,
    probe: Callable[[], T],
    seconds: float = 10,
    *,
    detail: Callable[[], str] | None = None,
) -> T:
    """Poll a test-owned resource while it catches up with the client."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if result := probe():
            return result
        time.sleep(0.05)
    diagnostic = detail() if detail else ""
    message = f"timed out waiting for {description}"
    raise AssertionError(f"{message}\n{diagnostic}" if diagnostic else message)


def run(args: list[str], env: dict[str, str], *, check: bool = True) -> bytes:
    """Run one bounded fixture command and preserve its diagnostic on failure."""
    result = subprocess.run(args, env=env, capture_output=True, timeout=5, check=False)
    if check:
        assert result.returncode == 0, (args, result.stderr.decode(errors="replace"))
    return result.stdout


def signal_group(child: subprocess.Popen[bytes], sig: int) -> bool:
    """Signal the group still owned by this session, if present."""
    try:
        os.killpg(child.pid, sig)
    except ProcessLookupError:
        return False
    except PermissionError:
        # Darwin can report EPERM for a GUI group after its leader exits.
        if child.poll() is None:
            raise
        return False
    return True


def stop_process(child: subprocess.Popen[bytes]) -> None:
    """Reap the leader and give its entire process group a shutdown deadline."""
    if signal_group(child, signal.SIGTERM):
        deadline = time.monotonic() + 5
        while True:
            child.poll()
            if not signal_group(child, 0):
                break
            if time.monotonic() >= deadline:
                signal_group(child, signal.SIGKILL)
                break
            time.sleep(0.05)
    child.wait(timeout=5)


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
            stop_process(child)


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
local trigger = os.getenv('TERMNAV_TEST_PASTE_TRIGGER')
local paste_wezterm = wezterm
local report
if trigger then
  local status_path = assert(os.getenv('TERMNAV_TEST_PASTE_STATUS'))
  report = function(message)
    local status = assert(io.open(status_path, 'a'))
    status:write(message, '\\n')
    status:close()
  end
  -- The real paste action declines staging silently. Capture the fixture's
  -- child outcome without launching another clipboard read on a timeout.
  paste_wezterm = setmetatable({
    run_child_process = function(args)
      report('stage started')
      local ok, succeeded, stdout, stderr = pcall(wezterm.run_child_process, args)
      if not ok then
        report('stage raised: ' .. tostring(succeeded))
        error(succeeded)
      end
      if succeeded then
        report('stage succeeded')
      else
        local detail = stderr == '' and 'no child diagnostic' or tostring(stderr)
        report('stage failed: ' .. detail:sub(1, 2000))
      end
      return succeeded, stdout, stderr
    end,
  }, { __index = wezterm })
end
local paste = dofile(assets .. 'paste-image.lua').new(paste_wezterm, { routes = routes })
local paste_action = paste:paste_action()
if trigger and wezterm.gui then
  -- macOS CI can run the actual action inside WezTerm without Accessibility
  -- permissions for posting keyboard events. Poll independently of status
  -- updates, since an idle window can miss the request after the prompt.
  local last_owner
  local function try_paste()
    local pending = io.open(trigger, 'r')
    if not pending then return false end
    pending:close()
    -- This fixture starts one window in its own WezTerm process.
    local window = wezterm.gui.gui_windows()[1]
    local pane = window and window:active_pane()
    if not pane then return false end
    -- The remote prompt can arrive before WezTerm sees the tmux user vars.
    local owner = routes.terminal_owner(pane)
    if owner ~= 'tmux' then
      if owner ~= last_owner then report('waiting for tmux ownership: ' .. owner) end
      last_owner = owner
      return false
    end
    if not os.remove(trigger) then return false end
    report('consumed trigger: ' .. owner)
    window:perform_action(paste_action, pane)
    return true
  end
  local function poll()
    if not try_paste() then wezterm.time.call_after(0.1, poll) end
  end
  wezterm.time.call_after(0.1, poll)
end
wezterm.on('user-var-changed', function(window, pane, name, value)
  paste:user_var_changed(window, pane, name, value)
end)
return {
  enable_wayland = false,
  -- On macOS both OpenGL and Software request an accelerated CGL pixel
  -- format. Use the native WebGPU backend on the hosted virtual display.
  front_end = trigger and 'WebGpu' or 'Software',
  font = wezterm.font(trigger and 'Menlo' or 'DejaVu Sans Mono'),
  enable_tab_bar = false,
  initial_cols = 110,
  initial_rows = 30,
  keys = { { key = 'v', mods = 'CTRL', action = paste_action } },
}
""",
        encoding="utf-8",
    )
    tmux = root / "tmux.conf"
    # tmux 3.8 skips its fast-input heuristic for bracketed-paste clients.
    # Exercise its default here while the PTY fixture retains the older wiring.
    tmux.write_text(
        "set -g status off\n"
        "set -g allow-passthrough on\n"
        "set -as terminal-features 'xterm-256color:RGB:sixel'\n"
        f"set-environment -g TERMNAV_PASTE_IMAGE_PREVIEW {mode}\n"
        + documented_binding()
        + "set -gu assume-paste-time\n",
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


class WindowUnavailable(Exception):
    """The native window server could not capture a current window."""


def mac_window_preview(helper: str, pid: int, capture: Path, env: dict[str, str]) -> bool:
    """Return the pixel probe for a fresh capture of the current WezTerm window."""
    window_ids = run([helper, "window", str(pid)], env).decode().split()
    if not window_ids:
        raise WindowUnavailable("WezTerm has no visible window")
    (window_id,) = window_ids
    capture.unlink(missing_ok=True)
    args = ["/usr/sbin/screencapture", "-x", "-o", f"-l{window_id}", str(capture)]
    try:
        result = subprocess.run(args, env=env, capture_output=True, timeout=5, check=False)
    except subprocess.TimeoutExpired as exc:
        raise WindowUnavailable(f"screencapture timed out for window {window_id}") from exc
    if result.returncode or not capture.is_file():
        error = result.stderr.decode(errors="replace").strip()
        raise WindowUnavailable(
            f"screencapture for window {window_id} exited {result.returncode}: "
            f"{error or 'no screenshot produced'}"
        )
    return run([helper, "preview", str(capture)], env).strip() == b"yes"


def wait_for_preview(
    description: str, probe: Callable[[], bool], *, before_paste: bool = False
) -> None:
    """Wait for a usable frame before paste or a colored frame after paste."""
    unavailable = ""

    def ready() -> bool:
        nonlocal unavailable
        try:
            colored = probe()
        except WindowUnavailable as exc:
            unavailable = str(exc)
            return False
        unavailable = ""
        if before_paste:
            assert not colored, "fixture colors before paste"
            return True
        return colored

    until(description, ready, detail=lambda: unavailable)


def exercise(
    root: Path, binary: Path, assets: Path, display: str | None, image: Path, mode: str
) -> None:
    """Invoke WezTerm's paste action over real SSH, then inspect the preview and path."""
    root.mkdir(mode=0o700)
    client = root / "client"
    server = root / "server"
    env = environment(server, binary)
    gui_env = environment(client, binary)
    lua, config = write_config(root, mode)
    write_zshrc(server / "home", assets)
    gui_env["TERMNAV_TEST_ASSETS"] = str(assets)
    socket = str(root / "tmux.sock")
    tmux = ["tmux", "-S", socket]
    raw = root / "client.raw"
    input_log = root / "client.input"
    remote = [*tmux, "-f", str(config), "new-session", "-s", "preview", env["SHELL"], "-di"]
    # Record the remote client PTY without putting another local terminal
    # emulator between WezTerm and OpenSSH.
    if display is None:
        # Only CI uses the native pasteboard. Apple sshd expects an audit
        # address that the private stdio proxy transport cannot supply.
        helper = str(root.parent / "paste-preview-macos")
        trigger = root / "paste-trigger"
        paste_status = root / "paste-status"
        gui_env["TERMNAV_TEST_PASTE_TRIGGER"] = str(trigger)
        gui_env["TERMNAV_TEST_PASTE_STATUS"] = str(paste_status)
        recording = ["/usr/bin/script", "-qF", str(raw), *remote]
        sshd = Path(os.environ["TERMNAV_TEST_SSHD"]).resolve(strict=True)
        clipboard = nullcontext()
    else:
        # Only the local GUI receives DISPLAY. Remote Zsh cannot read the clipboard.
        gui_env.update(DISPLAY=display, LIBGL_ALWAYS_SOFTWARE="1")
        recording = [
            "script",
            "-qefc",
            shlex.join(remote),
            "--log-in",
            str(input_log),
            "--log-out",
            str(raw),
        ]
        sshd = Path("/usr/sbin/sshd")
        selection = ["xclip", "-selection", "clipboard", "-t", "image/png"]
        clipboard = process(
            [*selection, "-i", "-quiet", str(image)], gui_env, root / "clipboard.log"
        )
    ssh = ssh_command(root, recording, env, sshd=sshd)
    pinned_cli = os.environ.get("CI_WEZTERM_PINNED_BIN")
    wezterm_gui = os.environ.get("CI_WEZTERM_PINNED_GUI") or (
        str(Path(pinned_cli).with_name("wezterm-gui")) if pinned_cli else "wezterm-gui"
    )
    gui = [
        wezterm_gui,
        "--config-file",
        str(lua),
        "start",
        "--always-new-process",
        "--",
        *ssh,
    ]
    pane = ""
    window_id = ""

    def screen() -> str:
        return run([*tmux, "capture-pane", "-pJ", "-t", pane], env).decode()

    def stream() -> bytes:
        return raw.read_bytes() if raw.is_file() else b""

    def draws_preview() -> bool:
        if display is not None:
            return window_draws_preview(window_id, gui_env)
        return mac_window_preview(helper, window.pid, root / "window.png", gui_env)

    try:
        with clipboard, process(gui, gui_env, root / "wezterm.log") as window:
            if display is None:
                run([helper, "clipboard", str(image)], os.environ.copy())
            else:
                until(
                    "the X11 PNG selection", lambda: run([*selection, "-o"], gui_env, check=False)
                )
                find_window = ["xdotool", "search", "--onlyvisible", "--pid", str(window.pid)]
            until("the remote Zsh prompt", lambda: PROMPT.encode() in stream(), seconds=30)
            pane = (
                run([*tmux, "display-message", "-p", "-t", "preview:", "#{pane_id}"], env)
                .decode()
                .strip()
            )
            assert pane.startswith("%") and pane[1:].isdigit(), pane
            paste_time = run([*tmux, "show-options", "-gv", "assume-paste-time"], env)
            assert int(paste_time) > 0, paste_time
            features = run([*tmux, "list-clients", "-F", "#{client_termfeatures}"], env)
            (client_features,) = features.splitlines()
            assert b"bpaste" in client_features.split(b","), features
            if mode == "sixel":
                assert b"sixel" in client_features.split(b","), features
                assert (
                    run([*tmux, "display-message", "-p", "#{sixel_support}"], env).strip() == b"1"
                )
            if display is None:
                wait_for_preview("a pre-paste screenshot", draws_preview, before_paste=True)
                trigger.touch(exist_ok=False)
            else:
                window_ids = until(
                    "the WezTerm window",
                    lambda: run(find_window, gui_env, check=False),
                )
                (window_id,) = window_ids.decode().split()
                assert not draws_preview(), "fixture colors before paste"
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
            wait_for_preview("the preview's colors in the WezTerm window", draws_preview)
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
                f"PASS: real WezTerm paste over SSH renders {mode} and pastes one path", flush=True
            )
    except Exception:
        if display is None:
            status = paste_status.read_text(errors="replace") if paste_status.is_file() else ""
            print(
                f"{mode} paste trigger pending: {trigger.is_file()}\n"
                f"{mode} WezTerm paste status: {status or 'no status recorded'}",
                file=sys.stderr,
            )
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
    """Use a private X server or the GitHub-hosted macOS desktop."""
    macos = sys.platform == "darwin"
    if macos and (
        os.environ.get("GITHUB_ACTIONS") != "true"
        or os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted"
    ):
        raise SystemExit("The native clipboard test requires a GitHub-hosted macOS runner")
    with tempfile.TemporaryDirectory(prefix="tn-gui-", dir="/tmp") as directory:
        root = Path(directory)
        image = root / "fixture.png"
        image.write_bytes(sample_png())
        if macos:
            assert Path(os.environ["CI_WEZTERM_PINNED_GUI"]).is_file(), "missing pinned macOS GUI"
            swift_source = Path(__file__).with_name("support") / "paste-preview-macos.swift"
            subprocess.run(
                [
                    "xcrun",
                    "swiftc",
                    "-O",
                    str(swift_source),
                    "-o",
                    str(root / "paste-preview-macos"),
                ],
                check=True,
                timeout=120,
            )
            for mode in ("blocks", "sixel"):
                exercise(root / mode, binary, assets, None, image, mode)
            return
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
    # Reap the GUI and SSH connection when the CI runner cancels this process.
    def terminate(signum: int, _frame: object) -> None:
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, terminate)
    main(Path(sys.argv[1]).resolve(strict=True), Path(sys.argv[2]).resolve(strict=True))
