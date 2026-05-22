# shellcheck shell=bash

nvim_open_parse_link_target() {
  local input="$1"

  if [[ "$input" =~ ^(.+):([0-9]+):([0-9]+)$ ]]; then
    nvim_open_file="${BASH_REMATCH[1]}"
    nvim_open_line="${BASH_REMATCH[2]}"
    nvim_open_col="${BASH_REMATCH[3]}"
  elif [[ "$input" =~ ^(.+):([0-9]+)$ ]]; then
    nvim_open_file="${BASH_REMATCH[1]}"
    nvim_open_line="${BASH_REMATCH[2]}"
    nvim_open_col=0
  else
    nvim_open_file="$input"
    nvim_open_line=1
    nvim_open_col=0
  fi

  case "$nvim_open_line" in *[!0-9]* | "") nvim_open_line=1 ;; esac
  case "$nvim_open_col" in *[!0-9]* | "") nvim_open_col=0 ;; esac

  # Terminal links often point at diff output paths. Strip only in link mode;
  # CLI mode must preserve argv literally.
  nvim_open_file="${nvim_open_file#a/}"
  nvim_open_file="${nvim_open_file#b/}"

  # Expand leading tilde and literal home references from terminal output.
  # shellcheck disable=SC2088 # matching literal ~ from terminal output, not shell expansion
  [[ "$nvim_open_file" == "~/"* ]] && nvim_open_file="$HOME/${nvim_open_file#"~/"}"
  nvim_open_file=$(nvim_open_expand_home_ref "$nvim_open_file")
}

nvim_open_link() {
  local input="${1:-}" base_cwd="${2:-}" source="${3:-terminal}" context_arg="${4:-}"
  local preferred_socket="$context_arg" remote_host="" tmux_panes tmux_rc
  local control_rc

  [[ -n "$input" ]] || return 0
  if [[ "$source" == "remote" ]]; then
    remote_host="$context_arg"
    preferred_socket=""
  fi

  nvim_open_parse_link_target "$input"

  if [[ "$source" != "remote" ]]; then
    # If WezTerm knows the click came from a specific nvim pane, use that exact
    # server first. This avoids opening in another recently active nvim instance.
    if [[ "$source" == "nvim" ]] && nvim_open_socket "$preferred_socket" "$nvim_open_file" "$nvim_open_line" "$nvim_open_col" "$base_cwd" "$source"; then
      return 0
    fi

    if [[ -n "${TMUX:-}" ]]; then
      # A tmux click should only target nvim in the current tmux window. The
      # global RPC registry can point at a recently active editor in a different
      # tmux window/session, which is more surprising than failing clearly.
      nvim_open_current_window "$nvim_open_file" "$nvim_open_line" "$nvim_open_col" "$base_cwd" "$source" && return 0
      return 1
    fi

    # Outside tmux there is no pane-local window boundary, so the published
    # current editor socket remains the least surprising target.
    if nvim_open_rpc "$nvim_open_file" "$nvim_open_line" "$nvim_open_col" "$base_cwd" "$source"; then
      return 0
    fi
  fi

  if [[ "$source" == "remote" && -n "$remote_host" ]]; then
    if nvim_open_remote_controlmaster "$remote_host" "$input"; then
      return 0
    else
      control_rc=$?
    fi

    case "$control_rc" in
      10 | 12 | 13) ;;
      *) return 1 ;;
    esac
  fi

  tmux_panes=$(tmux list-panes -a -F '#{pane_id}	#{pane_current_command}	#{pane_start_command}	#{pane_pid}' 2>&1)
  tmux_rc=$?
  if [[ "$tmux_rc" -ne 0 ]]; then
    nvim_open_log "tmux-list-failed rc=$tmux_rc output=$tmux_panes"
    tmux_panes=""
  fi

  # Remote tmux fallback is only a transport for links that already came from a
  # remote context. A local click should fail, not type into an unrelated SSH pane.
  [[ "$source" == "remote" ]] || return 1
  nvim_open_remote_tmux_fallback "$remote_host" "$input" "$tmux_panes"
}

nvim_open_tmux_link() {
  local input="${1:-}" base_cwd="${2:-}" source="${3:-terminal}" context_arg="${4:-}"
  local message rc

  # `link` is the strict API: callers can observe failure and decide what to do.
  # `tmux-link` is for background tmux commands, where failure must be translated
  # before tmux exposes the shell command that produced it.
  nvim_open_link "$input" "$base_cwd" "$source" "$context_arg"
  rc=$?
  [[ "$rc" -eq 0 ]] && return 0

  if [[ -n "$input" ]]; then
    message=$(nvim_open_click_failure_message "$input" "$source" "$context_arg")
  else
    message="No file link target was provided"
  fi
  [[ "$rc" -eq 1 ]] || message="${message} (opener exit ${rc})"
  nvim_open_show_message "$message"
  return 0
}
