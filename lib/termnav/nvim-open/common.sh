# shellcheck shell=bash

nvim_open_resolve_state_home() {
  case "${XDG_STATE_HOME:-}" in
    /*) REPLY="$XDG_STATE_HOME" ;;
    *)
      if [[ -n "${HOME:-}" ]]; then
        REPLY="$HOME/.local/state"
      else
        REPLY=""
        return 1
      fi
      ;;
  esac
}

nvim_open_state_home() {
  nvim_open_resolve_state_home || return 1
  printf '%s\n' "$REPLY"
}

nvim_open_resolve_state_dir() {
  nvim_open_resolve_state_home || return 1
  REPLY="$REPLY/nvim-tmux-open"
}

nvim_open_state_dir() {
  nvim_open_resolve_state_dir || return 1
  printf '%s\n' "$REPLY"
}

nvim_open_resolve_log_file() {
  nvim_open_resolve_state_home || return 1
  REPLY="$REPLY/wezterm-nvim-open.log"
}

nvim_open_log_file() {
  nvim_open_resolve_log_file || return 1
  printf '%s\n' "$REPLY"
}

nvim_open_log() {
  local log_file
  nvim_open_resolve_log_file || return 0
  log_file="$REPLY"
  mkdir -p "${log_file%/*}" 2>/dev/null || true
  printf '%s helper %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$log_file" 2>/dev/null || true
}

nvim_open_vim_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/''/g")"
}

nvim_open_shell_quote() {
  # Quote path input for the remote shell. This protects file names while still
  # letting the command prefix use remote-side variables such as $HOME.
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

nvim_open_tmux_quote() {
  local value="$1"

  # tmux command prompts parse their own quoting before the shell sees the
  # command. Escape only tmux double-quote syntax; shell quoting is separate.
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '"%s"' "$value"
}

nvim_open_expand_home_ref() {
  local path="$1"

  # WezTerm regex links preserve shell-style references from source files.
  # Expand only the stable home aliases; never evaluate arbitrary shell text.
  if [[ "$path" == "\$HOME" ]]; then
    printf '%s\n' "$HOME"
  elif [[ "$path" == "\$HOME/"* ]]; then
    printf '%s/%s\n' "$HOME" "${path#\$HOME/}"
  elif [[ "$path" == "\${HOME}" ]]; then
    printf '%s\n' "$HOME"
  elif [[ "$path" == "\${HOME}/"* ]]; then
    printf '%s/%s\n' "$HOME" "${path#\$\{HOME\}/}"
  else
    printf '%s\n' "$path"
  fi
}

nvim_open_click_failure_message() {
  local target="$1" source="${2:-terminal}" context="${3:-}"

  if [[ "$source" == "remote" && -n "$context" ]]; then
    printf 'No nvim session found for %s: %s\n' "$context" "$target"
  else
    printf 'No nvim session found for file link: %s\n' "$target"
  fi
}

nvim_open_show_message() {
  local message="$1" popup_command

  if [[ -n "${TMUX:-}" ]] && command -v tmux >/dev/null 2>&1; then
    # Link-open failures are rare and easy to miss in the status line. Prefer a
    # popup when the running tmux supports it, but keep display-message as the
    # compatibility path so older tmux versions still get a concise error.
    popup_command="printf '%s\n\nPress Enter to close.\n' $(nvim_open_shell_quote "$message"); IFS= read -r _"
    tmux display-popup -E -w 72 -h 7 -T "nvim open" "$popup_command" 2>/dev/null && return 0
    tmux display-message "$message" 2>/dev/null && return 0
  fi

  printf '%s\n' "$message" >&2
}

nvim_open_socket() {
  local socket="$1" target_file="$2" target_line="$3" target_col="$4" target_cwd="$5" source="$6"
  local expr quoted_cwd quoted_file quoted_source

  [[ -n "$socket" && -S "$socket" ]] || return 1

  quoted_file=$(nvim_open_vim_quote "$target_file")
  quoted_cwd=$(nvim_open_vim_quote "$target_cwd")
  quoted_source=$(nvim_open_vim_quote "$source")
  expr="luaeval('_G.nvim_tmux_open(_A[1], _A[2], _A[3], _A[4], _A[5])', [${quoted_file}, ${target_line}, ${target_col}, ${quoted_cwd}, ${quoted_source}])"
  nvim --server "$socket" --remote-expr "$expr" >/dev/null 2>&1
}

_nvim_open_owner_committed() {
  local marker="$1" line
  local -a lines=()
  [[ -f "$marker" && ! -L "$marker" && -r "$marker" ]] || return 1
  while IFS= read -r line; do
    lines[${#lines[@]}]="$line"
    ((${#lines[@]} <= 1)) || return 1
  done <"$marker"
  ((${#lines[@]} == 1)) && [[ "${lines[0]}" == v2 ]]
}

_nvim_open_read_registry_record() {
  local record="$1" expected_owner="${2:-}" registry_root="$3" line
  local -a lines=()

  [[ -f "$record" && ! -L "$record" && -r "$record" ]] || return 1
  while IFS= read -r line; do
    lines[${#lines[@]}]="$line"
    ((${#lines[@]} <= 4)) || return 1
  done <"$record"
  ((${#lines[@]} == 4)) || return 1
  [[ "${lines[0]}" == v2 ]] || return 1
  ((${#lines[1]} == 20)) || return 1
  case "${lines[1]}" in *[!0-9]*) return 1 ;; esac
  case "${lines[2]}" in
    "" | "." | ".." | *[![:alnum:]_.-]*) return 1 ;;
  esac
  [[ -z "$expected_owner" || "${lines[2]}" == "$expected_owner" ]] || return 1
  [[ -n "${lines[3]}" ]] || return 1
  _nvim_open_owner_committed "$registry_root/owners/${lines[2]}" || return 1
  REPLY_SEQUENCE="${lines[1]}"
  REPLY_OWNER="${lines[2]}"
  REPLY_SOCKET="${lines[3]}"
}

_nvim_open_ordered_owner_records() {
  local owners_dir="$1" registry_root="$2" record sequence owner insert previous
  local LC_ALL=C
  local -a records=() sequences=() owners=()

  REPLY_RECORDS=()
  for record in "$owners_dir"/*; do
    [[ -e "$record" ]] || continue
    _nvim_open_read_registry_record "$record" "${record##*/}" "$registry_root" || continue
    sequence=$REPLY_SEQUENCE
    owner=$REPLY_OWNER
    insert=${#records[@]}
    while ((insert > 0)); do
      previous=$((insert - 1))
      if [[ "$sequence" > "${sequences[$previous]}" ||
        ("$sequence" == "${sequences[$previous]}" && "$owner" > "${owners[$previous]}") ]]; then
        records[insert]="${records[previous]}"
        sequences[insert]="${sequences[previous]}"
        owners[insert]="${owners[previous]}"
        insert=$previous
      else
        break
      fi
    done
    records[insert]="$record"
    sequences[insert]="$sequence"
    owners[insert]="$owner"
  done
  REPLY_RECORDS=("${records[@]}")
}

