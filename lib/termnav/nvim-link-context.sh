# shellcheck shell=bash

nvim_link_context_tmux_remote_host() {
  local value

  [[ -n "${TMUX:-}" ]] || return 1
  value=$(tmux show-environment -g TERMNAV_REMOTE_LINK_HOST 2>/dev/null) || return 1
  [[ "$value" == TERMNAV_REMOTE_LINK_HOST=* ]] || return 1
  value="${value#TERMNAV_REMOTE_LINK_HOST=}"
  [[ -n "$value" ]] || return 1
  printf '%s\n' "$value"
}

nvim_link_context_ssh_remote_host() {
  [[ -n "${SSH_CONNECTION:-}" ]] || return 1
  hostname -s 2>/dev/null || hostname 2>/dev/null || true
}

nvim_link_context_remote_host() {
  # Return nothing for local sessions so tools such as eza can keep hostless
  # local file:// links while still sharing remote host discovery with ripgrep.
  if [[ -n "${TERMNAV_REMOTE_LINK_HOST:-}" ]]; then
    printf '%s\n' "$TERMNAV_REMOTE_LINK_HOST"
  elif nvim_link_context_tmux_remote_host; then
    :
  else
    nvim_link_context_ssh_remote_host
  fi
}

nvim_link_context_host() {
  # ripgrep requires a host token for its hyperlink template; give it a stable
  # local marker when there is no remote context to publish.
  if nvim_link_context_remote_host; then
    :
  else
    printf 'localhost\n'
  fi
}
