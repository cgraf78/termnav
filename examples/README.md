# Termnav examples

These examples compose Termnav's public APIs without copying its routing
protocols or assuming where Shdeps installed the repository. They use only
reserved `example.com` names and contain no deployment-specific host policy.

## WezTerm

[`wezterm.lua`](wezterm.lua) appends Termnav's public hyperlink rules and
registers its `open-uri` router while preserving an existing config table. A
typical `.wezterm.lua` can use it like this:

```lua
local wezterm = require("wezterm")
local config = wezterm.config_builder()
local example = dofile(wezterm.home_dir .. "/.config/wezterm/termnav.lua")
example.apply({ wezterm = wezterm, config = config })
return config
```

The caller continues to own fonts, colors, domains, and key assignments.

## Neovim

Copy [`nvim.lua`](nvim.lua) into your Neovim config and call:

```lua
require("termnav").setup()
```

The wrapper finds Termnav's provider-owned setup module through Shdeps. Pass a
`termnav_options` table only when you need to change the provider's documented
events or collaborators.

## tmux

Source [`tmux.conf`](tmux.conf) from a larger tmux config:

```tmux
source-file ~/.config/tmux/termnav.conf
```

It provides a deliberately small starting point: Ctrl-Tab ownership across
tmux layers and safe Ctrl-click forwarding. More elaborate foreground-process
policy still belongs in the consuming tmux config.

## Custom Ctrl-click tokens

Copy
[`tmux-follow/extensions.d/example-ticket.sh`](tmux-follow/extensions.d/example-ticket.sh)
to
`$XDG_CONFIG_HOME/termnav/tmux-follow/extensions.d/` (normally
`~/.config/termnav/tmux-follow/extensions.d/`) and replace the synthetic ticket
format and URL. The detector validates the complete numeric suffix before it
claims a token, so malformed values continue through Termnav's normal routing.

## Interactive shell

Load the provider-owned shell integration directly; no wrapper file is needed:

```sh
. "$(shdeps dep-file cgraf78/termnav share/termnav/shell.sh)"
```

Hook that loader into the shell framework you already use. Termnav exposes the
callbacks, while the consumer owns when prompt hooks run. The same loader also
exposes `termnav_file_links_need_plain_output`; use its return status to choose
between a tool's plain-path output and its semantic OSC-8 mode without copying
terminal or tmux-client detection into shell aliases.

## Neovim command launcher

Consumers that wrap `nvim` can source
`lib/termnav/nvim-open/launcher.sh` through Shdeps and try the reusable route
before resolving their real editor:

```bash
. "$(shdeps dep-file cgraf78/termnav lib/termnav/nvim-open/launcher.sh)"
if termnav_nvim_try_reuse "$@"; then
  exit 0
fi
exec /path/to/real/nvim "$@"
```

The helper deliberately returns nonzero for editor-driven calls, shell scripts,
flags, multiple targets, non-tmux sessions, and failed routing. The wrapper
therefore retains ownership of real-editor discovery and blocking behavior.
