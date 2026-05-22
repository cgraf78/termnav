# shellcheck shell=bash
# termnav shell integration.
#
# Source this file from an interactive shell to publish terminal-navigation
# context for WezTerm, tmux, SSH, and remote file-link helpers:
#   . "$(shdeps dep-file cgraf78/termnav share/termnav/shell.sh)"

# shellcheck disable=SC2034 # public marker for callers that verify the loader ran.
TERMNAV_SHELL_LOADED=1

# ---------------------------------------------------------------------------
# Public API - stable shell integration surface
# ---------------------------------------------------------------------------
# _termnav_wezterm_publish_link_context
#   Publish the current pane cwd and remote-host context for terminal links.
# _termnav_wezterm_preexec <command>
#   Prompt hook callback that marks nvim/vim commands before they start.
# _termnav_wezterm_precmd
#   Prompt hook callback that marks the pane as no longer inside nvim/vim.

_termnav_wezterm_active() {
  [[ -z "${NVIM:-}" ]] &&
    [[ -n "${TMUX:-}" ||
      "${TERM_PROGRAM:-}" == "WezTerm" ||
      -n "${WEZTERM_PANE:-}" ||
      -n "${SSH_CONNECTION:-}" ]]
}

_termnav_wezterm_set_user_var() {
  local encoded
  case "$1" in
    IS_NVIM)
      [[ "${_termnav_wezterm_sent_IS_NVIM:-0}" == 1 && "${_termnav_wezterm_last_IS_NVIM-}" == "$2" ]] && return
      _termnav_wezterm_sent_IS_NVIM=1
      _termnav_wezterm_last_IS_NVIM="$2"
      ;;
    NVIM_LINK_CWD)
      [[ "${_termnav_wezterm_sent_NVIM_LINK_CWD:-0}" == 1 && "${_termnav_wezterm_last_NVIM_LINK_CWD-}" == "$2" ]] && return
      _termnav_wezterm_sent_NVIM_LINK_CWD=1
      _termnav_wezterm_last_NVIM_LINK_CWD="$2"
      ;;
    NVIM_REMOTE_LINK_HOST)
      [[ "${_termnav_wezterm_sent_NVIM_REMOTE_LINK_HOST:-0}" == 1 && "${_termnav_wezterm_last_NVIM_REMOTE_LINK_HOST-}" == "$2" ]] && return
      _termnav_wezterm_sent_NVIM_REMOTE_LINK_HOST=1
      _termnav_wezterm_last_NVIM_REMOTE_LINK_HOST="$2"
      ;;
    NVIM_REMOTE_CWD)
      [[ "${_termnav_wezterm_sent_NVIM_REMOTE_CWD:-0}" == 1 && "${_termnav_wezterm_last_NVIM_REMOTE_CWD-}" == "$2" ]] && return
      _termnav_wezterm_sent_NVIM_REMOTE_CWD=1
      _termnav_wezterm_last_NVIM_REMOTE_CWD="$2"
      ;;
    NVIM_REMOTE_TMUX)
      [[ "${_termnav_wezterm_sent_NVIM_REMOTE_TMUX:-0}" == 1 && "${_termnav_wezterm_last_NVIM_REMOTE_TMUX-}" == "$2" ]] && return
      _termnav_wezterm_sent_NVIM_REMOTE_TMUX=1
      _termnav_wezterm_last_NVIM_REMOTE_TMUX="$2"
      ;;
  esac

  encoded=$(printf '%s' "$2" | base64 | tr -d '\n')

  # shellcheck disable=SC1003 # \033\\ is ESC+backslash (ST), not a quote escape.
  if [[ -n "${TMUX:-}" ]]; then
    printf '\ePtmux;\e\033]1337;SetUserVar=%s=%s\007\e\\' "$1" "$encoded"
  else
    printf '\033]1337;SetUserVar=%s=%s\007' "$1" "$encoded"
  fi
}

