# termnav Libraries

This directory contains reusable shell and Lua integration code behind the
installed commands.

## Areas

- `nvim-link-context.sh` captures link context shared by commands.
- `nvim-open/` owns shell routing for local and remote open requests, including
  the conservative launcher predicate that reuses an editor pane only for a
  simple file open typed at an interactive prompt.
- `nvim/` owns Neovim-side Lua helpers.
- `shell/` owns reusable shell helpers for terminal control protocols and the
  rich-versus-plain file-link decision for the active terminal or tmux client.
- `termnav-tmux-context` publishes tmux ownership when a terminal client
  attaches, before any shell or editor redraw can refresh terminal metadata.
- `wezterm/` owns WezTerm link routing and public link rules.

Keep terminal-specific behavior in the closest integration directory, but keep
shared route vocabulary centralized so tmux, Neovim, and WezTerm agree on link
shape and target semantics.
