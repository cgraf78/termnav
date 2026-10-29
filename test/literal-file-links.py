"""Exercise OSC 8 filename provenance through both routers and a real editor."""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from socket import gethostname


def main() -> None:
    """Keep every editor, socket and persistence channel invocation-owned."""
    binary = str(Path(sys.argv[1]).resolve())
    # Minimal Linux images need not ship the hostname command. The native
    # router uses the system hostname too; keep this fixture dependency-free.
    hostname = gethostname().split(".", 1)[0]
    repo = Path(__file__).resolve().parent.parent
    nvim = os.environ.get("TERMNAV_TEST_NVIM_BINARY") or str(
        Path.home() / ".local/share/neovim/neovim/bin/nvim"
    )
    if not Path(nvim).is_file():
        nvim = shutil.which("nvim")
    if not nvim:
        print("SKIP: literal file links require Neovim")
        return
    socket_parent = "/tmp" if Path("/tmp").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="tn-literal-", dir=socket_parent) as root:
        root = Path(root).resolve()
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("TMUX", "NVIM", "SSH_", "WEZTERM_", "TERMNAV_", "XDG_"))
        }
        env.update(HOME=str(root), TMPDIR=str(root))
        env["PATH"] = str(Path(nvim).parent) + os.pathsep + env["PATH"]
        for name in ("CONFIG", "CACHE", "DATA", "STATE", "RUNTIME"):
            env[f"XDG_{name}_HOME" if name != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(root / name)
        socket = str(root / "n.sock")
        setup = root / "setup.lua"
        setup.write_text(
            "_G.nvim_tmux_open = dofile("
            + json.dumps(str(repo / "lib/termnav/nvim/nvim-tmux-open.lua"))
            + ").open\n"
        )

        def run(argv: list[str]) -> str:
            """Run one isolated, bounded helper and return its stdout."""
            return subprocess.run(
                argv,
                env=env,
                cwd=root,
                capture_output=True,
                text=True,
                check=True,
                timeout=10,
            ).stdout.strip()

        editor = subprocess.Popen(
            [
                nvim,
                "--headless",
                "-u",
                "NONE",
                "-i",
                "NONE",
                "--listen",
                socket,
                "-c",
                f"luafile {setup}",
            ],
            env=env,
            cwd=root,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        try:
            deadline = time.monotonic() + 5
            while not Path(socket).exists():
                assert editor.poll() is None, editor.stderr.read().decode()
                assert time.monotonic() < deadline, "editor socket timed out"
                time.sleep(0.01)
            registry = root / "STATE/nvim-tmux-open"
            registry.mkdir(parents=True)
            (registry / "current").write_text(socket + "\n")
            plain = root / "report"
            literal = root / "report:2026"
            plain.write_text("neighbor\n")
            literal.write_text("".join(f"line {i}\n" for i in range(30)))
            driver = root / "route.lua"
            driver.write_text("""
local request = vim.json.decode(vim.env.LINK_REQUEST)
local routes = dofile(request.module).new({
  hostname = function() return request.hostname end,
  background_child_process = function(argv)
    vim.fn.system(argv)
    assert(vim.v.shell_error == 0, 'native opener failed')
  end,
}, { remote_open_scope = 'fixture' })
local pane = {
  pane_id = function() return 17 end,
  get_user_vars = function() return { IS_NVIM = 'true', NVIM_OPEN_SOCKET = request.socket } end,
  get_current_working_dir = function() return { host = '', file_path = request.cwd } end,
}
assert(routes.open_uri(nil, pane, request.uri) == false, 'URI was not handled')
""")
            bin_dir = root / "bin"
            bin_dir.mkdir()
            (bin_dir / "termnav").symlink_to(binary)
            env["TERMNAV_BIN_DIR"] = str(bin_dir)

            def opened(uri: str, router: str, want: Path, line: int = 1, column: int = 1) -> None:
                """Verify a routed click changes the actual editor buffer and cursor."""
                # Local WezTerm requests must use the exact pane's socket,
                # rather than accidentally succeeding through global discovery.
                # Remote transport fixtures use the registry as their destination.
                global_socket = socket
                if router == "wezterm" and "://remote.example/" not in uri:
                    global_socket = str(root / "unavailable-global.sock")
                (registry / "current").write_text(global_socket + "\n")
                # Reset to a distinct buffer so a dropped click cannot pass.
                run(
                    [
                        nvim,
                        "--server",
                        socket,
                        "--remote-expr",
                        "execute('edit ' . fnameescape(" + json.dumps(str(root / "reset")) + "))",
                    ]
                )
                if router == "wezterm":
                    env["LINK_REQUEST"] = json.dumps(
                        dict(
                            module=str(repo / "lib/termnav/wezterm/link-routes.lua"),
                            socket=socket,
                            cwd=str(root),
                            uri=uri,
                            hostname=hostname,
                        )
                    )
                    env["LINK_DRIVER"] = str(driver)
                    run(
                        [
                            nvim,
                            "--headless",
                            "-u",
                            "NONE",
                            "-i",
                            "NONE",
                            "-c",
                            "lua local ok,e = pcall(dofile, vim.env.LINK_DRIVER); "
                            "if not ok then print(e); vim.cmd('cquit 1') end",
                            "-c",
                            "qa!",
                        ]
                    )
                else:
                    run(
                        [
                            binary,
                            "tmux",
                            "follow-click",
                            uri,
                            "",
                            "",
                            "0",
                            str(root),
                            "",
                            "%1",
                            "",
                            "",
                            "",
                            str(root / "tmux.sock"),
                        ]
                    )
                deadline = time.monotonic() + 3
                while True:
                    actual = run(
                        [
                            nvim,
                            "--server",
                            socket,
                            "--remote-expr",
                            "json_encode([expand('%:p'), line('.'), col('.')])",
                        ]
                    )
                    if json.loads(actual) == [str(want), line, column]:
                        return
                    assert time.monotonic() < deadline, (router, uri, actual)
                    time.sleep(0.01)

            for router in ("wezterm", "tmux"):
                opened(literal.as_uri().replace("%3A", ":"), router, literal)
                opened(literal.as_uri(), router, literal)
                opened(f"nvim-open://{literal}:12:3", router, literal, 12, 3)
                opened(f"nvim-remote://localhost{literal}:12:3", router, literal, 12, 3)
                opened(f"nvim-remote://{hostname}{literal}:12:3", router, literal, 12, 3)
                # The file URI keeps its literal suffix even after disappearance.
                opened((root / "missing:2026").as_uri(), router, root / "missing:2026")

            # Minimal distributions need not install hostname(1). Keep only the
            # actual opener/editor in PATH so system-name routing cannot pass
            # accidentally through a command supplied by the developer's host.
            (bin_dir / "nvim").symlink_to(nvim)
            original_path = env["PATH"]
            env["PATH"] = str(bin_dir)
            assert shutil.which("hostname", path=env["PATH"]) is None
            for name in dict.fromkeys((gethostname(), hostname)):
                for router in ("wezterm", "tmux"):
                    opened(f"nvim-remote://{name}{literal}:12:3", router, literal, 12, 3)
            env["PATH"] = original_path

            # The OS fast path must retain administrator-resolved and explicitly
            # published host aliases, which need not match the kernel's name.
            hostname_command = bin_dir / "hostname"
            hostname_command.write_text("""#!/bin/sh
test "$1" = -f || exit 1
printf '%s\\n' termnav-resolved-name.invalid
""")
            hostname_command.chmod(0o700)
            env["PATH"] = str(bin_dir) + os.pathsep + original_path
            opened(
                f"nvim-remote://termnav-resolved-name.invalid{literal}:12:3", "tmux", literal, 12, 3
            )
            hostname_command.unlink()
            env["TERMNAV_REMOTE_LINK_HOST"] = "termnav-published-name.invalid"
            opened(
                f"nvim-remote://termnav-published-name.invalid{literal}:12:3",
                "tmux",
                literal,
                12,
                3,
            )
            env.pop("TERMNAV_REMOTE_LINK_HOST")
            env["PATH"] = original_path

            # An approved-master fixture executes the actual remote shell
            # request locally. This exercises the older location-only remote API
            # without opening a network connection or requiring a second editor.
            ssh = bin_dir / "ssh"
            ssh.write_text("""#!/bin/sh
if test "$1" = -G; then
  printf 'controlpath /tmp/termnav-fixture-master\\n'
  exit 0
fi
for remote_command do :; done
exec /bin/sh -c "$remote_command"
""")
            ssh.chmod(0o700)
            env["PATH"] = str(bin_dir) + os.pathsep + env["PATH"]
            env["TERMNAV_SSH_CONTROL_HOSTS"] = "remote.example"
            for router in ("wezterm", "tmux"):
                opened(f"file://remote.example{literal}", router, literal)
                opened(f"file://remote.example{str(literal).replace(':', '%3A')}", router, literal)
                opened(f"nvim-remote://remote.example{literal}:12:3", router, literal, 12, 3)

            # Exact-pane helpers retain their five-argument contract. The final
            # argument is canonical diagnostic syntax understood by old helpers.
            helper = bin_dir / "transport"
            helper.write_text("""#!/bin/sh
printf '%s\\n' "$@" > "$HELPER_LOG"
exec "$TEST_TERMNAV" nvim open link "$5" "$TEST_CWD" nvim "$TEST_SOCKET"
""")
            helper.chmod(0o700)
            env.update(
                TERMNAV_REMOTE_OPEN_HELPER=str(helper),
                TERMNAV_SSH_CONTROL_HOSTS="",
                HELPER_LOG=str(root / "helper.log"),
                TEST_TERMNAV=binary,
                TEST_CWD=str(root),
                TEST_SOCKET=socket,
            )
            for router in ("wezterm", "tmux"):
                opened(f"file://remote.example{literal}", router, literal)
                fields = (root / "helper.log").read_text().splitlines()
                assert fields == [
                    router,
                    "fixture" if router == "wezterm" else str(root / "tmux.sock"),
                    "17" if router == "wezterm" else "%1",
                    "remote.example",
                    f"{literal}:1:0",
                ], fields

            eza = shutil.which("eza")
            if eza:
                output = run([binary, "eza", "--oneline", str(literal)])
                uri = output.split("\x1b]8;;", 1)[1].split("\x1b\\", 1)[0]
                for router in ("wezterm", "tmux"):
                    opened(uri, router, literal)
            else:
                print("SKIP: actual eza producer (eza unavailable); literal URI cases passed")
            print(
                "literal file links: both routers opened exact filenames and diagnostic positions"
            )
        finally:
            editor.terminate()
            try:
                editor.wait(timeout=3)
            except subprocess.TimeoutExpired:
                editor.kill()
                editor.wait(timeout=3)


if __name__ == "__main__":
    main()
