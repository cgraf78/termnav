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
- `termnav paste-image stage|receive ...`: give a desktop clipboard image to
  the program in the active tmux pane, locally or one remote hop away; see
  [Clipboard image paste](#clipboard-image-paste).
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
declined at the current boundary, `open-url` uses it when the destination
is not known to be WezTerm, and `paste-image stage` uses it when the clipboard
holds no image or also holds text. `nvim ssh-open` additionally uses `10` for
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
  the outer terminal, and resolves the outward route of one exact tmux client
  for other WezTerm user-variable publishers;
- `paste_image` owns the clipboard image transfer grammar, the desktop-side
  `stage` reader, and the popup-side `receive` endpoint;
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
- `lib/termnav/wezterm/paste-image.lua`: WezTerm endpoint of
  `termnav paste-image`: a Ctrl-V action and a `user-var-changed` handler for
  `TERMNAV_PASTE_IMAGE`. See [Clipboard image paste](#clipboard-image-paste).
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
Mouse coordinates supplied by tmux as `#{mouse_x}` and `#{mouse_y}` are
zero-based cells relative to the clicked pane. `TOP` and `LEFT` remain accepted
in the eleven-field click record for compatibility but are not subtracted from
those cells. Supply `SOCKET` from `#{socket_path}` to identify the exact tmux
server; Termnav also resolves Unicode grapheme widths.

For WezTerm regex captures, build the replacement with
`routes.raw_link_uri("$1")`. These are raw filenames, so a literal `%20` is
preserved. `nvim-open://`, `nvim-remote://HOST/PATH`, `lazygit-edit://`, and
`file://` callers supply percent-encoded URI targets. `file://` denotes a
literal filename: `file:///tmp/report:2026` opens `/tmp/report:2026`, never
`/tmp/report` at line 2026. Both raw and encoded colon bytes are filename data.
Diagnostic links retain `:LINE[:COLUMN]` syntax through the other schemes;
`nvim-remote://localhost/tmp/report:2026:12:3` opens the local filename
`/tmp/report:2026` at line 12, column 3. Localhost and this machine's hostname
route locally in both WezTerm and tmux.

**Producer migration:** diagnostic producers previously emitting
`file://HOST/PATH:LINE:COLUMN` must switch to
`nvim-remote://HOST/PATH:LINE:COLUMN`, including ripgrep's
`--hyperlink-format=nvim-remote://{host}{path}:{line}:{column}`. This corrects
an ambiguous historical contract; suffix-bearing file URIs are now literal.
Deploy the matching native binary and WezTerm Lua module before switching
diagnostic producers. `nvim open file` and `nvim open tmux-file` expose the
literal-filename routing mode; `link` and `tmux-link` retain location parsing.
The tmux variants display routing failures instead of returning them to the
mouse binding. A new Lua module paired with an older local native binary fails
visibly rather than guessing the filename. Remote SSH and configured helpers
keep the existing `link` API: literal targets append explicit `:1:0`
coordinates before transport, preserving the full filename on older peers.
No additional authenticated transport is opened for compatibility.

Generic links preserve genuine
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
`TERMNAV_TAB_SELECT`, `TERMNAV_TAB_MOVE`, and `TERMNAV_OPEN_URL` requests,
and the `TERMNAV_PASTE_IMAGE` announcements handled by `paste-image.lua`.
`routes.setup()` owns the request handlers; consumers should use route helpers
instead of interpreting metadata directly, and should publish open-URL
requests through `termnav open-url`. There are no `DOT_*` aliases.

## Clipboard image paste

Pressing Ctrl-V in WezTerm while the desktop clipboard holds an image, such as
a screenshot, gives that image to the program in the active tmux pane, whether
it runs on the desktop or on a remote host. The image is saved as a PNG on the
host where the innermost tmux runs and its absolute path is bracket-pasted
into that pane; coding agents that attach an image when its path is pasted
pick it up from there. Text paste is unchanged. The image travels in-band
through the terminal byte stream, so it crosses SSH, Eternal Terminal, and
local tmux layers without a tunnel, port forward, second login, or daemon.

### Flow

1. **Gesture.** `paste-image.lua`'s Ctrl-V action always performs WezTerm's
   normal `PasteFrom("Clipboard")` first, immediately. Only when the pane's
   terminal owner is tmux (the `link-routes.lua` policy) does a scheduled
   callback run `termnav paste-image stage`. On exit `0` WezTerm arms that
   pane for 10 seconds with the staged file and sends the private trigger key
   `\033[777010u` to it. Any other status ends the gesture; a text or mixed
   clipboard has already been pasted as text.
2. **Routing.** tmux matches the trigger as `user-keys[14]` (`User14`). Each
   layer either declines with a message (the active pane is in a mode, panes
   are synchronized, or the next hop cannot carry the transfer), forwards the
   key inward with `send-keys -H` into a nested tmux, or, as the innermost
   layer, opens a popup running `termnav paste-image receive` for its active
   pane and the client that pressed the key. Forwarding policy belongs to the
   consumer's tmux configuration.
3. **Handshake.** The receiver puts its terminal in raw mode, generates a
   random id, and announces `ready:<id>` to WezTerm through the exact
   client's outward route, the one `tmux follow-click` uses for URLs, never
   the session's most recent client.
4. **Transfer.** WezTerm accepts `ready` only for the armed pane, once, before
   the arm expires, and types the transfer into the popup in bounded chunks
   from timer callbacks.
5. **Delivery.** The receiver rejects the first byte outside the grammar,
   checks the id, length, SHA-256 digest, size cap, and PNG signature, writes
   the image atomically, announces `done:<id>`, bracket-pastes the path into
   the pane through a private tmux buffer (`paste-buffer -p -d`), and exits.
   On any failure it announces `cancel:<id>`, keeps discarding input until
   the terminator `!` (or the idle limit) so bytes still in flight never
   reach the pane, shows a message on the client, and writes nothing.
6. **Cleanup.** WezTerm deletes the staged file on `done`, `cancel`,
   abandonment, or expiry. `stage` prunes its own staged files older than ten
   minutes, and `receive` prunes its own images older than seven days.

### Commands

`termnav paste-image stage` reads the desktop clipboard through reader
subprocesses that share a hard five-second limit; validation, hashing, and
encoding follow outside it and are linear in the capped size. WezTerm runs
staging asynchronously, so the limit can delay only an image paste. A
reader's output goes straight to an owner-only file, and a file-size limit
stops it one byte past the cap. macOS uses one `osascript` run that inspects the pasteboard
classes and writes its PNG class into an owner-only file; Linux uses
`wl-paste --list-types` and `wl-paste --type image/png` when
`WAYLAND_DISPLAY` is set (falling back to X when `wl-paste` is missing), or
`xclip -selection clipboard -o -t TARGETS` and `-t image/png` when `DISPLAY` is
set. It exits `3`, printing nothing, when the clipboard offers no `image/png`
or also offers plain text (`text/plain`, `UTF8_STRING`, `STRING`, `TEXT`,
`COMPOUND_TEXT`, or the macOS string classes). Otherwise it checks the PNG
signature and the 16 MiB cap, writes `<length>:<sha256-hex>:<base64>` as one
line without a newline to an owner-only `stage-*.b64` file in
`$XDG_RUNTIME_DIR/termnav-UID/paste-image/` (the private runtime directory, below the temporary directory when `XDG_RUNTIME_DIR` is unset),
prints that path, and exits `0`. A reader failure, timeout, non-PNG data, or
an oversized image exits `1`.

`TERMNAV_PASTE_IMAGE_CLIPBOARD` replaces the platform reader with a command
run as `COMMAND types` (print offered MIME types, one per line) and
`COMMAND png` (print the PNG). It exists for tests; it also lets an
unsupported desktop supply its own reader.

`termnav paste-image receive --target-pane PANE --client-name NAME
[--client-pid PID --client-tty TTY] [--tmux-socket PATH]` is the popup
endpoint. `PANE` receives the path; the client identifies where announcements
go and where messages are shown. Without `--client-pid` and `--client-tty`
the client is resolved from its name. tmux commands use `--tmux-socket`, or
the socket in the popup's `$TMUX`, explicitly. Images are written as
`clip-<UTC timestamp>-<random>.png` (0600) under
`$XDG_CACHE_HOME/termnav/paste-images`, or `$HOME/.cache/termnav/paste-images`
when the XDG value is unset or relative. Both Termnav directories are created
or tightened to 0700, and a directory that is a symlink, not a directory, or
owned by another user is refused. Exit status `0` means the image was saved
and its path pasted; `1` means nothing was pasted and a message was shown; `2`
is invalid syntax.

### Protocol

| Item | Value |
| --- | --- |
| Trigger key | `\033[777010u`, tmux `user-keys[14]` |
| User variable | `TERMNAV_PASTE_IMAGE` |
| Announcements | `ready:<id>`, `ack:<id>:<offset>:<proof>`, `done:<id>`, `cancel:<id>` |
| Transfer | `<id>:<length>:<sha256>:<base64>!` |

- `<id>` is 16 lowercase hexadecimal characters from the receiver; WezTerm
  accepts 8 to 64.
- `<length>` is the decoded PNG size in decimal without sign or leading zero,
  from 8 to 16777216 (16 MiB).
- `<sha256>` is 64 lowercase hexadecimal characters.
- `<base64>` uses the standard alphabet with mandatory `=` padding on one
  line: exactly `4 * ceil(length / 3)` characters.
- `<offset>` counts transfer bytes the receiver has accepted, from the first
  id byte. Offsets strictly increase, so every announcement is a distinct
  value; WezTerm fires `user-var-changed` only on change.
- `<proof>` is the first 16 characters of `<sha256>`, which the receiver
  could only learn by reading the header WezTerm typed.

Flow control: until an acknowledgement with the matching proof arrives,
WezTerm types only the header `<id>:<length>:<sha256>:`, at most 139 bytes of
digits, lowercase hex, and colons. The receiver acknowledges as soon as it
has accepted the header, then at least every 128 KiB. WezTerm accepts an
acknowledgement only from the transfer's pane, for its id, with the proof,
and for an offset it has sent; it then keeps at most 512 KiB beyond that
offset in flight and sends at most 32 KiB per timer callback. It abandons
the transfer, sending `!`, when no acknowledgement arrives for five seconds,
and sends `!` as soon as the transfer's pane announces `cancel` under any id,
because a receiver rejecting a transfer meant for another id is still
discarding input until the terminator.

The receiver gives up after 30 seconds without input. A first byte that
cannot start a transfer (not lowercase hex), such as Esc, is the user's
keystroke: it cancels and discards input for a one-second quiet period. A
lowercase hex first byte with the wrong id is a transfer meant for another
receiver: it cancels and discards until `!` or the 30-second idle limit.

The WezTerm module sends nothing but the header, base64 characters, and the
terminator, even if a staged file was replaced; a malformed staged header
sends only `!`.

### Wiring

The innermost tmux layer needs a binding like this. `display-popup` does not
expand formats in its command, so the binding goes through `run-shell`, which
does:

```tmux
set -s user-keys[14] "\e[777010u"
bind-key -n User14 if-shell -F "#{||:#{pane_in_mode},#{synchronize-panes}}" {
  display-message "Image paste is unavailable here"
} {
  run-shell -b "tmux -S #{q:socket_path} display-popup -c #{q:client_name} -t #{q:pane_id} -w 60 -h 5 -E 'termnav paste-image receive --target-pane #{q:pane_id} --client-name #{q:client_name} --client-pid #{client_pid} --client-tty #{q:client_tty} --tmux-socket #{q:socket_path}'"
}
```

In WezTerm,
pass the `link-routes.lua` instance so both modules share one terminal-owner
policy, bind Ctrl-V, and forward `user-var-changed`:

```lua
local routes = dofile(link_routes_path).new(wezterm)
local paste_image = dofile(paste_image_path).new(wezterm, { routes = routes })
table.insert(config.keys, { key = "v", mods = "CTRL", action = paste_image:paste_action() })
wezterm.on("user-var-changed", function(window, pane, name, value)
  paste_image:user_var_changed(window, pane, name, value)
end)
```

`user_var_changed` returns whether the variable belongs to this protocol. Do
not return that value from the event handler: `false` would stop later
handlers such as `routes.setup()`'s. `new` also accepts `termnav`, an explicit
binary path; without it the binary is found as `link-routes.lua` finds it.
Without `routes`, the module loads `link-routes.lua` from its own directory.

### Security model

The desktop serves nothing on request. A remote process can cause an image to
be sent only by announcing `ready` while WezTerm is armed for the pane that
leads to it, which requires the user's Ctrl-V there within the last ten
seconds, and only once per arm. Residual risks, accepted: another visible
pane, or a same-user process that can write the client's terminal, can forge
`ready` during that window. WezTerm then types only the header (digits,
lowercase hex, and colons) into whatever has focus. A forger that cannot read
that input cannot produce the proof, so WezTerm abandons after five seconds
with `!`. A forger that is itself the focused application, or otherwise
reads what WezTerm types, can produce the proof and receives the image the
user just pasted. If a forged `ready` wins the race against the real
receiver, the receiver rejects the foreign id, keeps discarding, and its
`cancel` makes WezTerm end the transfer, so nothing reaches the pane's
application. If focus moves to another pane mid-transfer, at most the current
window (512 KiB beyond the last acknowledgement, or the header before the
first) can reach that pane's application before acknowledgements stop.

### Reach and non-goals

The receiver's announcements use the outward user-variable route, which
follows local tmux layers and wraps one passthrough frame for a layer it
cannot inspect, so the reach is local tmux plus one remote hop; an
uninspectable layer needs `allow-passthrough`. Remote panes without tmux keep
plain text paste. mosh is unsuitable for multi-megabyte input and should be
declined by routing. A clipboard holding both text and an image pastes the
text. VS Code's integrated terminal intercepts Ctrl-V itself. Only PNG images
are supported. A 16 MiB image crossed one local tmux layer in about 3.5
seconds in the test harness.

Known limitations, shared with pane navigation's forwarding gate: a layer
can see only the foreground program of its pane. When it forwards the
trigger into SSH or Eternal Terminal with mouse reporting on but no tmux runs
on the far side, the remote application receives the trigger key itself:
remote Neovim inserts a stray character and Vim inserts `2` and leaves insert
mode. Likewise a remote tmux whose configuration lacks the `user-keys[14]`
entry passes the raw trigger to its pane's application. No `ready` follows in
either case, so WezTerm sends nothing else and the arm expires.

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
and `--failed`, and accepts an optional `--waiting` (default 0). Counts are
nonnegative integers capped at 100,000, and `failed + waiting` must not exceed
`pending`.

Counts are per-pane effective states chosen by the publisher; Termnav carries
them unchanged. tmux-tools' `term-pane-state` counts each pane once, under the
first match of red (a failed command or blocked agent), yellow (waiting for an
answer), green (other pending attention), or working. `pending` counts the red,
yellow, and green panes, `failed` the red ones, `waiting` the yellow ones, and
`working` only panes without attention. Consumers derive green as
`pending - failed - waiting`.

The client identity and foreground terminal ownership must still match when
publishing. Exit status 0 means the parent accepted the snapshot or Termnav
published it to the exact direct terminal. Status 1 is a routing, terminal, or
identity error; invalid arguments return 2. Legacy versions return 3 for a
direct terminal; callers must treat it as unsupported and require an updated
Termnav, never implement their own tty-write fallback. No new SSH connection
is opened. The accompanying tmux-tools `term-pane-state` publisher supplies
these identities and aggregates each layer's local and child counts.

Parent panes receive a 15-second lease in `@termnav_child_attention`, measured
on the receiving host, as `EXPIRY:WORKING:PENDING:FAILED`. The waiting count
travels in a separate `@termnav_child_waiting` lease, `EXPIRY:WAITING`, with
the same expiry. Both options are written in one tmux command list, and the
waiting lease is removed in that list when the count is 0. A direct terminal
receives the `TERM_PANE_STATE` OSC user variable, a JSON object with `v` (1),
`working`, `pending`, `failed`, `waiting`, and `heartbeat`. These are advisory
snapshots: overlapping deliveries can briefly arrive out of order, the next
publication reconciles them, and disconnected publishers expire. Consumers
must expire terminal state by local receipt time rather than comparing clocks
across machines. The protocol does not acknowledge user attention or alter the
underlying activity lifecycle.

That command list holds two `set-option` commands, the main lease first, sent
from Termnav's own tmux client. Each publish renews the lease with a later
expiry; tmux-tools' `term-pane-state` publishes from a status job every few
seconds, usually with the same counts. A parent tmux that forwards only from
its own status job therefore adds up to one interval per nested layer. To
forward a change at once, a parent config can hook `after-set-option`: tmux
3.2 and later run it after each `set-option` a client sends, with the
command's pane as its target and its arguments in `#{hook_arguments}`.
Commands a hook runs synchronously, such as those inside `if-shell -F`, do
not fire it again, while background `if-shell -b` or `run-shell -b -C`
commands started from a hook do. Matching the two option names and comparing
the counts without their expiry with the last ones seen on that pane keeps
renewals process-free.

`waiting` is an additive field: the relay request stays protocol v2 and the
terminal snapshot stays `"v":1`. Readers treat a request or snapshot without
`waiting` as 0, and older readers ignore it, so layers can update in any
order. The main lease keeps its exact four-field shape because older readers
discard leases with extra fields. An older Termnav renews only the main lease:
a request it relays to a newer Termnav arrives without `waiting` and clears
the waiting lease, while its own direct pane writes leave an earlier waiting
lease to expire. Lease readers should therefore ignore an expired waiting
lease and clamp it to `pending - failed`. Wherever a layer is older, waiting
panes still count as pending and display as ordinary attention.

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
