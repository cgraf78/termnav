# shellcheck shell=bash
# Helpers for publishing WezTerm OSC 1337 SetUserVar requests.

# A shell may source this library again after its tmux context changes. Reset the
# memoized client classification so re-sourcing remains an explicit refresh.
_termnav_wezterm_tmux_client_cache_set=0
_termnav_wezterm_tmux_client_cache_key=""
_termnav_wezterm_tmux_client_cache_nested=0
_termnav_wezterm_tmux_client_cache_at=0
# Remote-host discovery may classify the client first. Keep that observation
# only for the surrounding publish, including failed and clockless queries.
_termnav_wezterm_tmux_publish_observation_key=""
_termnav_wezterm_tmux_publish_observation_nested=""

termnav_wezterm_base64() {
  # Empty values are common when clearing user vars. Their base64 encoding is
  # also empty, so avoid two subprocesses on this prompt-time path.
  [[ -n "$1" ]] || return 0
  printf '%s' "$1" | base64 | tr -d '\n'
}

termnav_wezterm_tmux_client_is_nested() {
  local age termname nested=0 now="${SECONDS-}" tmux_key="${TMUX:-}"

  [[ -n "$tmux_key" ]] || return 1
  case "$now" in
    '' | *[!0-9]*) now="" ;;
    *) now=$((10#$now)) ;;
  esac
  if [[ -n "$now" && "${_termnav_wezterm_tmux_client_cache_set:-0}" == 1 &&
    "${_termnav_wezterm_tmux_client_cache_key-}" == "$tmux_key" ]]; then
    age=$((now - _termnav_wezterm_tmux_client_cache_at))
    if ((age >= 0 && age < 5)); then
      [[ "${_termnav_wezterm_tmux_client_cache_nested:-0}" == 1 ]]
      return
    fi
  fi

  # Do not immediately repeat a query already attempted by this publish. Empty
  # means failed or unknown and remains uncached after the publish boundary.
  if [[ "${_termnav_wezterm_tmux_publish_observation_key-}" == "$tmux_key" ]]; then
    [[ "${_termnav_wezterm_tmux_publish_observation_nested-}" == 1 ]]
    return
  fi

  # A failed or empty query is not client state. Leave it uncached so a tmux
  # server or client that is still attaching can recover on the next publish.
  termname=$(tmux display-message -p "#{client_termname}" 2>/dev/null) || return 1
  [[ -n "$termname" ]] || return 1
  case "$termname" in
    tmux* | screen*) nested=1 ;;
  esac

  if [[ -n "$now" ]]; then
    _termnav_wezterm_tmux_client_cache_set=1
    _termnav_wezterm_tmux_client_cache_key="$tmux_key"
    _termnav_wezterm_tmux_client_cache_nested=$nested
    _termnav_wezterm_tmux_client_cache_at=$now
  else
    # Without a usable shell clock, prefer a fresh query over an observation
    # that could otherwise remain stale for the lifetime of the shell.
    _termnav_wezterm_tmux_client_cache_set=0
  fi

  [[ "$nested" == 1 ]]
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
