# termnav

`termnav` owns terminal navigation helpers: WezTerm link routing, tmux
ctrl-click follow-through, OSC-8-aware `eza` links, and `nvim-tmux-open`.

## Public API

- `bin/nvim-tmux-open`: open local or remote terminal targets in Neovim.
- `bin/tmux-follow-click`: resolve tmux mouse clicks to URL/file actions.
- `bin/eza-nvim-links`: run `eza` with remote-aware file hyperlinks.
- `lib/termnav/wezterm/link-routes.lua`: WezTerm route handlers.
- `lib/termnav/wezterm/public-link-rules.lua`: public WezTerm link rules
  for terminal tokens such as localhost URLs, git SSH remotes, CVEs, and RFCs.
  The module returns fresh rule copies from `public_link_rules()` and appends
  them to an existing WezTerm `hyperlink_rules` table with
  `add_public_link_rules(rules)`.
- `lib/termnav/nvim/*.lua`: Neovim-side terminal navigation helpers.
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
- WezTerm for the Lua link-route modules and OSC user-variable context
  publishing. The tmux and Neovim helpers remain useful without WezTerm when a
  consumer invokes them directly.
- `eza` for `eza-nvim-links`.
- `nvim-ssh-control-open` and `nvim-remote-pane-host` are optional extension
  commands for remote-pane workflows.

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

Run tests with:

```bash
./test/termnav-test
```

## License

MIT. See [`LICENSE`](LICENSE).
