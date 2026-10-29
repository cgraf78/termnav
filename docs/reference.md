# termnav reference

Command, protocol, and integration contracts for people wiring termnav into
their own configuration or building on it. The [README](../README.md) covers
installation and the quickstart, [`examples/`](../examples/) holds tested
configuration, and `man termnav` has the complete command syntax.

## Command interface

`termnav` is the only compiled executable. Its stable command groups are:

- `termnav navigate ACTION DIRECTION`: route pane selection, pane movement,
  tab selection, and tab movement. `pane-move left|down|up|right` swaps the
  active pane with one directional neighbor through local tmux ancestry only;
  it never crosses SSH, WezTerm, VS Code, or another terminal boundary.
- `termnav ssh SSH-ARG...`: supervise an enhanced interactive SSH session and
  its connection-scoped reverse relay. Plain and control-mode invocations
  replace Termnav with OpenSSH, preserving the launched PID. Termnav never
  opens a second authenticated transport, including during cleanup or remote
  editor reuse.
- `termnav relay send ACTION DIRECTION`: send one navigation request to the
  inherited relay using the same `pane-select`, `tab-select`, or `tab-move`
  action names as `termnav navigate`. The host-local `pane-move` action is
  rejected rather than serialized. `serve`, `commit`, and `sweep` are
  documented integration commands used by the SSH and tmux adapters rather
  than interactive entry points.
- `termnav tmux context ...`: publish tmux ownership to one exact terminal
  client; `termnav tmux focus ...` maintains hierarchical focus leases; and
  `termnav tmux follow-click ...` resolves mouse metadata to a browser or
  editor target. A URL target is delivered the way `open-url` delivers, but
  starting from the exact client that received the click instead of the most
  recent one, so it crosses a local or SSH-separated outer tmux; like
  `open-url`, it identifies WezTerm across SSH by tmux 3.2's XTVERSION record.
  A clicking control-mode or VS Code (xterm.js) client declines rather than
  receiving the request. When the request is declined or fails, `$BROWSER`
  runs with the URL (the shared convention: `:`-separated entries, each a
  program with optional arguments, `%s` for the URL), as if from the clicked
  pane. An entry that is still running after three seconds counts as
  launched and is left running; one that fails sooner passes the URL to the
  next. When none opens it, the clicking client shows a message with the URL. A
  clicked line that is not on the live screen, such as a scrolled copy-mode
  line, resolves from tmux's copy of that line. A well-formed click exits `0`
  and writes nothing to stdout, which tmux would show in the pane.
- `termnav link-host`: print the host represented by the current terminal
  context for hyperlink producers such as ripgrep and eza.
- `termnav open-url [--tty PATH] URL`: ask the outer terminal to open an
  `http`, `https`, or `mailto` URL on the machine that runs it, across SSH and
  tmux, through the `TERMNAV_OPEN_URL` request. Inside tmux it writes to the
  most recently active client of the caller's session, preferring clients
  showing the caller's window and skipping control-mode and VS Code (xterm.js)
  clients; otherwise it writes to `--tty PATH` or the controlling terminal.
  `--tty` serves callers without a controlling terminal, such as Neovim's TUI
  server, whose stderr is still the terminal; it is ignored inside tmux.
  The request is written only when the destination is known to be WezTerm,
  the only terminal that acts on it: a tmux client by the XTVERSION reply tmux
  records as `client_termtype` (or, on tmux without one, its `TERM_PROGRAM`,
  `WEZTERM_PANE`, or terminal name), and a plain terminal by
  `TERM_PROGRAM=WezTerm` (which decides whenever set), `WEZTERM_PANE`, or
  `TERM=wezterm`. A client running inside another local tmux is followed to
  that tmux's client for the hosting pane and written to directly; ancestry
  whose pane does not own the client's tty is not followed. A tmux layer that
  cannot be inspected from this host, such as local tmux behind an SSH
  session, or one whose tmux reply proves it is tmux but whose pane cannot be
  found, gets one passthrough frame and is trusted; it then needs
  `allow-passthrough`. GNU screen (`STY`) cannot forward it and declines, as
  does everything else, before writing and with status `3`. That includes SSH
  or WSL sessions from WezTerm that forward none of those variables; set
  WezTerm's `term = "wezterm"` (with its terminfo installed remotely) to make
  such sessions identifiable. Other schemes and control characters are invalid
  syntax (`2`). No eligible client or terminal is an operational failure
  (`1`); stdout is never used. Success means the
  request was written, not that a terminal acted on it. Callers should fall
  back to their own opener on any nonzero status. Editors and scripts should
  call it rather than constructing the escape.
  WezTerm's `routes.setup()` handler enforces the same scheme and
  control-character policy on every `TERMNAV_OPEN_URL` value, because any
  program whose output reaches the terminal can set it; rejected values are
  logged as a warning without the value and never opened.
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
declined at the current boundary, and `open-url` uses it when the destination
is not known to be WezTerm. `nvim ssh-open` additionally uses `10` for
unavailable connection configuration, `11` for an invalid host, `12` for SSH
failure, and `13` for a disallowed host. `vscode focus` uses `10` when no
adapter is available. Passthrough commands otherwise preserve child status.

