# shellcheck shell=bash
# Decide whether command output should contain plain paths or routed OSC-8 links.
#
# Return status is intentionally shell-friendly: success means the active
# terminal needs plain paths, while failure means it can route semantic file
# links. Consumers can therefore choose only presentation policy; Termnav keeps
# terminal/client discovery and its platform fallbacks in one reusable place.

_termnav_file_links_env_has_plain_marker() {
  local env_text
  env_text=$'\n'"$1"$'\n'
  case "$env_text" in
    *$'\nTERM_PROGRAM=vscode\n'* | *$'\nVSCODE_IPC_HOOK_CLI='* | *$'\nTERMNAV_FILE_LINKS_PLAIN=1\n'*)
      return 0
      ;;
  esac
  return 1
}

_termnav_file_links_env_has_wsl_marker() {
  local env_text
  env_text=$'\n'"$1"$'\n'
  case "$env_text" in
    *$'\nWSL_DISTRO_NAME='* | *$'\nWSL_INTEROP='*)
      return 0
      ;;
  esac
  return 1
}

_termnav_file_links_env_has_router() {
  local env_text
  env_text=$'\n'"$1"$'\n'
  case "$env_text" in
    *$'\nTERM_PROGRAM=WezTerm\n'* | *$'\nWEZTERM_PANE='*)
      return 0
      ;;
  esac
  return 1
}

_termnav_file_links_env_needs_plain_output() {
  local env_text="$1"

  # A positively identified Termnav-capable router wins over stale editor
  # variables inherited through SSH, tmux, or a long-lived shell.
  _termnav_file_links_env_has_router "$env_text" && return 1
  _termnav_file_links_env_has_plain_marker "$env_text" && return 0

  # Windows-side terminals do not reliably preserve VS Code markers when they
  # launch WSL. In an otherwise unmarked WSL client, file:// OSC-8 targets carry
  # Linux paths that the terminal generally cannot resolve as workspace files.
  # Keep semantic links only when a router such as WezTerm identifies itself.
  _termnav_file_links_env_has_wsl_marker "$env_text" && return 0

  return 1
}

_termnav_file_links_current_env_needs_plain_output() {
  local env_text
  env_text=$(
    [[ -n "${TERM_PROGRAM:-}" ]] && printf 'TERM_PROGRAM=%s\n' "$TERM_PROGRAM"
    [[ -n "${VSCODE_IPC_HOOK_CLI:-}" ]] && printf 'VSCODE_IPC_HOOK_CLI=%s\n' "$VSCODE_IPC_HOOK_CLI"
    [[ -n "${TERMNAV_FILE_LINKS_PLAIN:-}" ]] && printf 'TERMNAV_FILE_LINKS_PLAIN=%s\n' "$TERMNAV_FILE_LINKS_PLAIN"
    [[ -n "${WEZTERM_PANE:-}" ]] && printf 'WEZTERM_PANE=%s\n' "$WEZTERM_PANE"
    [[ -n "${WSL_DISTRO_NAME:-}" ]] && printf 'WSL_DISTRO_NAME=%s\n' "$WSL_DISTRO_NAME"
    [[ -n "${WSL_INTEROP:-}" ]] && printf 'WSL_INTEROP=%s\n' "$WSL_INTEROP"
  )

  _termnav_file_links_env_needs_plain_output "$env_text"
}

_termnav_file_links_filter_env() {
  # Never retain an attached process's whole environment in a shell variable.
  # Besides being unnecessary, expanded function arguments and assignments are
  # visible under `set -x`. Filter in an external process before shell state can
  # contain unrelated credentials or other private client variables.
  awk -F= '
    $1 == "TERM_PROGRAM" ||
    $1 == "VSCODE_IPC_HOOK_CLI" ||
    $1 == "TERMNAV_FILE_LINKS_PLAIN" ||
    $1 == "WEZTERM_PANE" ||
    $1 == "WSL_DISTRO_NAME" ||
    $1 == "WSL_INTEROP"
  '
}

_termnav_file_links_tmux_client_needs_plain_output() {
  [[ -n "${TMUX:-}" ]] || return 1

  local client_pid client_env proc_environ
  client_pid=$(tmux display-message -p '#{client_pid}' 2>/dev/null || true)
  [[ "$client_pid" =~ ^[0-9]+$ ]] || return 1

  proc_environ="${TERMNAV_FILE_LINKS_PROC_ROOT:-/proc}/$client_pid/environ"
  if [[ -r "$proc_environ" ]]; then
    client_env=$(tr '\0' '\n' <"$proc_environ" 2>/dev/null | _termnav_file_links_filter_env || true)
  else
    # macOS has no procfs. Inspect the attached tmux client process when
    # possible so a pane attached from VS Code is not misclassified from stale
    # pane variables, while an attached WezTerm client keeps rich links.
    client_env=$(ps eww -p "$client_pid" 2>/dev/null | tr ' ' '\n' | _termnav_file_links_filter_env || true)
  fi

  _termnav_file_links_env_needs_plain_output "$client_env"
}

_termnav_file_links_need_plain_output_uncached() {
  if [[ -n "${TMUX:-}" ]]; then
    _termnav_file_links_tmux_client_needs_plain_output
    return
  fi

  _termnav_file_links_current_env_needs_plain_output
}

termnav_file_links_need_plain_output() {
  local now="${SECONDS-}"

  if [[ "${_termnav_file_links_cache_disabled:-}" == 1 ]]; then
    _termnav_file_links_need_plain_output_uncached
    return
  fi

  # SECONDS is builtin in Bash and zsh, but callers can unset or replace it.
  # Without a numeric clock no TTL can expire, so preserve correctness by
  # bypassing the cache instead of turning the fallback value into a permanent
  # cache key. Keep that decision for the shell's lifetime: Bash permits an
  # unset SECONDS to be recreated as a numeric but non-advancing scalar, which
  # must not make an earlier cache entry permanent or reusable again.
  case "$now" in
    '' | *[!0-9]*)
      _termnav_file_links_cache_disabled=1
      _termnav_file_links_need_plain_output_uncached
      return
      ;;
    *) now=$((10#$now)) ;;
  esac

  # `ls` and `rg` can hit this path repeatedly. One shell-second keeps tmux
  # reattachment responsive without paying for tmux IPC plus process-environment
  # inspection on every command in a tight loop.
  if [[ "${_termnav_file_links_cache_second:-}" == "$now" &&
    -n "${_termnav_file_links_cache_status+x}" ]]; then
    return "$_termnav_file_links_cache_status"
  fi

  _termnav_file_links_need_plain_output_uncached
  _termnav_file_links_cache_status=$?
  _termnav_file_links_cache_second="$now"
  return "$_termnav_file_links_cache_status"
}
