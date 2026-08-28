# termnav

![Tests](https://github.com/cgraf78/termnav/actions/workflows/test.yml/badge.svg?branch=main)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Bash Version](https://img.shields.io/badge/bash-%3E%3D3.2-blue.svg)](https://www.gnu.org/software/bash/)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20WSL-lightgrey.svg)](#)

`termnav` owns terminal navigation helpers: nearest-scope tab routing,
WezTerm link routing, tmux ctrl-click follow-through, OSC-8-aware `eza`
links, and existing-session Neovim opens.

## Installation

Install the latest checksum-verified release archive with:

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/termnav/main/install.sh | bash
```

The installer selects the platform archive, verifies its SHA-256 checksum and
embedded build identity, activates the complete release atomically, and links
`termnav` into the user command directory. There is one executable and one
installation path to build, publish, update, and diagnose.

## Command interface

`termnav` is the only compiled executable. Its stable command groups are:

- `termnav navigate ACTION DIRECTION`: route pane selection, tab selection,
  and tab movement through local tmux ancestry, SSH relays, and the originating
  terminal.
- `termnav ssh SSH-ARG...`: supervise the user's one requested SSH session and
  its connection-scoped reverse relay. It never opens a second authenticated
  transport, including during cleanup or remote editor reuse.
- `termnav relay send ACTION DIRECTION`: send one navigation request to the
  inherited relay using the same `pane-select`, `tab-select`, or `tab-move`
  action names as `termnav navigate`. `serve`, `commit`, and `sweep` are
  documented integration commands used by the SSH and tmux adapters rather
  than interactive entry points.
- `termnav tmux context ...`: publish tmux ownership to one exact terminal
  client; `termnav tmux focus ...` maintains hierarchical focus leases; and
  `termnav tmux follow-click ...` resolves mouse metadata to a browser or
  editor target.
- `termnav link-host`: print the host represented by the current terminal
  context for hyperlink producers such as ripgrep and eza.
- `termnav nvim open MODE ...`: open a target in the narrowest eligible editor
  scope. `ssh-open` is the fail-closed existing-ControlMaster transport.
- `termnav vscode focus ...`: publish ordered, authenticated Neovim focus
  ownership to the VS Code window displaying the exact terminal client.
- `termnav eza ...`: run eza with remote-aware OSC-8 file links.
- `termnav version`: print the timestamp/commit build identity.
- `termnav asset-path RELATIVE_PATH`: print the canonical path of an existing
  provider asset. Paths must stay below the installed Termnav root.

Complete leaf syntax is available from command-group help and the manpage.
Exit status `0` means handled/success, `1` means operational failure, and `2`
means invalid syntax. Navigation and relay-send use `3` for a valid request
declined at the current boundary. `nvim ssh-open` additionally uses `10` for
unavailable connection configuration, `11` for an invalid host, `12` for SSH
failure, and `13` for a disallowed host. `vscode focus` uses `10` when no
adapter is available. Passthrough commands otherwise preserve child status.

Ripgrep's `--hostname-bin` accepts an executable name but cannot pass arguments.
A consumer can provide a tiny `ripgrep-link-host` wrapper that delegates to the
explicit `termnav link-host` command; Termnav therefore needs no alternate
executable name or argv[0] dispatch. The private
`share/termnav/shims/ssh` adapter is required for PATH interception and contains
only enough policy to execute `termnav ssh`. It passes its exact runtime
directory to the native resolver because checkout and installed shim copies can
coexist during an update; recursive shim entry is rejected before another
process can be created. During the bounded stage where the shim arrives before
the binary, it uses a trusted platform OpenSSH path rather than the inherited
shim-bearing PATH so repository synchronization can finish safely. Historical
public command names are not installed.

There is intentionally no source-checkout installer. Shdeps consumers use the
ordinary `cgraf78/termnav github` dependency form, which prefers a published
release archive and falls back according to Shdeps' generic provider policy.

## Code organization

The CLI adapters under `src/commands/` validate syntax and translate exit
status. Reusable behavior lives behind focused library interfaces:

- `navigation` owns typed scope traversal and routing decisions;
- `relay` owns the versioned Unix-socket protocol and transactional directive
  store;
- `ssh` owns exactly one SSH child and its reverse-forward lifecycle;
- `focus` owns one-hop tmux leases and pane-style restoration;
- `nvim` owns target parsing, registry selection, RPC, exact-pane transport
  fallback, and mux-only remote reuse;
- `click` owns mouse-text recognition and returns typed URL/file targets;
- `assets` owns installed provider-root discovery;
- `terminal`, `process`, `runtime`, and `links` isolate operating-system and
  terminal-protocol boundaries shared by those domains.

Public Rust items use rustdoc to describe ownership, security, persistence, and
performance contracts. Non-obvious implementation comments explain why a
boundary exists; command adapters intentionally contain little policy.

The SSH relay is a newline-delimited JSON v2 protocol on an owner-only Unix
socket. Requests carry `v: 2`, an `op` from `navigate`, `prepare-path`,
`abort-path`, `commit-path`, or `focus`, and operation-specific fields. Replies
carry `v: 2` and a bounded `result` vocabulary (`armed`, `emitted`, `declined`,
`claimed`, `released`, or `error`). Rust vocabulary lives in
`src/relay/protocol.rs`; the frozen Python peer in tests is the cross-version
interoperability oracle. Consumers should invoke `termnav relay` rather than
constructing protocol objects directly.

## Integration assets

- `lib/termnav/wezterm/link-routes.lua`: WezTerm route handlers. Navigation
  consumers use one pane-owner snapshot per gesture and avoid foreground
  process inspection when current Neovim or tmux metadata is already present.
- `lib/termnav/wezterm/public-link-rules.lua`: public WezTerm link rules
  for terminal tokens such as localhost URLs, git SSH remotes, CVEs, and RFCs.
  The module returns fresh rule copies from `public_link_rules()` and appends
  them to an existing WezTerm `hyperlink_rules` table with
  `add_public_link_rules(rules)`.
- `lib/termnav/nvim/setup.lua`: reusable Neovim-side setup for publishing the
  current editor socket, cwd, and remote context to WezTerm.
- `lib/termnav/nvim/navigation.lua`: Neovim pane and tab navigation. Directional
  pane edges and tab boundaries use bounded one-shot native jobs, while
  Ctrl-backslash preserves the local
  previous-split/previous-tmux-pane history expected from vim-tmux-navigator
  without guessing at an ancestor's history.
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
- `share/termnav/vscode/termnav-0.3.0`: local VS Code extension that
  owns and publishes each window's tab-switch socket and capability. New
  integrations use the latest versioned directory declared by their consuming
  configuration.
- `share/termnav/shell.sh`: sourceable interactive shell loader for inherited
  SSH relay interposition, WezTerm pane context publishing, and file-link mode
  classification. It prepends a private Termnav shim directory rather than
  defining an `ssh()` shell function, so child processes use the same route.

Discover non-binary assets through the standalone command so consumers do not
depend on a particular installer or activation root:

```bash
. "$(termnav asset-path share/termnav/shell.sh)"
```

Consumers own keybindings, terminal config, and environment-specific extension
files; this repo owns reusable route parsing and open-through behavior.

The adapter directory is versioned because VS Code records that directory in
its extension registry. Adapter releases must bump the manifest version, the
directory name, and the consuming local-extension configuration together.

## Examples

[`examples/`](examples/) contains tested, copyable composition for WezTerm,
Neovim, tmux, and an XDG token-detector extension. The examples keep terminal
and editor policy in the consumer while loading Termnav-owned implementations
through `termnav asset-path`.

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
- VS Code with the local `cgraf.termnav` adapter installed. The adapter
  publishes a private, per-window socket into
  its terminals; VS Code, VS Code Insiders, Cursor, and compatible builds use
  the same transport without product-specific CLI discovery.
- `nabheet.vscode-ide-mcp` is required only when
  `TERMNAV_VSCODE_FALLBACK_BACKEND=mcp` selects the devserver fallback. Its
  deployment must set the absolute `TERMNAV_VSCODE_MCP_TOKEN_FILE`. Termnav
  defaults to `127.0.0.1:9876/mcp`, `tools/call`, `execute_command`, and VS
  Code's terminal previous/next command IDs. Consumers may override those with
  `TERMNAV_VSCODE_MCP_ADDRESS`, `TERMNAV_VSCODE_MCP_PATH`,
  `TERMNAV_VSCODE_MCP_METHOD`, `TERMNAV_VSCODE_MCP_TOOL`,
  `TERMNAV_VSCODE_MCP_PREVIOUS_COMMAND`, and
  `TERMNAV_VSCODE_MCP_NEXT_COMMAND`; non-loopback addresses are rejected.
- `nvim-remote-pane-host` is an optional extension command for custom
  remote-pane workflows.

`termnav tmux follow-click` loads environment-specific token detectors from
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
`termnav eza` also accepts `TERMNAV_EZA_NVIM_LINKS_FORCE_TTY=1` for
test harnesses that need to exercise TTY-restoration behavior while stdout is
piped.

The WezTerm protocol consists of `IS_NVIM`, `NVIM_OPEN_SOCKET`,
`NVIM_LINK_CWD`, `NVIM_REMOTE_LINK_HOST`, `NVIM_REMOTE_CWD`,
`NVIM_REMOTE_TMUX`, and `TERMNAV_TMUX` pane metadata plus the
`TERMNAV_TAB_SELECT`, `TERMNAV_TAB_MOVE`, and `TERMNAV_OPEN_URL` requests.
`routes.setup()` owns the request handlers; consumers should use route helpers
instead of interpreting metadata directly. There are no `DOT_*` aliases.

`lib/termnav/nvim/setup.lua` accepts `group_name`, `opener`, `navigation`,
`wezterm_vars`, `vscode_focus`, `publish_delay_ms`, `publish_events`,
`refresh_events`, and `clear_events`. `navigation.lua` accepts `application`,
`command`, `executable`, `mappings`, `notify`, `schedule`, and `spawn`.
`vscode-focus.lua` accepts `command`, `interval_ms`, `observed`, and `source`.
Defaults are production implementations; collaborator overrides exist for
embedding and deterministic tests.

The stable shell callbacks from `share/termnav/shell.sh` are
`_termnav_wezterm_publish_link_context`, `_termnav_wezterm_preexec COMMAND`,
`_termnav_wezterm_precmd`, and `termnav_file_links_need_plain_output`. The first
three are hook callbacks despite their leading underscore; callers must not use
other `_termnav_*` functions.

Remote typed-command fallbacks append the generic user command directories
`$HOME/.local/bin`, `$HOME/.local/share/mise/shims`, `/opt/homebrew/bin`, and
`/usr/local/bin` after inherited PATH. Set `TERMNAV_REMOTE_TOOL_PATH` in the
environment that launches Termnav to replace that append-only list. Termnav
embeds the selected paths in the existing ControlMaster command, rebasing paths
below the local home onto remote `$HOME`; it does not require sshd `AcceptEnv`
or open another authenticated transport.

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

`termnav tmux context` is intended for tmux's `client-attached` hook.
WezTerm user variables belong to a terminal pane rather than the long-lived
tmux pane, so attaching a new client to an existing session does not inherit
the shell or Neovim metadata previously emitted there. The attach publisher
writes `TERMNAV_TMUX=true` synchronously to the exact client tty. Direct
terminal clients receive raw OSC; an immediate tmux parent receives one
passthrough wrapper, including when it advertises legacy `screen-*` terminfo.
Control-mode clients are skipped without requiring terminal metadata.

### Nested tmux leaf focus

`termnav tmux focus` lets a tmux configuration distinguish the single focused
leaf pane from active container panes higher in a nested tmux tree. A focused
nested client publishes a short lease on the exact parent pane that hosts it.
Local nesting is resolved from that client's process environment; nesting over
SSH uses the same per-session relay established by `termnav ssh`.

The publisher is intentionally one-hop. Every tmux layer runs the same hooks,
so a chain of any depth converges without a root coordinator or topology
configuration. The hooks also reconcile the active pane when a client gains or
loses terminal focus, preventing an unfocused nested tmux from retaining its
own active background. Separate clients attached to the same inner session
publish to their own parent panes independently. A killed publisher, broken
SSH session, or missed detach event fails closed when its lease expires.

The rendering path remains entirely inside tmux. Border and label formats can
combine `pane_active`, the current client's `focused` flag, and the absence of
the pane-local `@termnav_child_focus` option. Pane content styles are different:
tmux compiles them without client context and does not reevaluate them when a
user option changes. A configuration that gives the active pane a different
background should therefore keep `window-active-style` literal and publish its
inactive/container style through `@termnav_inactive_style`.
Termnav applies that value as a pane-local active-style override while either a
focused child owns the pane or no terminal client focuses it. It restores any
preexisting override once the pane is again the focused leaf. No subprocess
runs during a redraw, and the uncommon selection repair hook is skipped unless
a stale unfocused marker is present. See `examples/tmux.conf` for the complete
pattern.

As with every full-screen application in a tmux pane, the pane grid itself is
shared if the exact same outer pane is viewed by multiple outer clients. tmux
can render its own borders per client, but the nested application's already
rendered cell backgrounds cannot differ between those viewers. Distinct outer
panes attaching distinct clients to one inner session remain independent.

The native `termnav relay` path passes the exact tmux client,
creation stamp, and source scope into the shared native navigation policy.
Application-origin requests cannot carry that
identity, so one client snapshot resolves both the logical tmux scope and any
safe physical provenance. Local pane operations need no client, and local tab
operations need only a session shared by the clients viewing that pane. A sole
eligible client, unique focused client, or unique fresh client is retained when
available so a buffered chord sequence keeps its original ancestry. Physical
ambiguity fails closed only when the action must leave the local tmux scope.
Every selected client is revalidated before traversal and again before terminal
or relay dispatch, so detach, recreation, pane-switch, and session-switch races
do not redirect a delayed chord. Linked windows use the selected client's
session instead of whichever session happens to appear first in a server-wide
query.

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
`TERMNAV_VSCODE_FALLBACK_BACKEND=mcp` is explicitly set.

There is no terminal-specific parent-tmux fallback. Local parents are followed
through process ancestry and remote parents through the SSH relay; if neither
route can identify the next scope, navigation is consumed without guessing.

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
that `termnav nvim ssh-open` may contact through an existing SSH
ControlMaster connection. The helper fails closed when the variable is unset
or the target host is not listed, so reusable installs do not inherit private
host policy from this repo.

Non-OpenSSH transports may set `TERMNAV_REMOTE_OPEN_HELPER` to an executable
that accepts `KIND SCOPE PANE HOST TARGET` and exits zero only after accepting
the request. `KIND=tmux` carries the exact tmux `#{socket_path}` and `%pane` in
`SCOPE` and `PANE`; `KIND=wezterm` carries a callback-local WezTerm mux scope
and pane ID. Pane IDs can repeat across GUI classes and mux servers, so a
missing WezTerm scope fails closed. Supply the opaque scope as
`remote_open_scope` to `link-routes.lua`'s `new()` function, or in the WezTerm
process environment as `TERMNAV_WEZTERM_SCOPE` or `WEZTERM_UNIX_SOCKET`.
Never derive it from pane shell metadata, which may be shared by several
attached terminal clients. Termnav invokes this capability only after
existing-ControlMaster reuse is definitively unavailable or declines the
request, gives it a bounded lifetime, and otherwise fails closed. An
indeterminate timed-out SSH request is never retried through a second
transport. Termnav never guesses among same-host panes or types tmux prefix
keys into a terminal stream.
Install the new binary before reloading the tmux and WezTerm integrations that
publish this identity; those files are the sole coordinated consumers.

`termnav ssh` sends each relay path through OpenSSH's per-session
environment channel, including sessions carried by an existing ControlMaster.
It never changes SSH's destination, remote-command arguments, or login-shell
selection. Explicit commands are enhanced only when their command line requests
a TTY, while non-TTY commands and control modes pass through unchanged. The
sourceable shell integration exposes this behavior to descendants through a
private `ssh` PATH shim, so scripts and tools need no Termnav-specific
integration.
Strict `ExitOnForwardFailure=yes` configurations are delegated unchanged rather
than making Termnav's optional relay mandatory.

Run tests with:

```bash
./test/termnav-test
```

CI also runs the WezTerm suite against Arch's current `eza` package so changes
to the optional `--hyperlink` argument cannot be hidden by the suite's
missing-tool skip on other platforms.

## License

MIT. See [`LICENSE`](LICENSE).