## Integration notes

Ripgrep's `--hostname-bin` accepts an executable name but cannot pass arguments.
A consumer can provide a tiny `ripgrep-link-host` wrapper that delegates to the
explicit `termnav link-host` command; Termnav therefore needs no alternate
executable name or argv[0] dispatch. The private `share/termnav/shims/ssh`
adapter remains available for callers that explicitly opt into scoped PATH
interception. It contains only enough policy to execute `termnav ssh` and passes
its exact runtime directory to the native resolver because checkout and
installed shim copies can coexist during an update. Recursive shim entry is
rejected before another process can be created. During the bounded stage where
the shim arrives before the binary, it uses a trusted platform OpenSSH path
rather than the inherited shim-bearing PATH so repository synchronization can
finish safely. Historical public command names are not installed.

There is intentionally no source-checkout installer. Shdeps consumers use the
ordinary `cgraf78/termnav github` dependency form, which prefers a published
release archive and falls back according to Shdeps' generic provider policy.

## Code organization

The CLI adapters under `src/commands/` validate syntax and translate exit
status. Reusable behavior lives behind focused library interfaces:

- `navigation` owns typed scope traversal and routing decisions;
- `relay` owns the versioned Unix-socket protocol and transactional directive
  store;
- `ssh` owns one enhanced SSH child and its reverse-forward lifecycle, while
  plain invocations replace Termnav with OpenSSH;
- `focus` owns one-hop tmux leases and pane-style restoration;
- `nvim` owns target parsing, registry selection, RPC, exact-pane transport
  fallback, and mux-only remote reuse;
- `click` owns mouse-text recognition and returns typed URL/file targets;
- `browser` owns open-URL validation and delivery of `TERMNAV_OPEN_URL` to
  the outer terminal;
- `vscode` owns VS Code window focus publication to the companion extension;
- `assets` owns installed provider-root discovery;
- `shell` owns POSIX shell quoting and tmux format escaping at explicit
  subprocess boundaries;
- `cli` owns top-level command parsing and dispatch;
- `version` owns the embedded build identity;
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
- `lib/termnav/nvim/navigation.lua`: Neovim pane and tab navigation. Its
  `setup()` owns `<C-h/j/k/l>` selection and `<M-H/J/K/L>` movement mappings in
  normal and terminal modes; terminal mappings preserve fzf input and own the
  leave-job-mode/schedule/re-enter transition. Directional pane edges and tab
  boundaries use bounded one-shot native jobs, while Ctrl-backslash preserves
  the local previous-split/previous-tmux-pane history expected from
  vim-tmux-navigator without guessing at an ancestor's history.
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
- `share/termnav/shell.sh`: sourceable interactive shell loader for shell-local
  SSH relay interposition, WezTerm pane context publishing, and file-link mode
  classification. Its `ssh()` function affects commands entered in that shell;
  child processes resolve their own SSH executable from PATH.

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

## VS Code MCP fallback

The `nabheet.vscode-ide-mcp` extension is required only when
`TERMNAV_VSCODE_FALLBACK_BACKEND=mcp` selects the MCP fallback route. Its
deployment must set the absolute `TERMNAV_VSCODE_MCP_TOKEN_FILE`. Termnav
defaults to `127.0.0.1:9876/mcp`, `tools/call`, `execute_command`, and VS
Code's terminal previous/next command IDs. Consumers may override those with
`TERMNAV_VSCODE_MCP_ADDRESS`, `TERMNAV_VSCODE_MCP_PATH`,
`TERMNAV_VSCODE_MCP_METHOD`, `TERMNAV_VSCODE_MCP_TOOL`,
`TERMNAV_VSCODE_MCP_PREVIOUS_COMMAND`, and
`TERMNAV_VSCODE_MCP_NEXT_COMMAND`; non-loopback addresses are rejected.

## Click detectors and remote link hosts

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

