# Test Harness

`test/termnav-test` is the complete local and CI entrypoint. It loads shared
helpers from `test/helpers.sh`, uses fixtures under `test/fixtures/`, and runs
focused suites from `test/suites/`.

Suite ownership follows the integration boundary:

- `nvim-test` covers Neovim helper behavior.
- `remote-test` covers remote open routing.
- `tmux-test` covers tmux command behavior.
- `tab-switch-test` covers nearest-scope tmux traversal and terminal
  bridge selection.
- `vscode-test` covers VS Code command backends.
- `wezterm-test` covers WezTerm link rules.

Prefer fake terminal commands and fixture files over depending on an active
tmux, Neovim, SSH, or WezTerm session.

The VS Code socket-adapter runtime assertions use `node` when it is available;
the manifest, shell backend, and router coverage still run without it. The
extension itself uses the Node runtime bundled with VS Code.
