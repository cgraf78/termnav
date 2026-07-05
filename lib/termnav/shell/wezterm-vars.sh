# shellcheck shell=bash
# Helpers for publishing WezTerm OSC 1337 SetUserVar requests.

termnav_wezterm_base64() {
  printf '%s' "$1" | base64 | tr -d '\n'
}

termnav_wezterm_tmux_client_is_nested() {
  local termname

  [[ -n "${TMUX:-}" ]] || return 1
  termname=$(tmux display-message -p "#{client_termname}" 2>/dev/null) || return 1
  case "$termname" in
    tmux* | screen*) return 0 ;;
    *) return 1 ;;
  esac
}

termnav_wezterm_tmux_passthrough() {
  local sequence="$1"
  local esc=$'\033'

  sequence=${sequence//"$esc"/"$esc$esc"}
  # shellcheck disable=SC1003 # \033\\ is ESC+backslash (ST), not a quote escape.
  printf '\033Ptmux;%s\033\\' "$sequence"
}

termnav_wezterm_user_var_sequence() {
  local name="$1" value="${2:-}" mode="${3:-auto}"
  local encoded raw sequence

  encoded=$(termnav_wezterm_base64 "$value")
  raw=$(printf '\033]1337;SetUserVar=%s=%s\007' "$name" "$encoded")

  case "$mode" in
    raw)
      printf '%s' "$raw"
      ;;
    tmux | passthrough)
      termnav_wezterm_tmux_passthrough "$raw"
      ;;
    auto)
      if [[ -n "${TMUX:-}" ]]; then
        sequence=$(termnav_wezterm_tmux_passthrough "$raw")
        if termnav_wezterm_tmux_client_is_nested; then
          termnav_wezterm_tmux_passthrough "$sequence"
        else
          printf '%s' "$sequence"
        fi
      else
        printf '%s' "$raw"
      fi
      ;;
    *)
      printf 'termnav_wezterm_user_var_sequence: unknown mode: %s\n' "$mode" >&2
      return 2
      ;;
  esac
}

termnav_wezterm_set_user_var() {
  local name="$1" value="${2:-}" tty="${3:-/dev/tty}" mode="${4:-auto}"

  [[ -n "$tty" ]] || {
    printf 'termnav_wezterm_set_user_var: tty is empty\n' >&2
    return 2
  }

  termnav_wezterm_user_var_sequence "$name" "$value" "$mode" >"$tty"
}
