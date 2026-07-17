# termnav

![Tests](https://github.com/cgraf78/termnav/actions/workflows/test.yml/badge.svg?branch=main)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Bash Version](https://img.shields.io/badge/bash-%3E%3D3.2-blue.svg)](https://www.gnu.org/software/bash/)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20WSL-lightgrey.svg)](#)

`termnav` owns terminal navigation helpers: WezTerm link routing, tmux
ctrl-click follow-through, OSC-8-aware `eza` links, and `nvim-tmux-open`.

## Public API

- `bin/nvim-tmux-open`: open local or remote terminal targets in Neovim.
- `bin/nvim-link-host`: print the host token for `rg --hostname-bin`.
- `bin/nvim-ssh-control-open`: open a remote target through an existing
  SSH ControlMaster connection without starting a new authentication flow.
- `bin/wezterm-switch-tab`: request WezTerm tab switching, including
  parent-tmux bubbling for nested local or remote tmux sessions.
- `bin/wezterm-move-tab`: request WezTerm tab movement, including parent-tmux
  bubbling for nested local or remote tmux sessions.
- `bin/vscode-switch-tab`: request a VS Code integrated-terminal tab switch
  through the pluggable command-execution bridge. There is no matching
  move/reorder command because VS Code exposes no command to reorder
  terminal tabs (drag-only).
- `bin/tmux-follow-click`: resolve tmux mouse clicks to URL/file actions.
- `bin/eza-nvim-links`: run `eza` with remote-aware file hyperlinks.
- `lib/termnav/wezterm/link-routes.lua`: WezTerm route handlers.
- `lib/termnav/wezterm/public-link-rules.lua`: public WezTerm link rules
  for terminal tokens such as localhost URLs, git SSH remotes, CVEs, and RFCs.
  The module returns fresh rule copies from `public_link_rules()` and appends
  them to an existing WezTerm `hyperlink_rules` table with
  `add_public_link_rules(rules)`.
- `lib/termnav/nvim/setup.lua`: reusable Neovim-side setup for publishing the
  current editor socket, cwd, and remote context to WezTerm.
- `lib/termnav/nvim/nvim-tmux-open.lua` and
  `lib/termnav/nvim/wezterm-vars.lua`: lower-level Neovim helpers used by the
  setup module and advanced consumers.
- `lib/termnav/shell/wezterm-vars.sh`: shell helpers for emitting WezTerm
  `SetUserVar` OSC requests with raw or tmux-passthrough framing.
- `lib/termnav/shell/vscode-command.sh`: dispatch seam for executing a VS
  Code command by ID through a pluggable backend (`TERMNAV_VSCODE_BACKEND`,
  default `mcp`).
- `lib/termnav/shell/vscode-backend-mcp.sh`: default backend, calling the
  `nabheet.vscode-ide-mcp` extension's local HTTP JSON-RPC API.
- `share/termnav/shell.sh`: sourceable interactive shell loader for WezTerm
  pane context publishing.

Source non-binary assets through shdeps so install locations stay under the
dependency manager's contract:

```bash
. "$(shdeps dep-file cgraf78/termnav share/termnav/shell.sh)"
```

`shdeps` installs the `bin/` entry points as PATH-visible symlinks. Consumers
own keybindings, terminal config, and environment-specific extension files;
this repo owns reusable route parsing and open-through behavior.

## Dependencies

- Bash for the CLI entry points and shell loader.
- `tmux` for `tmux-follow-click`, tmux pane capture, tmux mouse forwarding, and
  tmux-aware Neovim targeting.
- Neovim with a server/socket setup for `nvim-tmux-open` to open file targets
  in an existing editor session.
- WezTerm for the Lua link-route modules, OSC user-variable context
  publishing, and tab switch/move requests. The tmux and Neovim helpers remain
  useful without WezTerm when a consumer invokes them directly.
- `eza` for `eza-nvim-links`.
- `ssh` for `nvim-ssh-control-open` when remote file links should reuse an
  existing ControlMaster connection.
- `curl` for `vscode-switch-tab`'s default MCP backend.
- VS Code with the `nabheet.vscode-ide-mcp` extension installed and running,
  for the VS Code tab bridge. The auth token it reads is written by
  dotfiles' `vscode.sh` merge hook to
  `${XDG_STATE_HOME:-$HOME/.local/state}/dot/vscode-mcp-auth-token`; this
  repo only reads that file, never generates or manages the token.
- `nvim-remote-pane-host` is an optional extension command for custom
  remote-pane workflows.

`tmux-follow-click` loads environment-specific token detectors from
`${XDG_CONFIG_HOME:-~/.config}/termnav/tmux-follow/extensions.d/*.sh`. Set
`TERMNAV_TMUX_FOLLOW_EXTENSION_DIR` for tests or managed deployments that keep
those detectors somewhere else. Detector files call
`tmux_follow_register_token_detector <function>`; detectors claim a token by
setting `target` and `target_kind` and returning 0.

Set `TERMNAV_REMOTE_LINK_HOST` when a shell, tmux server, or managed remote
transport already knows the remote host identity that file links should carry.
`bin/eza-nvim-links` also accepts `TERMNAV_EZA_NVIM_LINKS_FORCE_TTY=1` for
test harnesses that need to exercise TTY-restoration behavior while stdout is
piped.

The `IS_NVIM` and `NVIM_*` WezTerm user variables are termnav's private
cross-process protocol between shell/Neovim publishers and WezTerm route
consumers. Configure integrations through the modules above instead of setting
or reading those names directly.

Set `TERMNAV_SSH_CONTROL_HOSTS` to a comma-separated allowlist of host aliases
that `bin/nvim-ssh-control-open` may contact through an existing SSH
ControlMaster connection. The helper fails closed when the variable is unset
or the target host is not listed, so reusable installs do not inherit private
host policy from this repo.

Run tests with:

```bash
./test/termnav-test
```

CI also runs the WezTerm suite against Arch's current `eza` package so changes
to the optional `--hyperlink` argument cannot be hidden by the suite's
missing-tool skip on other platforms.

## License

MIT. See [`LICENSE`](LICENSE).