nvim_open_rpc() {
  local target_file="$1" target_line="$2" target_col="$3" target_cwd="$4" source="$5"
  local socket socket_file state_dir registry_root seen_sockets latest_file legacy_file
  local preferred_record="" new_activity=""
  local legacy_first=0
  local -a socket_files=() owner_records=() REPLY_RECORDS=()

  nvim_open_resolve_state_dir || return 1
  state_dir="$REPLY"
  registry_root="$state_dir/registry"

  latest_file="$registry_root/current/latest"
  legacy_file="$state_dir/current"

  if _nvim_open_read_registry_record "$latest_file" "" "$registry_root"; then
    preferred_record="$latest_file"
    new_activity="$latest_file"
  fi
  _nvim_open_ordered_owner_records "$registry_root/current/owners" "$registry_root"
  owner_records=("${REPLY_RECORDS[@]}")
  if [[ -z "$new_activity" ]]; then
    new_activity=${owner_records[0]:-}
  fi
  # A tie cannot establish which format was written last. Prefer the legacy
  # record during rolling upgrades so a stale new-format record cannot win.
  if [[ -e "$legacy_file" && (-z "$new_activity" || ! "$new_activity" -nt "$legacy_file") ]]; then
    socket_files+=("$legacy_file")
    legacy_first=1
  fi
  if [[ -n "$preferred_record" ]]; then
    socket_files+=("$preferred_record")
  fi
  for socket_file in "${owner_records[@]}"; do
    socket_files+=("$socket_file")
  done

  # Read the legacy singleton registry during rolling upgrades. New publishers
  # use owner-scoped records and never create these paths.
  [[ "$legacy_first" == 1 ]] || socket_files+=("$legacy_file")
  for socket_file in "$state_dir"/panes/*; do
    [[ -e "$socket_file" ]] && socket_files+=("$socket_file")
  done

  seen_sockets=$'\n'
  for socket_file in "${socket_files[@]}"; do
    [[ -r "$socket_file" ]] || continue
    case "$socket_file" in
      "$registry_root"/*/owners/*)
        _nvim_open_read_registry_record "$socket_file" "${socket_file##*/}" "$registry_root" || continue
        socket="$REPLY_SOCKET"
        ;;
      "$latest_file")
        _nvim_open_read_registry_record "$socket_file" "" "$registry_root" || continue
        socket="$REPLY_SOCKET"
        ;;
      *) IFS= read -r socket <"$socket_file" || socket="" ;;
    esac
    [[ -n "$socket" ]] || continue
    [[ "$seen_sockets" != *$'\n'"$socket"$'\n'* ]] || continue
    seen_sockets+="$socket"$'\n'
    [[ -S "$socket" ]] || continue
    if nvim_open_socket "$socket" "$target_file" "$target_line" "$target_col" "$target_cwd" "$source"; then
      return 0
    fi
    nvim_open_log "rpc-open-failed socket=$socket"
  done

  return 1
}

