# shellcheck shell=bash

nvim_open_cli() {
  local file="${1:-}" cwd="${2:-}"

  [[ -n "$file" ]] || return 1
  nvim_open_current_window "$file" 1 0 "$cwd" "cli"
}
