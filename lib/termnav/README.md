# termnav Libraries

This directory contains reusable shell and Lua integration code behind the
installed commands.

## Areas

- `nvim-link-context.sh` captures link context shared by commands.
- `nvim-open/` owns shell routing for local and remote open requests.
- `nvim/` owns Neovim-side Lua helpers.
- `wezterm/` owns WezTerm link routing and public link rules.

Keep terminal-specific behavior in the closest integration directory, but keep
shared route vocabulary centralized so tmux, Neovim, and WezTerm agree on link
shape and target semantics.