The eza adapter percent-encodes literal semicolons only in OSC 8 URI fields,
locally and remotely. This preserves file links with stable WezTerm's parser;
Display labels, OSC parameters, and ordinary output are unchanged; encoded
targets retain their file semantics when their path interpretation is unique.
Eza's partially encoded percent signs are resolved against existing paths on
the producing host. The adapter checks every interpretation of at most eight
percent-encoded characters (256 metadata probes maximum per complete URI).
Multiple existing interpretations, metadata errors, larger expansions, or a
URI above 64 KiB disable that explicit link while preserving its displayed
label. UTF-8 characters are interpreted as whole scalars, never partial bytes.
Mouse coordinates supplied to `follow-click` are zero-based window cells.
Supply `TOP` and `LEFT` from the clicked pane and `SOCKET` from `#{socket_path}`;
Termnav translates to pane cells once and resolves Unicode grapheme widths.

For WezTerm regex captures, build the replacement with
`routes.raw_link_uri("$1")`. These are raw filenames, so a literal `%20` is
preserved. Existing `nvim-open://`, `lazygit-edit://`, and `file://` callers
continue to supply percent-encoded URI targets. Generic links preserve genuine
`a/` and `b/` directories; Git diff prefix interpretation belongs to the producer
that knows it is emitting a diff, not to the generic editor opener.

On supported stable WezTerm, file clicks schedule an asynchronous acknowledged
opener. The GUI stays responsive while the native helper applies its RPC/SSH
deadlines. A failed request displays an actionable notification without echoing
untrusted terminal paths or child stderr. Closed panes are not acted on, and
only one request per pane is in flight at once.

## WezTerm protocol

The WezTerm protocol consists of `IS_NVIM`, `NVIM_OPEN_SOCKET`,
`NVIM_LINK_CWD`, `NVIM_REMOTE_LINK_HOST`, `NVIM_REMOTE_CWD`,
`NVIM_REMOTE_TMUX`, and `TERMNAV_TMUX` pane metadata plus the
`TERMNAV_TAB_SELECT`, `TERMNAV_TAB_MOVE`, and `TERMNAV_OPEN_URL` requests.
`routes.setup()` owns the request handlers; consumers should use route helpers
instead of interpreting metadata directly, and should publish open-URL
requests through `termnav open-url`. There are no `DOT_*` aliases.

## Neovim integration options

`lib/termnav/nvim/setup.lua` accepts `group_name`, `opener`, `navigation`,
`wezterm_vars`, `vscode_focus`, `publish_delay_ms`, `publish_events`,
`refresh_events`, and `clear_events`. `navigation.lua` accepts `application`,
`command`, `executable`, `mappings`, `notify`, `schedule`, `spawn`, and
`terminal`. Outside tmux, `terminal()` names the editor's terminal for
`termnav navigate --tty`, because Neovim 0.10+ runs the editor in a session
whose children cannot open `/dev/tty`. `wezterm-vars.lua` publishes to the
same device and owns that lookup as `editor_tty_path()`: the controlling
terminal when there is one, otherwise the stderr terminal, and that only while
the TUI attached over the editor's stdio is present, so a UI attached over a
socket (`--remote-ui`, or what remains after `:detach`) never redirects it.
`vscode-focus.lua` accepts `command`, `interval_ms`, `observed`, and `source`.
Defaults are production implementations; collaborator overrides exist for
embedding and deterministic tests. A partial `application` table is merged
over those defaults, so a consumer can replace its tab abstraction without
silently removing provider-owned pane movement. It may override
`pane_move(source_window, target_window)` and return whether it handled the
local swap; an explicit non-function value disables that primitive and makes
local pane movement a safe decline.

The Neovim navigation context exposes `pane_move("left"|"down"|"up"|"right")`.
It swaps the current buffer and saved view with the directional neighbor and
keeps focus on the moved content. At a Neovim edge it queues
`termnav navigate pane-move DIRECTION`; no tmux means a process-free no-op.
Consumers should not copy the terminal-mode transition or install duplicate
Alt mappings when `mappings` is enabled. A tmux binding that has already
decided it owns the chord should invoke `termnav navigate pane-move DIRECTION`
in the foreground without `--parent`; the typed router probes the current scope
and any same-host containing tmux scopes itself.

Local Neovim movement starts no process. A tmux movement uses one Termnav
process and one guarded mutation command for each attempted local scope;
bounded client inspection and revalidation queries run only when session
identity or outward traversal requires them. Routing is bounded by actual
nesting depth, never starts a resident worker, and stops before relay or
terminal I/O.

## Shell callbacks

