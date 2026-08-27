# shellcheck shell=bash
# termnav shell integration.
#
# Source this file from an interactive shell to publish terminal-navigation
# context for WezTerm, tmux, SSH, and remote file-link helpers:
#   . "$(shdeps dep-file cgraf78/termnav share/termnav/shell.sh)"

# shellcheck disable=SC2034 # public marker for callers that verify the loader ran.
TERMNAV_SHELL_LOADED=1

_termnav_shell_script_parent() {
  case "$1" in
    */*) printf '%s\n' "${1%/*}" ;;
    *) printf '.\n' ;;
  esac
}

_termnav_shell_script_dir() {
  local path="$1" dir target
  while [[ -L "$path" ]]; do
    dir=$(cd -P -- "$(_termnav_shell_script_parent "$path")" && pwd) || return 1
    target=$(readlink "$path") || return 1
    [[ "$target" == /* ]] || target="$dir/$target"
    path="$target"
  done
  cd -P -- "$(_termnav_shell_script_parent "$path")" && pwd
}

_termnav_shell_source_path="$0"
if [[ -n "${ZSH_VERSION:-}" ]]; then
  eval '_termnav_shell_source_path="${(%):-%x}"'
elif [[ -n "${BASH_SOURCE[0]:-}" ]]; then
  _termnav_shell_source_path="${BASH_SOURCE[0]}"
fi
_termnav_shell_dir=$(_termnav_shell_script_dir "$_termnav_shell_source_path") || return 1
_termnav_root="${_termnav_shell_dir%/share/termnav}"
_termnav_ssh_shim_dir="$_termnav_root/share/termnav/shims"

# A shell function affects only commands parsed by that shell. PATH is the
# process-level interface, so unrelated descendant launchers inherit the same
# SSH route without knowing about Termnav. Keep activation idempotent because
# dotfiles and other consumers may reload this file in place.
case ":${PATH:-}:" in
  *":$_termnav_ssh_shim_dir:"*) ;;
  *) PATH="$_termnav_ssh_shim_dir${PATH:+:$PATH}" ;;
esac
export PATH

# Warm the shared dispatcher without making shell startup wait for Python. The
# resident service performs the old forwarded-socket sweep once while starting,
# then removes interpreter startup from every later navigation and commit
# gesture. Use the provider beside this asset so development and installed
# generations never share implementation accidentally.
# Repository tests source this integration under many disposable HOME values.
# Their dedicated service suite owns lifecycle coverage; suppressing activation
# here prevents test-only runtimes from leaving detached processes behind.
if [[ "${REPO_TEST:-0}" != 1 && "${DOT_TEST:-0}" != 1 ]]; then
  "$_termnav_root/bin/termnav-relay" warm >/dev/null 2>&1 || true
fi

# Prompt hooks share this cache in the current shell. Re-sourcing the integration
# intentionally refreshes it, matching the low-level tmux-client cache.
_termnav_wezterm_remote_link_host_cache_set=0
_termnav_wezterm_remote_link_host_cache_tmux=""
_termnav_wezterm_remote_link_host_cache_ssh=""
_termnav_wezterm_remote_link_host_cache_at=0
_termnav_wezterm_remote_link_host_cache_value=""
_termnav_wezterm_remote_link_host_result=""

# ---------------------------------------------------------------------------
# Public API - stable shell integration surface
# ---------------------------------------------------------------------------
# _termnav_wezterm_publish_link_context
#   Publish the current pane cwd and remote-host context for terminal links.
# _termnav_wezterm_preexec <command>
#   Prompt hook callback that marks nvim/vim commands before they start.
# _termnav_wezterm_precmd
#   Prompt hook callback that marks the pane as no longer inside nvim/vim.
# termnav_file_links_need_plain_output
#   Return success when command output should use plain paths instead of OSC-8
#   file links for the current terminal or attached tmux client.

# shellcheck source=../../lib/termnav/shell/wezterm-vars.sh
. "$_termnav_root/lib/termnav/shell/wezterm-vars.sh" || return 1
# shellcheck source=../../lib/termnav/shell/file-links.sh
. "$_termnav_root/lib/termnav/shell/file-links.sh" || return 1

_termnav_wezterm_active() {
  [[ -z "${NVIM:-}" ]] &&
    [[ -n "${TMUX:-}" ||
      "${TERM_PROGRAM:-}" == "WezTerm" ||
      -n "${WEZTERM_PANE:-}" ||
      -n "${SSH_CONNECTION:-}" ]]
}

_termnav_wezterm_set_user_var() {
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

  termnav_wezterm_user_var_sequence "$1" "$2" auto
}

_termnav_wezterm_publish_tmux_context() {
  _termnav_wezterm_active || return 0

  # TMUX cannot change during one shell process's lifetime, but a child tmux or
  # nested shell can overwrite this pane-scoped WezTerm variable. Reassert the
  # current shell's value whenever control returns to its prompt. This emits one
  # local OSC sequence and performs no tmux process query.
  if [[ -n "${TMUX:-}" ]]; then
    _termnav_wezterm_set_user_var TERMNAV_TMUX true
  else
    _termnav_wezterm_set_user_var TERMNAV_TMUX ""
  fi
}

_termnav_wezterm_remote_link_host_get() {
  local age fields nested=0 now="${SECONDS-}" parsed termname

  _termnav_wezterm_remote_link_host_result=""

  # If a caller replaced a value previously exported by Termnav, ownership has
  # transferred to the caller and the new value is an explicit override.
  if [[ -n "${_TERMNAV_REMOTE_LINK_HOST_DISCOVERED:-}" &&
    "${TERMNAV_REMOTE_LINK_HOST:-}" != "$_TERMNAV_REMOTE_LINK_HOST_DISCOVERED" ]]; then
    unset _TERMNAV_REMOTE_LINK_HOST_DISCOVERED
  fi
  if [[ -n "${TERMNAV_REMOTE_LINK_HOST:-}" &&
    -z "${_TERMNAV_REMOTE_LINK_HOST_DISCOVERED:-}" ]]; then
    _termnav_wezterm_remote_link_host_result="$TERMNAV_REMOTE_LINK_HOST"
    return
  fi

  # Remote identity changes far less often than prompt hooks run. Keep negative
  # results too, but only briefly so a persistent transport can seed tmux state
  # after this shell has started. SECONDS is builtin in Bash 3.2 and zsh.
  case "$now" in
    '' | *[!0-9]*) now="" ;;
    *) now=$((10#$now)) ;;
  esac
  if [[ -n "$now" && "${_termnav_wezterm_remote_link_host_cache_set:-0}" == 1 &&
    "${_termnav_wezterm_remote_link_host_cache_tmux-}" == "${TMUX:-}" &&
    "${_termnav_wezterm_remote_link_host_cache_ssh-}" == "${SSH_CONNECTION:-}" ]]; then
    age=$((now - _termnav_wezterm_remote_link_host_cache_at))
    if ((age >= 0 && age < 5)); then
      _termnav_wezterm_remote_link_host_result="$_termnav_wezterm_remote_link_host_cache_value"
      return
    fi
  fi

  if [[ -n "${TMUX:-}" ]]; then
    # Some persistent remote transports do not expose SSH_CONNECTION. Their
    # landing helpers can seed tmux's global env so already-running shells can
    # still publish the same host identity that local link routing uses. Read
    # client classification in the same call and prime its ordinary TTL cache.
    _termnav_wezterm_tmux_publish_observation_key="${TMUX:-}"
    _termnav_wezterm_tmux_publish_observation_nested=""
    fields=$(tmux display-message -p 't#{client_termname}' \; show-environment -g TERMNAV_REMOTE_LINK_HOST 2>/dev/null) || true
    case "$fields" in
      t*)
        parsed="${fields#t}"
        termname="${parsed%%$'\n'*}"
        if [[ "$fields" == *$'\n'TERMNAV_REMOTE_LINK_HOST=* ]]; then
          _termnav_wezterm_remote_link_host_result="${fields#*$'\n'TERMNAV_REMOTE_LINK_HOST=}"
        fi
        if [[ -n "$termname" ]]; then
          case "$termname" in
            tmux* | screen*) nested=1 ;;
          esac
          _termnav_wezterm_tmux_publish_observation_nested=$nested
          if [[ -n "$now" ]]; then
            _termnav_wezterm_tmux_client_cache_set=1
            _termnav_wezterm_tmux_client_cache_key="${TMUX:-}"
            _termnav_wezterm_tmux_client_cache_nested=$nested
            _termnav_wezterm_tmux_client_cache_at=$now
          fi
        fi
        ;;
    esac
  fi

  if [[ -z "$_termnav_wezterm_remote_link_host_result" && -n "${SSH_CONNECTION:-}" ]]; then
    _termnav_wezterm_remote_link_host_result=$(hostname -s 2>/dev/null || hostname 2>/dev/null || true)
  fi

  if [[ -n "$now" ]]; then
    _termnav_wezterm_remote_link_host_cache_set=1
    _termnav_wezterm_remote_link_host_cache_tmux="${TMUX:-}"
    _termnav_wezterm_remote_link_host_cache_ssh="${SSH_CONNECTION:-}"
    _termnav_wezterm_remote_link_host_cache_at=$now
    _termnav_wezterm_remote_link_host_cache_value="$_termnav_wezterm_remote_link_host_result"
  else
    # An unusable clock cannot bound staleness, so retain correctness by querying
    # again rather than turning this short cache into a permanent one.
    _termnav_wezterm_remote_link_host_cache_set=0
  fi
}

_termnav_wezterm_remote_link_host() {
  _termnav_wezterm_tmux_publish_observation_key=""
  _termnav_wezterm_remote_link_host_get
  if [[ -n "$_termnav_wezterm_remote_link_host_result" ]]; then
    printf '%s\n' "$_termnav_wezterm_remote_link_host_result"
  fi
  _termnav_wezterm_tmux_publish_observation_key=""
  return 0
}

_termnav_wezterm_publish_link_context() {
  local publish_tmux_context="${1:-}" remote_host
  _termnav_wezterm_tmux_publish_observation_key=""
  _termnav_wezterm_remote_link_host_get
  remote_host="$_termnav_wezterm_remote_link_host_result"
  # Hyperlink-aware tools run as shell children, so export the same host that
  # WezTerm receives via user vars. Track values exported here separately so
  # cache expiry and tmux changes may refresh them without overriding users.
  if [[ -n "${_TERMNAV_REMOTE_LINK_HOST_DISCOVERED:-}" &&
    "${TERMNAV_REMOTE_LINK_HOST:-}" == "$_TERMNAV_REMOTE_LINK_HOST_DISCOVERED" ]]; then
    if [[ -n "$remote_host" ]]; then
      export TERMNAV_REMOTE_LINK_HOST="$remote_host"
      export _TERMNAV_REMOTE_LINK_HOST_DISCOVERED="$remote_host"
    else
      unset TERMNAV_REMOTE_LINK_HOST _TERMNAV_REMOTE_LINK_HOST_DISCOVERED
    fi
  elif [[ -n "$remote_host" && -z "${TERMNAV_REMOTE_LINK_HOST:-}" ]]; then
    export TERMNAV_REMOTE_LINK_HOST="$remote_host"
    export _TERMNAV_REMOTE_LINK_HOST_DISCOVERED="$remote_host"
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
  if [[ "$publish_tmux_context" == with-tmux-context ]]; then
    # Keep this initial publication inside the same observation boundary as
    # remote-host discovery. Even a failed tmux query is then shared instead of
    # immediately repeated merely to frame one more user variable.
    _termnav_wezterm_publish_tmux_context
  fi
  _termnav_wezterm_tmux_publish_observation_key=""
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
  _termnav_wezterm_publish_tmux_context
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
  _termnav_wezterm_publish_link_context with-tmux-context
fi
