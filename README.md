# termnav

![Tests](https://github.com/cgraf78/termnav/actions/workflows/test.yml/badge.svg?branch=main)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Bash Version](https://img.shields.io/badge/bash-%3E%3D3.2-blue.svg)](https://www.gnu.org/software/bash/)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20WSL-lightgrey.svg)](#)

`termnav` owns terminal navigation helpers: nearest-scope tab routing,
WezTerm link routing, tmux ctrl-click follow-through, OSC-8-aware `eza`
links, and `nvim-tmux-open`.

## Installation

For the simplest checkout-backed install:

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/termnav/main/install.sh | bash
```

This keeps a durable managed checkout under `$XDG_DATA_HOME` when that path is
absolute, or under `$HOME/.local/share` otherwise. To manage the checkout path
yourself, keep it at a stable path and run:

```bash
./install.sh
```

The installer creates checkout-backed symlinks for every public command under
`$HOME/.local/bin` and for each matching manual page under
`$HOME/.local/share/man/man1`. Set `PREFIX` to relocate both trees, or set
`BIN_DIR` and `MAN_DIR` independently. Re-running the installer is safe and
retargets existing symlinks, but it refuses to replace a non-symlink path.
Moving or deleting the checkout breaks the installed links.

Command and shell-loader path resolution keeps `lib/termnav/` and
`share/termnav/` version-coupled to this checkout. The installer creates no
second library, shared-asset, or completion tree. Continue to resolve those
non-binary assets through shdeps, or use their absolute paths in this checkout;
consumers must still select versioned VS Code payloads explicitly.

## Public API

- `bin/nvim-tmux-open`: open local or remote terminal targets in Neovim.
- `bin/nvim-link-host`: print the host token for `rg --hostname-bin`.
- `bin/nvim-ssh-control-open`: open a remote target through an existing
  SSH ControlMaster connection without starting a new authentication flow.
- `bin/wezterm-switch-tab`: request WezTerm tab switching, including
  parent-tmux bubbling for nested local or remote tmux sessions.
- `bin/wezterm-move-tab`: request WezTerm tab movement, including parent-tmux
  bubbling for nested local or remote tmux sessions.
- `bin/wezterm-select-pane`: request directional pane selection in a parent
  tmux through WezTerm's remote-safe terminal loopback.
- `bin/vscode-switch-tab`: request a VS Code integrated-terminal tab switch
  through the pluggable command-execution bridge. There is no matching
  move/reorder command because VS Code exposes no command to reorder
  terminal tabs (drag-only).
- `bin/vscode-nvim-focus`: publish ordered, leased Neovim focus ownership to
  the window-scoped VS Code adapter. Persistent tmux sessions resolve the
  currently focused client rather than trusting inherited window state.
- `bin/termnav-switch-tab`: switch the nearest outer tab scope from a
  one-window tmux session, walking locally nested tmux parents before choosing
  the originating VS Code or WezTerm client.
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
- `lib/termnav/nvim-open/launcher.sh`: sourceable policy for safely reusing an
  existing Neovim pane for a simple interactive-shell file open. Call
  `termnav_nvim_try_reuse "$@"`; a nonzero result means the caller must launch
  and wait for its real editor.
- `lib/termnav/nvim/vscode-focus.lua`: leased VS Code focus publisher used by
  the Neovim setup module.
- `lib/termnav/shell/wezterm-vars.sh`: shell helpers for emitting WezTerm
  `SetUserVar` OSC requests with raw or tmux-passthrough framing.
- `lib/termnav/shell/file-links.sh`: terminal and attached-tmux-client
  classification for choosing plain paths versus Termnav-routable OSC-8 links.
  `termnav_file_links_need_plain_output` returns success for plain output.
- `lib/termnav/shell/vscode-command.sh`: dispatch seam for executing a VS
  Code command by ID through a pluggable backend (`TERMNAV_VSCODE_BACKEND`,
  automatically `socket` when the adapter is advertised, otherwise `mcp`).
- `lib/termnav/shell/vscode-backend-socket.sh`: window-scoped backend that
  posts an authenticated direction to the local adapter's owner-only Unix
  socket.
- `lib/termnav/shell/vscode-backend-mcp.sh`: legacy backend for direct callers
  that use the `nabheet.vscode-ide-mcp` extension's local HTTP JSON-RPC API.
- `share/termnav/vscode/termnav-0.3.0`: local VS Code extension that
  owns and publishes each window's tab-switch socket and capability. New
  integrations use the latest versioned directory declared by the consuming
  dotfiles.
- `share/termnav/shell.sh`: sourceable interactive shell loader for WezTerm
  pane context publishing and file-link mode classification.

Source non-binary assets through shdeps so install locations stay under the
dependency manager's contract:

```bash
. "$(shdeps dep-file cgraf78/termnav share/termnav/shell.sh)"
```

`shdeps` installs the `bin/` entry points as PATH-visible symlinks. Consumers
own keybindings, terminal config, and environment-specific extension files;
this repo owns reusable route parsing and open-through behavior.

The adapter directory is versioned because VS Code records that directory in
its extension registry. Adapter releases must bump the manifest version, the
directory name, and the dotfiles local-extension source row together.

## Examples

[`examples/`](examples/) contains tested, copyable composition for WezTerm,
Neovim, tmux, and an XDG token-detector extension. The examples keep terminal
and editor policy in the consumer while loading Termnav-owned implementations
through Shdeps.

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
- VS Code with the local `cgraf.termnav` adapter installed, plus `curl` with
  Unix-socket support. The adapter publishes a private, per-window socket into
  its terminals; VS Code, VS Code Insiders, Cursor, and compatible builds use
  the same transport without product-specific CLI discovery.
- `nabheet.vscode-ide-mcp` is required only by direct legacy callers or when
  `TERMNAV_VSCODE_BACKEND=mcp` is selected. Its auth token follows the dotfiles
  `vscode.sh` contract under an absolute
  `$XDG_STATE_HOME/dot` or the `$HOME/.local/state/dot` fallback.
- `nvim-remote-pane-host` is an optional extension command for custom
  remote-pane workflows.

`tmux-follow-click` loads environment-specific token detectors from
an absolute `$XDG_CONFIG_HOME/termnav/tmux-follow/extensions.d/*.sh`, falling
back to `$HOME/.config/termnav/tmux-follow/extensions.d/*.sh` when the XDG value
is empty or relative. With neither base directory, it simply loads no optional
extensions. Set
`TERMNAV_TMUX_FOLLOW_EXTENSION_DIR` for tests or managed deployments that keep
those detectors somewhere else. Detector files call
`tmux_follow_register_token_detector <function>`; detectors claim a token by
setting `target` and `target_kind` and returning 0.

Set `TERMNAV_REMOTE_LINK_HOST` when a shell, tmux server, or managed remote
transport already knows the remote host identity that file links should carry.
`bin/eza-nvim-links` also accepts `TERMNAV_EZA_NVIM_LINKS_FORCE_TTY=1` for
test harnesses that need to exercise TTY-restoration behavior while stdout is
piped.

The `IS_NVIM`, `NVIM_*`, and `TERMNAV_TMUX` WezTerm user variables are
termnav's private cross-process protocol between shell/Neovim publishers and
WezTerm route consumers. Configure integrations through the modules above
instead of setting or reading those names directly.

The file-link classifier keeps semantic links for an identified WezTerm router,
uses plain paths in VS Code and otherwise unmarked WSL terminals, and inspects
the attached tmux client instead of trusting a pane's inherited environment.
Consumers with another native-linkification policy may export
`TERMNAV_FILE_LINKS_PLAIN=1`; an identified WezTerm router still wins over a
stale inherited marker. Only recognized classifier fields are retained from an
attached client's environment.
Set `NVIM_LAUNCHER_FORCE_NEW=1` when a managed invocation must bypass pane reuse.
`NVIM_LAUNCHER_ALLOW_NONTTY=1` and `NVIM_LAUNCHER_ALLOW_NONSHELL_PARENT=1` are
explicit trust overrides for controlled wrappers and test harnesses; ordinary
launchers should leave both unset.

The VS Code adapter exposes the equivalent `termnav.nvimFocused` context key.
Each activation publishes a random per-window token with its socket. Neovim
claims must authenticate with that token and include a bounded process ancestry
containing the active terminal PID. This binds claims to the active terminal
without requiring the extension host to inspect host procfs across container or
PID-namespace boundaries. Claims are ordered across focus cycles and renewed
with a short lease. Focus loss, terminal changes, terminal closure,
editor-window blur, adapter shutdown, and lease expiry all reset the key to
false. Undefined, partial, or stale state therefore keeps normal VS Code
commands active. In tmux, the publisher reads the socket and token from each
focused client process. A still-running Neovim in a hidden pane actively
releases its old claim and continues probing so pane return or reattach recovers
without restarting the editor.

### Tab Scope Routing

`termnav-switch-tab` receives the tmux client PID, TTY, and terminal type
from the key binding that observed the chord. When that client was launched
inside another local tmux, the helper follows its `TMUX` and `TMUX_PANE`
environment to the parent server. Only clients whose active pane is the exact
parent pane are eligible, which keeps linked sessions from stealing a request.
The most recently active eligible client wins; a unique focused client breaks
same-second ties, and unresolved ties fail closed. A parent session with
multiple windows owns the switch immediately, while a one-window parent
contributes its selected client and traversal continues outward.

At the terminal boundary, WezTerm keeps the OSC user-variable transport. Each
VS Code window's `cgraf.termnav` adapter owns an owner-only Unix socket and
publishes its path and random capability as `TERMNAV_VSCODE_SOCKET` and
`TERMNAV_VSCODE_TOKEN`. Each activation atomically claims a random path and
rotates the token without depending on telemetry, workspace identity, or
product-global CLI state. The router reads both values from the originating
tmux client, so simultaneous windows, including windows on the same workspace,
do not compete for a fixed port or process-global bridge. A partial socket/token
pair fails closed. After first install,
reload or restart the editor window so it discovers the adapter, then relaunch
existing terminal or tmux clients to receive the value. The same terminal
relaunch is required after an extension-host or editor-window restart.
Without it the router fails closed; an MCP route is used only when
`TERMNAV_VSCODE_FALLBACK_BACKEND=mcp` is explicitly set. Standalone
`vscode-switch-tab` calls retain their legacy MCP default.

When a remote nested tmux parent cannot be reached as a local server, the
existing WezTerm `DOT_PARENT_SWITCH_TAB` loopback remains the fallback. An
ordinary SSH or mosh hop cannot carry that output-side loopback into VS Code;
a one-window remote tmux therefore fails closed there unless the caller
provides a reverse bridge explicitly.

Neovim socket discovery state lives under
an absolute `$XDG_STATE_HOME/nvim-tmux-open`, falling back to
`$HOME/.local/state/nvim-tmux-open`. The opener's diagnostic log uses the same
state root at `wezterm-nvim-open.log`. Discovery records are published
atomically with owner-only permissions and are scoped to the tmux server,
pane, and Neovim process so concurrent editors cannot replace or remove one
another's registrations. Each scope's atomic `latest` record is a complete
ordering point, while a final versioned per-process commit record keeps partial
initial publications undiscoverable. Long encoded pane identities are split
across bounded path components without losing identity information.
When Neovim does not already expose a record-safe RPC address,
Termnav creates its socket below an absolute `$XDG_RUNTIME_DIR/termnav` or a
private, short `/tmp/termnav-UID` fallback so Unix socket path limits do not
depend on the state-directory length. As required by the XDG specification,
empty and relative values are ignored. Runtime paths containing newlines also
use the short fallback so line-oriented discovery records remain valid.
Without either state base directory,
filesystem discovery and publication are disabled rather than writing below
`/`. Set `XDG_STATE_HOME` to an absolute
path in the environment that starts WezTerm, tmux, and Neovim; setting it only
inside a child interactive shell cannot change an already-running GUI or tmux
server's environment.

Set `TERMNAV_SSH_CONTROL_HOSTS` to a comma-separated allowlist of host aliases
that `bin/nvim-ssh-control-open` may contact through an existing SSH
ControlMaster connection. The helper fails closed when the variable is unset
or the target host is not listed, so reusable installs do not inherit private
host policy from this repo.

`termnav-relay ssh` sends each relay path through OpenSSH's per-session
environment channel, including sessions carried by an existing ControlMaster.
It leaves SSH's remote command channel and login-shell selection untouched, so
non-POSIX targets such as native Windows OpenSSH hosts still receive an
ordinary interactive shell. Strict `ExitOnForwardFailure=yes` configurations
are delegated unchanged rather than making Termnav's optional relay mandatory.

Run tests with:

```bash
./test/termnav-test
```

CI also runs the WezTerm suite against Arch's current `eza` package so changes
to the optional `--hyperlink` argument cannot be hidden by the suite's
missing-tool skip on other platforms.

## License

MIT. See [`LICENSE`](LICENSE).
