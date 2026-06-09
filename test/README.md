# Test Harness

`test/termnav-test` is the complete local and CI entrypoint. It loads shared
helpers from `test/helpers.sh`, uses fixtures under `test/fixtures/`, and runs
focused suites from `test/suites/`.

Suite ownership follows the integration boundary:

- `nvim-test` covers Neovim helper behavior.
- `remote-test` covers remote open routing.
- `tmux-test` covers tmux command behavior.
- `wezterm-test` covers WezTerm link rules.

Prefer fake terminal commands and fixture files over depending on an active
tmux, Neovim, SSH, or WezTerm session.
