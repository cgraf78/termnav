# AGENTS.md

## About

`termnav` is the single compiled executable for terminal navigation:
pane and tab routing, WezTerm link routing, tmux ctrl-click
follow-through, OSC-8-aware `eza` links, SSH reverse relay, and
existing-session Neovim opens. There is one installation path and no
source-checkout installer.

## Architecture

`src/main.rs` is a thin exit-code adapter. `src/commands/` validates
syntax and translates status. Reusable behavior lives in library
modules:

- `navigation` — typed scope traversal and routing
- `relay` — versioned Unix-socket protocol and transactional store
- `ssh` — one enhanced SSH child and its reverse-forward lifecycle
- `focus` — one-hop tmux leases and pane-style restoration
- `nvim` — target parsing, RPC, and mux-only remote reuse
- `click` — mouse-text recognition
- `assets` — installed provider-root discovery
- `terminal`, `process`, `runtime`, `links` — OS and terminal-protocol
  boundaries

`lib/termnav/` holds shell and Lua integrations. Shared route
vocabulary stays centralized so tmux, Neovim, and WezTerm agree.

Relay protocol vocabulary lives in `src/relay/protocol.rs`. The frozen
Python peer in tests is the cross-version oracle. Consumers should
invoke `termnav relay` rather than constructing protocol objects.

## Invariants

- `pane-move` stays inside local tmux ancestry. It never crosses SSH,
  WezTerm, VS Code, or another terminal boundary.
- Termnav never opens a second authenticated transport, including
  during cleanup or remote editor reuse.
- An indeterminate timed-out SSH request is never retried through a
  second transport.
- The private `share/termnav/shims/ssh` adapter is opt-in PATH
  interception only; recursive shim entry is rejected.
- MSRV is 1.88 (`Cargo.toml` `rust-version`).

## Testing

CI rust-ci `test-command`:

```sh
cargo test --locked && test/termnav-test
```

`test/termnav-test` builds the release binary unless
`TERMNAV_TEST_BINARY` is set. Prefer fake terminal commands and
fixtures over a live tmux, Neovim, SSH, or WezTerm session.

PR-only: `test/suites/relay-performance-test` against the PR base.
Required extra jobs: `test/suites/vscode-adapter-test` and Arch
`test/suites/wezterm-integration-test`.