The stable shell callbacks from `share/termnav/shell.sh` are
`_termnav_wezterm_publish_link_context`, `_termnav_wezterm_preexec COMMAND`,
`_termnav_wezterm_precmd`, and `termnav_file_links_need_plain_output`. The first
three are hook callbacks despite their leading underscore; callers must not use
other `_termnav_*` functions.

## Remote command paths

Remote typed-command fallbacks append the generic user command directories
`$HOME/.local/bin`, `$HOME/.local/share/mise/shims`, `/opt/homebrew/bin`, and
`/usr/local/bin` after inherited PATH. Set `TERMNAV_REMOTE_TOOL_PATH` in the
environment that launches Termnav to replace that append-only list. Termnav
embeds the selected paths in the existing ControlMaster command, rebasing paths
below the local home onto remote `$HOME`; it does not require sshd `AcceptEnv`
or open another authenticated transport.

## File-link classification

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

## VS Code focus protocol

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

## Tab scope routing

`termnav tmux context` is intended for tmux's `client-attached` hook.
WezTerm user variables belong to a terminal pane rather than the long-lived
tmux pane, so attaching a new client to an existing session does not inherit
the shell or Neovim metadata previously emitted there. The attach publisher
writes `TERMNAV_TMUX=true` synchronously to the exact client tty. Direct
terminal clients receive raw OSC; an immediate tmux parent receives one
passthrough wrapper, including when it advertises legacy `screen-*` terminfo.
Control-mode clients are skipped without requiring terminal metadata.

## Attention across terminal layers

`termnav tmux attention` forwards a snapshot from one exact attached client to
its containing tmux pane or existing SSH relay. It requires `--tmux-socket`,
`--client-name`, `--client-pid`, `--client-created`, `--working`, `--pending`,
and `--failed`. Counts are nonnegative integers capped at 100,000; failures
must be a subset of pending attention. Work and attention remain independent,
so an unfinished activity does not hide a failure elsewhere in the session.

The client identity and foreground terminal ownership must still match when
publishing. Exit status 0 means the parent accepted the snapshot or Termnav
published it to the exact direct terminal. Status 1 is a routing, terminal, or
identity error; invalid arguments return 2. Legacy versions return 3 for a
direct terminal; callers must treat it as unsupported and require an updated
Termnav, never implement their own tty-write fallback. No new SSH connection
is opened. The accompanying tmux-tools `term-pane-state` publisher supplies
these identities and aggregates each layer's local and child counts.

Parent panes receive a 15-second lease in `@termnav_child_attention`, measured
on the receiving host. A direct terminal receives the `TERM_PANE_STATE` OSC
user variable. These are advisory snapshots: overlapping deliveries can briefly
arrive out of order, the next publication reconciles them, and disconnected
publishers expire. Consumers must expire terminal state by local receipt time
rather than comparing clocks across machines. The protocol does not acknowledge
user attention or alter the underlying activity lifecycle.

Direct attention writes, including the SSH relay's terminal edge, use a fresh
nonblocking descriptor with matching character-device, inode, and device
identity. Client and foreground ownership are checked again after opening.
Backpressure is limited to 100 milliseconds; a partial frame fails without
replay. Inherited descriptors and navigation commit writes are unchanged.
Attention frames begin with CAN at each immediate parser boundary (outside
and inside a tmux passthrough frame) so a later snapshot cancels an earlier
incomplete escape. This is terminal output parsing recovery, not terminal
input or user acknowledgment. Platforms that deny reopening the captured
terminal decline attention publication rather than use an unbounded write.

## Nested tmux leaf focus

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
publish to their own parent panes independently. Each SSH relay remembers its
latest accepted focus generation and releases it before returning control to
the shell, including when a remote detach message cannot arrive before the
transport closes. Cleanup cannot clear a newer connection's generation.

The default publisher interval is one second, with a 3.5-second lease to allow
for scheduling and relay delays between renewals. The lease is a crash fallback:
a killed publisher or relay, unreachable parent, or missed detach event still
fails closed when the last renewal expires.

`watch`, `stop`, and `sync` identify their client either by name
(`--client-name`) or by exact PID and tty (`--client-pid` with
`--client-tty`). In notification hooks (`client-attached`, `client-detached`,
`client-focus-in`, `client-focus-out`, `client-session-changed`), tmux expands
`#{client_pid}` and `#{client_tty}` for the session's most recently active
client rather than the client that raised the event, so a second client of a
shared session could start the busier client's publisher and never claim its
own parent pane. Those hooks pass `#{hook_client}` instead. Termnav ignores a
named client that has gone, as in a detach hook, and a control-mode client.
tmux 3.2 has no `#{hook_client}`; there the example falls back to the exact
identity, which the attach hook resolves to the attaching client. Command hooks
such as `after-select-pane` run with the invoking client and keep the exact
form, which older configurations use for every hook.

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
available so a buffered chord sequence keeps its original ancestry. When
several clients claim focus, as happens when an idle terminal on another machine
never reports focus-out from a shared session, the uniquely fresh focused client
is the one that carried the chord and is retained. Physical ambiguity, including
stale or tied focus claims, fails closed only when the action must leave the
local tmux scope.
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