nvim_open_pane_key() {
  local pane="$1" tmux_value="${TMUX:-}" tmux_socket tmux_fields tmux_pid key
  local chunk first=1
  [[ -n "$pane" ]] || return 1

  tmux_fields="${tmux_value%,*}"
  if [[ "$tmux_fields" == "$tmux_value" || "$tmux_fields" != *,* ]]; then
    tmux_socket="$tmux_value"
    tmux_pid=""
  else
    tmux_socket="${tmux_fields%,*}"
    tmux_pid="${tmux_fields##*,}"
  fi

  key=$(printf '%s\0%s\0%s' "$tmux_socket" "$tmux_pid" "$pane" |
    LC_ALL=C od -An -tx1 -v | tr -d ' \n') || return 1
  [[ -n "$key" ]] || return 1
  while [[ -n "$key" ]]; do
    chunk="${key:0:120}"
    key="${key:120}"
    if [[ "$first" == 1 ]]; then
      printf 'v1-%s' "$chunk"
      first=0
    else
      printf '/%s' "$chunk"
    fi
  done
  printf '\n'
}

nvim_open_legacy_pane_key() {
  local pane="$1" tmux_socket="${TMUX%%,*}" key
  [[ -n "$pane" ]] || return 1
  key="$pane"
  if [[ -n "$tmux_socket" && "$tmux_socket" != "$TMUX" ]]; then
    key="$tmux_socket:$pane"
  fi
  printf '%s\n' "$key" | sed 's/[^[:alnum:]_.-]/_/g'
}

