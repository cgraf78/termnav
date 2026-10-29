# termnav

[![Tests](https://github.com/cgraf78/termnav/actions/workflows/test.yml/badge.svg?branch=main)](https://github.com/cgraf78/termnav/actions/workflows/test.yml?query=branch%3Amain)
[![Latest release](https://img.shields.io/github/release-date/cgraf78/termnav?display_date=published_at&label=latest%20release)](https://github.com/cgraf78/termnav/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-67ac09)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-%3E%3D1.88-blue)](#dependencies)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20WSL%20%7C%20Termux-lightgrey)](#dependencies)

**One set of keys for moving around a layered terminal.** `termnav` takes the
vim-tmux-navigator idea further out: past nested tmux, across SSH, and into
the tabs of the terminal that hosts it all.

- **Panes:** from Neovim, `Ctrl-h/j/k/l` moves between splits and, at the edge
  of the editor, on into the neighboring tmux pane. That includes tmux nested
  inside tmux, on the same host or over SSH through the `termnav ssh` relay
  (see [over SSH](#over-ssh) for what the remote host needs).
  `Alt-H/J/K/L` swaps the current pane with its neighbor; it stays inside the
  local Neovim and tmux layers and never crosses SSH, WezTerm, or VS Code.
  The shipped examples bind `Ctrl-h/j/k/l` in Neovim and tmux, and
  `Alt-H/J/K/L` in Neovim only; see
  [what the examples bind](#what-the-examples-bind-and-what-they-dont).
- **Tabs:** once `Ctrl-Tab` reaches tmux, it moves to the next window of a
  tmux layer that has several. In a one-window layer it continues outward to
  a parent tmux, a WezTerm tab, or (with the [VS Code adapter](#vs-code-advanced))
  a VS Code terminal tab. Getting the key to tmux takes terminal and tmux
  settings the examples don't ship; see
  [what the examples bind](#what-the-examples-bind-and-what-they-dont).
- **Links:** in tmux, Ctrl-click a file path to open it in a Neovim you
  already have running, or a URL to have WezTerm open it on your local
  machine, even from inside SSH. Remote file opens reuse an existing SSH
  ControlMaster connection for hosts you allow.

It is one Rust binary plus small Lua and shell adapters. There is no daemon:
keypresses run short-lived commands, nested tmux clients run one small focus
watcher each, and nothing runs during tmux redraws.

## Installation

Install the latest checksum-verified release archive with:

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/termnav/main/install.sh | bash
```

The installer selects the platform archive, verifies its SHA-256 checksum and
embedded build identity, activates the complete release atomically, and links
`termnav` into the user command directory. When GitHub CLI 2.49+ is logged in
to github.com, it also verifies the archive's GitHub artifact attestation
(signed by `cgraf78/actions`) and refuses an archive without a valid one;
otherwise it proceeds on the checksum alone and says so. Pass
`--require-attestation` to make provenance verification mandatory. Local
`--archive` installs are checksum-only; combining `--archive` with
`--require-attestation` is an error. There is one executable and one
installation path to build, publish, update, and diagnose.

For manual archive verification, follow the shared
[attestation verification guide](https://github.com/cgraf78/actions/blob/main/docs/workflow-api.md#verifying-published-attestations).
The release source is `cgraf78/termnav`; its signer is the reusable release
workflow in `cgraf78/actions`.

## Quickstart

The [`examples/`](examples/) directory holds tested configuration snippets,
and the release installs them too. Copy them into your own configs:

1. **tmux.** Copy the example and source it from your `tmux.conf`. Besides
   its bindings and focus hooks, it turns on `mouse`, `focus-events`,
   `allow-passthrough`, and hyperlinks, and sets `window-style`,
   `window-active-style`, and `@termnav_inactive_style`; edit those to taste:

   ```bash
   mkdir -p ~/.config/tmux
   cp "$(termnav asset-path examples/tmux.conf)" ~/.config/tmux/termnav.conf
   ```

   ```tmux
   source-file ~/.config/tmux/termnav.conf
   ```

2. **Neovim.** Copy the loader into your config and call it from `init.lua`:

   ```bash
   mkdir -p ~/.config/nvim/lua
   cp "$(termnav asset-path examples/nvim.lua)" ~/.config/nvim/lua/termnav.lua
   ```

   ```lua
   require("termnav").setup()
   ```

3. **WezTerm.** Copy the route module and apply it in `.wezterm.lua`:

   ```bash
   mkdir -p ~/.config/wezterm
   cp "$(termnav asset-path examples/wezterm.lua)" ~/.config/wezterm/termnav.lua
   ```

   ```lua
   local wezterm = require("wezterm")
   local config = wezterm.config_builder()
   local example = dofile(wezterm.home_dir .. "/.config/wezterm/termnav.lua")
   example.apply({ wezterm = wezterm, config = config })
   return config
   ```

### Over SSH

For navigation through nested tmux over SSH, load the shell integration in
your interactive shell. Its `ssh()` function runs `termnav ssh` for you:

```sh
. "$(termnav asset-path share/termnav/shell.sh)"
```

`termnav ssh` forwards a Unix socket with `RemoteForward` and names it to the
remote side with `SendEnv=TERMNAV_PARENT_RELAY`. Each remote host therefore
needs:

- termnav installed, plus the tmux and Neovim setup above;
- `AcceptEnv TERMNAV_PARENT_RELAY` in its sshd configuration. termnav ships
  that line as a fragment (`termnav asset-path share/termnav/sshd_config`
  prints its path); install it as, for example,
  `/etc/ssh/sshd_config.d/termnav.conf` and reload sshd;
- Unix-socket forwarding allowed (`AllowStreamLocalForwarding`, which is on
  by default).

### What the examples bind, and what they don't

- **Neovim** (`examples/nvim.lua`): `Ctrl-h/j/k/l` selects panes and
  `Alt-H/J/K/L` moves them, in normal and terminal modes; `Ctrl-\` returns
  to the previous pane in normal mode; `Ctrl-Tab`/`Ctrl-Shift-Tab` and
  `Alt-{`/`Alt-}` select and move tabs in normal, insert, and terminal modes.
- **tmux** (`examples/tmux.conf`): `Ctrl-h/j/k/l` pane selection,
  `Ctrl-Tab`/`Ctrl-Shift-Tab` tab routing, Ctrl-click link following, and the
  focus hooks that tell nested tmux layers which pane really has focus.
- **WezTerm** (`examples/wezterm.lua`): termnav's link rules and the handler
  that acts on tab and URL requests. It assigns no keys.

`Ctrl-Tab` needs your own configuration in three places before the routing
above works:

- **Terminal:** WezTerm's default key assignments use `Ctrl-Tab` and
  `Ctrl-Shift-Tab` to switch its own tabs, so the keys never reach tmux or
  Neovim. Your WezTerm config has to pass them to the pane instead, at least
  when the pane runs tmux or Neovim.
- **tmux:** tmux generally needs extended keys to tell `C-Tab` from `Tab`:
  `extended-keys on` plus the `extkeys` terminal feature for your terminal
  (and for `tmux*` when tmux is nested).
- **Neovim inside tmux:** the example's `C-Tab` binding takes the key in
  every pane, so Neovim's own `Ctrl-Tab` mapping only fires outside tmux
  unless your tmux binding forwards the key to Neovim panes.

The tmux example binds `Ctrl-h/j/k/l` but not `Alt-H/J/K/L`. A
`Ctrl-h/j/k/l` key goes to the pane when its foreground program owns it: Vim,
Neovim, and fzf always, and SSH (including `termnav ssh`, which Termnav's
shell integration runs for plain `ssh`), mosh, ET, or a nested tmux or screen
while it has mouse reporting on. Otherwise tmux selects its neighboring pane,
and at its edge `termnav navigate pane-select DIRECTION --parent` continues
outward. The example publishes that ownership test as
`@termnav_pane_owns_chord`. It sees only the pane's foreground process, so
extend it if you want the keys to reach an editor started by a parent that
doesn't `exec` it, such as `git commit` opening Vim, or fzf run by a shell key
binding.

Like vim-tmux-navigator's bindings, these take the keys from shell panes and
any other program the test doesn't recognize, so readline's `Ctrl-l` (clear
screen), `Ctrl-k` (kill line), and `Ctrl-j` (accept line) no longer reach the
shell. `Ctrl-h` is also backward-delete, and on terminals whose Backspace key
sends `^H` it collides with Backspace: tmux reads the byte as Backspace when
the terminal's erase character is `^H`, so left selection never fires, and as
`Ctrl-h` otherwise, so Backspace selects the left pane.

For `Alt-H/J/K/L` from shell panes, bind them in your own tmux config to
`termnav navigate pane-move DIRECTION`, passing the keys through to Neovim
panes the same way. The command exits with status `3` when it declines a
request at the current boundary; append the example's
`$navigate_declined_ok` to treat that as a no-op. See
[`examples/README.md`](examples/README.md) and the
[reference](docs/reference.md#command-interface).

## Commands

| Command | Purpose |
|---|---|
| `termnav navigate ACTION DIRECTION` | Route pane selection, pane movement, tab selection, and tab movement |
| `termnav ssh SSH-ARG...` | Run an interactive SSH session with a connection-scoped reverse relay |
| `termnav relay send ACTION DIRECTION` | Send one navigation request over the inherited relay |
| `termnav tmux context\|focus\|follow-click ...` | Publish tmux context, maintain nested focus leases, and follow Ctrl-clicks |
| `termnav link-host` | Print the host of the current terminal context, for hyperlink producers such as ripgrep and eza |
| `termnav open-url URL` | Ask WezTerm to open an `http`, `https`, or `mailto` URL on the machine it runs on |
| `termnav nvim open MODE ...` | Open a target in the narrowest eligible existing Neovim |
| `termnav vscode focus ...` | Publish Neovim focus to the VS Code adapter |
| `termnav eza ...` | Run eza with remote-aware OSC-8 file links compatible with stable WezTerm |
| `termnav asset-path RELATIVE_PATH` | Print the installed path of a Lua, shell, or example asset |
| `termnav version` | Print the timestamp/commit build identity |

Exit status `0` means success, `1` an operational failure, `2` invalid syntax,
and `3` a request declined at the current boundary. Command-specific codes,
protocols, environment variables, and integration contracts are in
[`docs/reference.md`](docs/reference.md); complete syntax is in
`man termnav`.

File-link routing preserves genuine `a/` and `b/` directories and colons in
filenames. WezTerm regex rules should build replacements with
`routes.raw_link_uri("$1")` so literal percent escapes are not decoded as URI
syntax. Encoded `nvim-open://`, `file://`, and `lazygit-edit://` links retain
their existing contract. Eza links encode literal semicolons for stable
WezTerm, without changing the displayed filename.

The WezTerm adapter acknowledges file-open failures asynchronously and shows a
concise notification. Remote opens reuse only allowlisted existing SSH masters
(`TERMNAV_SSH_CONTROL_HOSTS`) or an explicitly configured
`TERMNAV_REMOTE_OPEN_HELPER` with exact client scope and pane identity. It never
starts another authenticated connection or guesses among same-host panes.

## Dependencies

- The prebuilt `termnav` binary for Linux, macOS, or Android aarch64. Android
  x86_64 is not published because the standalone installer does not support it.
  Building a Git checkout requires Rust 1.88 or newer plus Bash. Source archive
  builds must provide `TERMNAV_BUILD_COMMIT`, because build identity cannot be
  recovered without Git metadata. The crate is not published to crates.io.
- Bash only for sourceable shell integration and optional click-detector
  extensions; navigation gestures do not start a shell or Python interpreter.
- `tmux` for `termnav tmux follow-click`, pane capture, mouse forwarding, and
  tmux-aware Neovim targeting.
- Neovim with a server/socket setup for `termnav nvim open` to open file targets
  in an existing editor session.
- WezTerm for the Lua link-route modules, OSC user-variable context
  publishing, and tab switch/move requests. The tmux and Neovim helpers remain
  useful without WezTerm when a consumer invokes them directly.
- `eza` for `termnav eza`.
- `ssh` for `termnav nvim ssh-open` when remote file links should reuse an
  existing ControlMaster connection.
- VS Code only for VS Code terminal tabs; see [VS Code (advanced)](#vs-code-advanced).

## VS Code (advanced)

The VS Code adapter (`cgraf.termnav`, in
[`share/termnav/vscode/`](share/termnav/vscode/)) ships with termnav but is not
published on the Marketplace, and `examples/` ships no VS Code keybindings.
Using it means loading the local extension yourself and writing your own
keybindings around its `termnav.nvimFocused` context. The adapter publishes a
private, per-window socket into its terminals; VS Code, VS Code Insiders,
Cursor, and compatible builds use the same transport. See the
[VS Code focus protocol](docs/reference.md#vs-code-focus-protocol) and the
[MCP fallback](docs/reference.md#vs-code-mcp-fallback).

## Development

Run CI's main test command with:

```bash
cargo test --locked && ./test/termnav-test
```

Contributor and agent notes, including the other required CI jobs, are in
[`AGENTS.md`](AGENTS.md). Its [release section](AGENTS.md#release) also covers
publication and artifact verification.

## License

MIT. See [`LICENSE`](LICENSE).
