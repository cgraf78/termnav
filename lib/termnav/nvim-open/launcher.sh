# shellcheck shell=bash
# Reuse an existing Neovim pane only for a simple file open typed at a prompt.
#
# `termnav_nvim_try_reuse <nvim-argv...>` returns 0 only after
# nvim-tmux-open successfully handles the request. Any ineligible context,
# uncertain parent process, or routing failure returns 1 so the caller can run
# its real editor and preserve normal blocking $EDITOR semantics.

_termnav_nvim_parent_argv_is_interactive_shell() {
  local parent="$1" arg first
  local -a argv
  shift
  argv=("$@")

  [[ -n "$parent" ]] || return 1
  parent="${parent##*/}"
  parent="${parent#-}"

  case "$parent" in
    bash | dash | fish | ksh | mksh | sh | yash | zsh)
      ;;
    *)
      return 1
      ;;
  esac

  [[ "${#argv[@]}" -gt 0 ]] || return 0

  first="${argv[0]##*/}"
  first="${first#-}"
  [[ "$first" == "$parent" ]] || return 1

  for arg in "${argv[@]:1}"; do
    case "$arg" in
      -c | --command)
        return 1
        ;;
      -*)
        ;;
      *)
        # A positional argument means this shell is running a script rather
        # than waiting at an interactive prompt. The editor must block there.
        return 1
        ;;
    esac
  done

  return 0
}

_termnav_nvim_parent_is_interactive_shell() {
  local arg args parent parent_info proc_dir
  local -a argv=()

  # Linux procfs preserves argv boundaries without a subprocess. Keep the ps
  # fallback for macOS and for restricted or malformed procfs mounts, including
  # Termux environments where one or both files may be unavailable.
  proc_dir="${NVIM_LAUNCHER_PROC_ROOT:-/proc}/$PPID"
  if [[ -r "$proc_dir/comm" && -r "$proc_dir/cmdline" ]]; then
    IFS= read -r parent 2>/dev/null <"$proc_dir/comm" || parent=""
    while IFS= read -r -d '' arg; do
      argv+=("$arg")
    done 2>/dev/null <"$proc_dir/cmdline"
    if [[ -n "$parent" && "${#argv[@]}" -gt 0 ]]; then
      _termnav_nvim_parent_argv_is_interactive_shell "$parent" "${argv[@]}"
      return $?
    fi
  fi

  parent_info=$(ps -ww -o comm= -o args= -p "$PPID" 2>/dev/null) || return 1
  read -r parent args <<<"$parent_info"
  if [[ -n "$args" ]]; then
    read -r -a argv <<<"$args"
    _termnav_nvim_parent_argv_is_interactive_shell "$parent" "${argv[@]}"
  else
    _termnav_nvim_parent_argv_is_interactive_shell "$parent"
  fi
}

_termnav_nvim_should_reuse() {
  [[ "${NVIM_LAUNCHER_FORCE_NEW:-0}" != 1 ]] || return 1
  [[ -n "${TMUX:-}" ]] || return 1
  [[ -t 0 || "${NVIM_LAUNCHER_ALLOW_NONTTY:-0}" == 1 ]] || return 1
  [[ "$#" -eq 1 ]] || return 1

  case "$1" in
    "" | -* | +*)
      return 1
      ;;
  esac

  command -v nvim-tmux-open >/dev/null 2>&1 || return 1
  [[ "${NVIM_LAUNCHER_ALLOW_NONSHELL_PARENT:-0}" == 1 ]] ||
    _termnav_nvim_parent_is_interactive_shell || return 1
  return 0
}

termnav_nvim_try_reuse() {
  _termnav_nvim_should_reuse "$@" || return 1
  nvim-tmux-open cli "$1" "$PWD"
}
