# Test Harness

`test/termnav-test` is the complete local and CI entrypoint. It loads shared
helpers from `test/helpers.sh`, uses fixtures under `test/fixtures/`, and runs
focused suites from `test/suites/`.

Suite ownership follows the integration boundary:

- `nvim-test` covers Neovim helper behavior.
- `nvim-launcher-test` covers the conservative sourceable policy for reusing an
  existing Neovim pane from simple interactive-shell file opens.
- `remote-test` covers remote open routing.
- `relay-test` covers nested SSH transport, directive storage, and in-band
  commit behavior.
- `relay-performance-test` keeps production `send` and `commit` dispatch on
  the lightweight path. Set `TERMNAV_PERFORMANCE_BASELINE` to a Git revision
  to compare alternating subprocess samples and enforce calibrated median and
  p95 non-regression budgets; pull-request CI compares against the explicit PR
  base revision automatically.
- `shell-test` covers direct and attached-client file-link classification plus
  shell-published tmux routing context.
- `tmux-test` covers tmux command behavior.
- `tab-switch-test` covers nearest-scope tmux traversal and terminal
  bridge selection.
- `vscode-test` covers VS Code command backends.
- `wezterm-test` covers WezTerm link rules.
- `install-test` covers the standalone checkout-backed command and manpage
  links, idempotent retargeting, custom destinations, complete source
  preflight, and refusal to overwrite user-owned paths.

Prefer fake terminal commands and fixture files over depending on an active
tmux, Neovim, SSH, or WezTerm session.

The VS Code socket-adapter runtime assertions use `node` when it is available;
the manifest, shell backend, and router coverage still run without it. The
extension itself uses the Node runtime bundled with VS Code.