## Neovim socket discovery

Neovim socket discovery state lives under
an absolute `$XDG_STATE_HOME/nvim-tmux-open`, falling back to
`$HOME/.local/state/nvim-tmux-open`. Discovery records are published
atomically with owner-only permissions and are scoped to the tmux server,
pane, and Neovim process so concurrent editors cannot replace or remove one
another's registrations. Each scope's atomic `latest` record is a complete
ordering point, while a final versioned per-process commit record keeps partial
initial publications undiscoverable. Long encoded pane identities are split
across bounded path components without losing identity information.
When Neovim does not already expose a record-safe RPC address,
Termnav creates its socket in the first candidate whose socket path is at
most 100 bytes (below every supported platform's Unix socket limit):
`$XDG_RUNTIME_DIR/termnav` and then `$TMPDIR/termnav-UID` when those base
directories are absolute and exist, then a short `/tmp/termnav-UID` fallback.
The chosen directory is secured owner-only, and socket path limits do not
depend on the state-directory length. Empty and relative values are ignored
(as XDG requires for `XDG_RUNTIME_DIR`). Runtime and temporary paths
containing newlines are also skipped so line-oriented discovery records remain
valid.
Without either state base directory,
filesystem discovery and publication are disabled rather than writing below
`/`. Set `XDG_STATE_HOME` to an absolute
path in the environment that starts WezTerm, tmux, and Neovim; setting it only
inside a child interactive shell cannot change an already-running GUI or tmux
server's environment.

## Remote editor opens

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

## SSH relay

`termnav ssh` sends each relay path through OpenSSH's per-session
environment channel, including sessions carried by an existing ControlMaster.
It never changes SSH's destination, remote-command arguments, or login-shell
selection. Explicit commands are enhanced only when their command line requests
a TTY, while non-TTY commands and control modes pass through unchanged. The
sourceable shell integration exposes this behavior through a shell-local
`ssh()` function. Scripts and descendant tools continue to resolve their own
SSH executable; callers that intentionally need inherited interception can add
the private shim directory to only that launcher's PATH.
Strict `ExitOnForwardFailure=yes` configurations are delegated unchanged rather
than making Termnav's optional relay mandatory.

The relay path travels as `TERMNAV_PARENT_RELAY` over `SendEnv`, next to a
`RemoteForward` of the local relay socket, so the remote sshd must accept that
variable. Install the shipped `share/termnav/sshd_config` fragment (`AcceptEnv
TERMNAV_PARENT_RELAY`; `termnav asset-path share/termnav/sshd_config` prints
its path) on each participating host, for example as
`/etc/ssh/sshd_config.d/termnav.conf`, then reload sshd. Keep
Unix-socket forwarding (`AllowStreamLocalForwarding`) enabled. The remote host
also needs termnav and its tmux and Neovim integration.

The remote relay socket defaults to `/tmp`. For a host where that directory
is unavailable, configure an existing, writable remote directory through a
host-scoped OpenSSH setting, for example in `~/.ssh/config`:

```sshconfig
Host phone
    SetEnv TERMNAV_REMOTE_SOCKET_DIR=/data/data/com.termux/files/usr/tmp
```

Termnav reads this hint from the existing `ssh -G` configuration query; it
does not probe the remote host or expand its environment. The hint itself
does not need `AcceptEnv`, but the generated `TERMNAV_PARENT_RELAY` still does.
OpenSSH removes configuration quotes before Termnav reads each assignment.
Use a literal absolute directory containing only ASCII letters, digits,
`/`, `.`, `_`, and `-`, with no `.` or `..` path components. Spaces, colons,
shell expressions, and token expansion such as `%h` are unsupported. The
complete generated socket path must fit 103 bytes; with the current filename,
the directory excluding trailing slashes can use at most 59 bytes. Repeated
hints must agree exactly. An invalid or conflicting hint leaves the original
SSH invocation unchanged, without a Termnav relay. A valid hint does not
create or check the remote directory; OpenSSH reports a forwarding failure if
it cannot bind there. The same selected socket is used for the reverse
forward, per-session environment, and existing-master cleanup.
