# shellcheck shell=bash

termnav_remote_shell_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

termnav_remote_nvim_command() {
  local arg command

  # Remote tmux commands do not run a login shell. Preserve its configured PATH
  # first, then add common user install locations for stripped-down ssh/tmux
  # environments on Linux, macOS, and WSL.
  # shellcheck disable=SC2016 # PATH and HOME expand in the remote shell.
  command='PATH="$PATH:$HOME/.local/bin:$HOME/.local/share/mise/shims:/opt/homebrew/bin:/usr/local/bin"; export PATH; command -v nvim-tmux-open >/dev/null 2>&1 || { printf "%s\n" "termnav: nvim-tmux-open not found on remote PATH" >&2; exit 127; }; nvim-tmux-open'
  for arg in "$@"; do
    command+=" $(termnav_remote_shell_quote "$arg")"
  done
  printf '%s\n' "$command"
}