nvim_open_pane_socket() {
  local pane_id="$1" target_file="$2" target_line="$3" target_col="$4" target_cwd="$5" source="$6"
  local key legacy_key socket socket_file state_dir registry_root latest_file legacy_file
  local preferred_record="" new_activity=""
  local legacy_first=0 seen_sockets
  local -a socket_files=() owner_records=() REPLY_RECORDS=()

  key=$(nvim_open_pane_key "$pane_id") || return 1
  legacy_key=$(nvim_open_legacy_pane_key "$pane_id") || return 1
  nvim_open_resolve_state_dir || return 1
  state_dir="$REPLY"
  registry_root="$state_dir/registry"
  latest_file="$registry_root/panes/$key/latest"
  legacy_file="$state_dir/panes/$legacy_key"

  if _nvim_open_read_registry_record "$latest_file" "" "$registry_root"; then
    preferred_record="$latest_file"
    new_activity="$latest_file"
  fi
  _nvim_open_ordered_owner_records "$registry_root/panes/$key/owners" "$registry_root"
  owner_records=("${REPLY_RECORDS[@]}")
  if [[ -z "$new_activity" ]]; then
    new_activity=${owner_records[0]:-}
  fi
  if [[ -e "$legacy_file" && (-z "$new_activity" || ! "$new_activity" -nt "$legacy_file") ]]; then
    socket_files+=("$legacy_file")
    legacy_first=1
  fi
  if [[ -n "$preferred_record" ]]; then
    socket_files+=("$preferred_record")
  fi
  for socket_file in "${owner_records[@]}"; do
    socket_files+=("$socket_file")
  done
  [[ "$legacy_first" == 1 ]] || socket_files+=("$legacy_file")

  seen_sockets=$'\n'
  for socket_file in "${socket_files[@]}"; do
    [[ -r "$socket_file" ]] || continue
    case "$socket_file" in
      "$registry_root"/*/owners/*)
        _nvim_open_read_registry_record "$socket_file" "${socket_file##*/}" "$registry_root" || continue
        socket="$REPLY_SOCKET"
        ;;
      "$latest_file")
        _nvim_open_read_registry_record "$socket_file" "" "$registry_root" || continue
        socket="$REPLY_SOCKET"
        ;;
      *) IFS= read -r socket <"$socket_file" || socket="" ;;
    esac
    [[ -n "$socket" ]] || continue
    [[ "$seen_sockets" != *$'\n'"$socket"$'\n'* ]] || continue
    seen_sockets+="$socket"$'\n'
    if nvim_open_socket "$socket" "$target_file" "$target_line" "$target_col" "$target_cwd" "$source"; then
      return 0
    fi
  done
  return 1
}

nvim_open_tmux_send() {
  local pane_id="$1" target_file="$2" target_line="$3" target_col="$4" target_cwd="$5"
  local fallback_file vim_file line col

  [[ -n "$pane_id" ]] || return 1
  fallback_file="$target_file"
  if [[ "$fallback_file" != /* && -n "$target_cwd" ]]; then
    fallback_file="${target_cwd%/}/$fallback_file"
  fi

  # This is the no-RPC fallback: we can't pass line/col structurally, so we type
  # an :edit command into the running nvim. Build it with fnameescape() (applied
  # by nvim) so filename metacharacters like * ? [ { don't glob or misparse in
  # :edit — a plain `:e <file>` mishandled those and could open the wrong file.
  # We only escape the file for the surrounding Vim double-quoted string (\ and
  # "). cursor() then sets line AND column (the previous `:e +LINE` dropped the
  # column entirely).
  vim_file=$(printf '%s' "$fallback_file" | sed 's/[\\"]/\\&/g')
  line="${target_line:-1}"
  [[ "$line" =~ ^[0-9]+$ ]] || line=1
  col="${target_col:-1}"
  [[ "$col" =~ ^[0-9]+$ ]] || col=1

  # Leave terminal-mode reliably. tmux key names for C-\ can degrade into a
  # literal C-n in nested terminal layouts; hex sends the exact control bytes.
  tmux send-keys -t "$pane_id" -H 1c 0e
  tmux send-keys -t "$pane_id" Escape
  tmux send-keys -t "$pane_id" -l ":exe 'e '.fnameescape(\"${vim_file}\")|call cursor(${line},${col})"
  tmux send-keys -t "$pane_id" Enter
}

nvim_open_current_window() {
  local target_file="$1" target_line="$2" target_col="$3" target_cwd="$4" source="$5"
  local first_pane="" pane_command pane_id tmux_panes

  tmux_panes=$(tmux list-panes -F '#{pane_id}	#{pane_current_command}' 2>/dev/null) || return 1
  while IFS=$'\t' read -r pane_id pane_command; do
    [[ "$pane_command" == "nvim" ]] || continue
    [[ -n "$first_pane" ]] || first_pane="$pane_id"
    if nvim_open_pane_socket "$pane_id" "$target_file" "$target_line" "$target_col" "$target_cwd" "$source"; then
      return 0
    fi
  done <<<"$tmux_panes"

  [[ -n "$first_pane" ]] || return 1
  nvim_open_tmux_send "$first_pane" "$target_file" "$target_line" "$target_col" "$target_cwd"
}
