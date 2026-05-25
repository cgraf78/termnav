# shellcheck shell=bash

nvim_open_ssh_option_takes_value() {
  case "$1" in
    -B | -b | -c | -D | -E | -e | -F | -I | -i | -J | -L | -l | -m | -O | -o | -p | -Q | -R | -S | -W | -w)
      return 0
      ;;
  esac
  return 1
}

nvim_open_et_option_takes_value() {
  case "$1" in
    -c | -i | -l | -o | -p | -r | -S | -s | -t)
      return 0
      ;;
  esac
  return 1
}

nvim_open_remote_candidate() {
  local candidate="$1"

  # `dev connect`/ET descendants can expose hosts either as user@host or as
  # HostName=host. Normalize only those transport spellings, not arbitrary text.
  candidate="${candidate#*@}"
  candidate="${candidate#HostName=}"
  candidate="${candidate%%:*}"
  [[ -n "$candidate" ]] || return 1
  printf '%s\n' "$candidate"
}

nvim_open_remote_host_from_positional_command() {
  local pane_command="$1" start_command="$2"
  local first_command token
  local -a words

  read -r -a words <<<"$start_command"
  [[ ${#words[@]} -gt 0 ]] || return 1
  # Only parse this command if it is actually an invocation of the transport.
  # tmux's pane_start_command is the command that *created* the pane, which for
  # an interactive pane is the login shell (e.g. `exec /usr/bin/zsh -l`); the
  # real `ssh host` is typed later and lives only in the process tree. Without
  # this guard a shell's argv is misread as transport options, returning the
  # shell word (`exec`) as the "host". Returning here instead lets the caller
  # fall through to the live process tree, which carries the true `ssh host`.
  first_command="${words[0]##*/}"
  [[ "$first_command" == "$pane_command" ]] || return 1
  words=("${words[@]:1}")

  while [[ ${#words[@]} -gt 0 ]]; do
    token="${words[0]}"
    words=("${words[@]:1}")

    if [[ "$token" == "--" ]]; then
      break
    fi
    if {
      [[ "$pane_command" == "et" ]] && nvim_open_et_option_takes_value "$token"
    } || {
      [[ "$pane_command" != "et" ]] && nvim_open_ssh_option_takes_value "$token"
    }; then
      [[ ${#words[@]} -gt 0 ]] && words=("${words[@]:1}")
      continue
    fi
    if [[ "$token" == -[A-Za-z]?* || "$token" == -o* || "$token" == -* ]]; then
      continue
    fi

    nvim_open_remote_candidate "$token"
    return 0
  done

  [[ ${#words[@]} -gt 0 ]] || return 1
  nvim_open_remote_candidate "${words[0]}"
}

nvim_open_pane_process_command() {
  local pane_pid="$1" pane_command="$2"

  [[ "$pane_pid" =~ ^[0-9]+$ ]] || return 1

  # tmux's pane_start_command is the command that created the pane. For
  # interactive shells, the active remote transport is usually a descendant
  # process typed later at the prompt, so inspect the pane process tree too.
  ps -axww -o pid= -o ppid= -o command= 2>/dev/null |
    awk -v root="$pane_pid" -v want="$pane_command" '
      function basename(command, parts, first) {
        split(command, parts, /[[:space:]]+/)
        first = parts[1]
        sub(/^.*\//, "", first)
        return first
      }

      {
        pid = $1
        ppid = $2
        $1 = ""
        $2 = ""
        sub(/^[[:space:]]+/, "")
        command[pid] = $0
        parent[pid] = ppid
        pids[++count] = pid
      }

      END {
        for (i = 1; i <= count; i++) {
          pid = pids[i]
          if (pid == root) {
            descendant[pid] = 1
            continue
          }
          parent_pid = pid
          while (parent_pid in parent) {
            if (parent[parent_pid] == root) {
              descendant[pid] = 1
              break
            }
            parent_pid = parent[parent_pid]
          }
        }

        for (i = 1; i <= count; i++) {
          pid = pids[i]
          if (descendant[pid] && basename(command[pid]) == want) {
            print command[pid]
          }
        }
      }
    ' |
    tail -1
}

nvim_open_remote_host_from_extension_command() {
  local pane_command="$1" start_command="$2"

  command -v nvim-remote-pane-host >/dev/null 2>&1 || return 1
  nvim-remote-pane-host "$pane_command" "$start_command"
}

nvim_open_remote_builtin_command() {
  case "$1" in
    ssh | mosh | et)
      return 0
      ;;
  esac
  return 1
}

nvim_open_remote_host_from_command() {
  local pane_command="$1" start_command="$2" pane_pid="$3" process_command

  if nvim_open_remote_builtin_command "$pane_command"; then
    nvim_open_remote_host_from_positional_command "$pane_command" "$start_command" && return 0
  else
    nvim_open_remote_host_from_extension_command "$pane_command" "$start_command" && return 0
  fi

  process_command=$(nvim_open_pane_process_command "$pane_pid" "$pane_command" 2>/dev/null || true)
  [[ -n "$process_command" ]] || return 1

  if nvim_open_remote_builtin_command "$pane_command"; then
    nvim_open_remote_host_from_positional_command "$pane_command" "$process_command"
  else
    nvim_open_remote_host_from_extension_command "$pane_command" "$process_command"
  fi
}

nvim_open_remote_hosts_match() {
  local actual="$1" expected="$2"

  # Local link context usually carries the ds alias (`dev1`), while transport
  # argv may expose an FQDN. Treat short/FQDN forms as the same host.
  [[ "$actual" == "$expected" ]] && return 0
  [[ "${actual%%.*}" == "$expected" ]] && return 0
  [[ "$actual" == "${expected%%.*}" ]] && return 0
  return 1
}

nvim_open_remote_pane_matches() {
  local pane_command="$1" start_command="$2" pane_pid="$3" expected_host="$4" actual_host

  if [[ -z "$expected_host" ]]; then
    nvim_open_remote_builtin_command "$pane_command" && return 0
    nvim_open_remote_host_from_command "$pane_command" "$start_command" "$pane_pid" >/dev/null
    return
  fi

  actual_host=$(nvim_open_remote_host_from_command "$pane_command" "$start_command" "$pane_pid" 2>/dev/null || true)
  nvim_open_remote_hosts_match "$actual_host" "$expected_host"
}

nvim_open_remote_controlmaster() {
  local remote_host="$1" input="$2" control_rc

  [[ -n "$remote_host" ]] || return 10
  nvim-ssh-control-open "$remote_host" "$input"
  control_rc=$?
  if [[ "$control_rc" -eq 0 ]]; then
    nvim_open_log "remote-controlmaster-open host=$remote_host input=$input"
    return 0
  fi

  nvim_open_log "remote-controlmaster-failed host=$remote_host rc=$control_rc"
  return "$control_rc"
}

nvim_open_remote_tmux_fallback() {
  local remote_host="$1" input="$2" tmux_panes="$3"
  local pane_command pane_id pane_pid pane_start_command remote_pane=""
  local quoted_command quoted_input shell_command

  # Find a local tmux pane running a known remote session. Local extensions can
  # teach this helper about extra transports with `nvim-remote-pane-host`.
  while IFS=$'\t' read -r pane_id pane_command pane_start_command pane_pid; do
    if nvim_open_remote_pane_matches "$pane_command" "$pane_start_command" "$pane_pid" "$remote_host"; then
      remote_pane="$pane_id"
      break
    fi
  done <<<"$tmux_panes"
  if [[ -z "$remote_pane" ]]; then
    if [[ -n "$remote_host" ]]; then
      nvim_open_log "no-matching-remote-pane host=$remote_host"
    else
      nvim_open_log "no-pane"
    fi
    return 1
  fi

  # C-b reaches the remote tmux prefix through the connection. Sleep gives the
  # remote command prompt time to initialize.
  tmux send-keys -t "$remote_pane" C-b :
  sleep 0.15
  quoted_input=$(nvim_open_shell_quote "$input")
  # This command executes inside the remote tmux server. Use the same
  # user-facing wrapper as local tmux clicks so a missing remote nvim produces a
  # status message instead of a raw `run-shell` failure.
  shell_command="\$HOME/.local/bin/nvim-tmux-open tmux-link ${quoted_input}"
  quoted_command=$(nvim_open_tmux_quote "$shell_command")
  tmux send-keys -t "$remote_pane" -l "run-shell ${quoted_command}"
  tmux send-keys -t "$remote_pane" Enter
  nvim_open_log "remote-tmux-open host=${remote_host:-unknown} pane=$remote_pane input=$input"
}