_termnav_wezterm_remote_link_host() {
  local tmux_value

  if [[ -n "${NVIM_REMOTE_LINK_HOST:-}" ]]; then
    printf '%s\n' "$NVIM_REMOTE_LINK_HOST"
    return
  fi

  if [[ -n "${TMUX:-}" ]]; then
    # Some persistent remote transports do not expose SSH_CONNECTION. Their
    # landing helpers can seed tmux's global env so already-running shells can
    # still publish the same host identity that local link routing uses.
    tmux_value=$(tmux show-environment -g NVIM_REMOTE_LINK_HOST 2>/dev/null) || tmux_value=""
    if [[ "$tmux_value" == NVIM_REMOTE_LINK_HOST=* ]]; then
      tmux_value="${tmux_value#NVIM_REMOTE_LINK_HOST=}"
      if [[ -n "$tmux_value" ]]; then
        printf '%s\n' "$tmux_value"
        return
      fi
    fi
  fi

  if [[ -n "${SSH_CONNECTION:-}" ]]; then
    hostname -s 2>/dev/null || hostname 2>/dev/null || true
  fi
}

_termnav_wezterm_publish_link_context() {
  local remote_host
  remote_host="$(_termnav_wezterm_remote_link_host)"
  # Hyperlink-aware tools run as shell children, so export the same host that
  # WezTerm receives via user vars. Host-specific config can override it.
  if [[ -n "$remote_host" && -z "${NVIM_REMOTE_LINK_HOST:-}" ]]; then
    export NVIM_REMOTE_LINK_HOST="$remote_host"
  fi

  # Regex fallback links need cwd from the producing pane. WezTerm's process
  # cwd is not reliable once tmux, SSH, or nvim is involved.
  _termnav_wezterm_set_user_var NVIM_LINK_CWD "$PWD"
  _termnav_wezterm_set_user_var NVIM_REMOTE_LINK_HOST "$remote_host"
  if [[ -n "$remote_host" ]]; then
    _termnav_wezterm_set_user_var NVIM_REMOTE_CWD "$PWD"
    if [[ -n "${TMUX:-}" ]]; then
      # Direct pane routing sends a tmux command through the visible terminal.
      # Advertise that only when the remote producer is actually inside tmux.
      _termnav_wezterm_set_user_var NVIM_REMOTE_TMUX true
    else
      _termnav_wezterm_set_user_var NVIM_REMOTE_TMUX ""
    fi
  else
    _termnav_wezterm_set_user_var NVIM_REMOTE_CWD ""
    _termnav_wezterm_set_user_var NVIM_REMOTE_TMUX ""
  fi
}

_termnav_wezterm_preexec() {
  _termnav_wezterm_publish_link_context
  case "$1" in
    nvim* | vim*) _termnav_wezterm_set_user_var IS_NVIM true ;;
  esac
}

_termnav_wezterm_precmd() {
  _termnav_wezterm_set_user_var IS_NVIM false
  _termnav_wezterm_publish_link_context
}

_termnav_wezterm_register_hooks() {
  if [ -n "${ZSH_VERSION:-}" ]; then
    autoload -Uz add-zsh-hook
    add-zsh-hook -d preexec _termnav_wezterm_preexec 2>/dev/null || true
    add-zsh-hook -d precmd _termnav_wezterm_precmd 2>/dev/null || true
    add-zsh-hook -d chpwd _termnav_wezterm_publish_link_context 2>/dev/null || true
    add-zsh-hook preexec _termnav_wezterm_preexec
    add-zsh-hook precmd _termnav_wezterm_precmd
    add-zsh-hook chpwd _termnav_wezterm_publish_link_context
  elif [ -n "${BASH_VERSION:-}" ]; then
    # bash-preexec convention; 70-integrations.bash loads the shim inside tmux.
    # shellcheck disable=SC2179 # append to array, not string.
    [[ " ${preexec_functions[*]} " != *" _termnav_wezterm_preexec "* ]] && preexec_functions+=(_termnav_wezterm_preexec)
    # shellcheck disable=SC2179
    [[ " ${precmd_functions[*]} " != *" _termnav_wezterm_precmd "* ]] && precmd_functions+=(_termnav_wezterm_precmd)
  fi
}

if _termnav_wezterm_active; then
  _termnav_wezterm_register_hooks
  _termnav_wezterm_publish_link_context
fi
