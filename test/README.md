# Test Harness

`test/termnav-test` is the complete local and CI entrypoint. It loads shared
helpers from `test/helpers.sh`, uses fixtures under `test/fixtures/`, and runs
focused suites from `test/suites/`.

Suite ownership follows the integration boundary:

- `ci-contract-test` keeps the repository's shared-workflow inputs and required
  aggregate aligned with the supported platform and performance matrix.
- `cli-contract-test` covers the unified command surface and usage failures.
- `examples-test` verifies that the documented tmux, Neovim, WezTerm, and token
  extension examples remain loadable and wired to the public API.
- `install-test` verifies the generated one-binary installer policy, unified
  manual page, retired checkout bootstrap, and absence of compatibility
  aliases. Shared release behavior is exercised by the package smoke jobs.
- `manpage-test` keeps the unified manual page aligned with explicit-command
  behavior.
- `navigation-cli-test` covers semantic navigation and exact-client tmux
  traversal through real tmux clients.
- `nvim-test` covers Neovim helper behavior.
- `nvim-launcher-test` covers the conservative sourceable policy for reusing an
  existing Neovim pane from simple interactive-shell file opens.
- `nvim-navigation-test` covers Neovim's fast-path mappings and boundary
  delegation.
- `process-contract-test` ensures retired executable and Python implementation
  paths stay absent from production artifacts.
- `relay-test` covers nested SSH transport, directive storage, and in-band
  commit behavior.
- `relay-performance-test` keeps production `send`, `commit`, and tmux-boundary
  dispatch on the lightweight path. Set `TERMNAV_PERFORMANCE_BASELINE` to a Git
  revision to compare alternating subprocess samples and enforce calibrated
  median non-regression budgets plus absolute median and p95 responsiveness
  ceilings; pull-request CI compares against the explicit PR base revision
  automatically.
- `relay-terminal-test` covers the terminal barrier, mixed-version relay paths,
  arbitrary nesting depth, burst preservation, and VS Code terminal handoff.
- `shell-test` covers direct and attached-client file-link classification plus
  shell-published tmux routing context.
- `test-isolation-test` verifies that the harness cannot consume live user
  session state.
- `tmux-focus-test` covers hierarchical focus ownership, leases, nested tmux,
  shared sessions, and pane-style restoration.
- `tmux-test` covers tmux command behavior and context publication.
- `vscode-adapter-test` drives the shipped VS Code controller and Unix-socket
  server through authentication, ordering, tab dispatch, focus transitions,
  lease expiry, idle-client timeout, activation, and cleanup.
- `wezterm-integration-test` covers WezTerm link routing, semantic eza links,
  shell integration, and Neovim opener edge cases.

Prefer fake terminal commands and fixture files over depending on an active
tmux, Neovim, SSH, or WezTerm session.

The VS Code socket-adapter runtime assertions use `node` when it is available;
local minimal environments may skip them, while the required dedicated Ubuntu
job guarantees they run in CI. The manifest, shell backend, and router coverage
still run without Node. The extension itself uses the runtime bundled with VS
Code.
