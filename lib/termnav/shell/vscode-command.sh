# shellcheck shell=bash
# Dispatch seam for executing a VS Code command via a pluggable backend.
#
# Callers (bin/vscode-switch-tab, bin/vscode-move-tab) only know VS Code
# command IDs — never how a command actually gets executed. This file owns
# backend selection and uniform failure handling so a future backend swap
# (if the MCP extension proves problematic) only changes
# TERMNAV_VSCODE_BACKEND's default, never any caller.

_termnav_vscode_command_dir() {
  cd -P -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd
}

# Execute a VS Code command by ID through the selected backend ($1).
# Silently no-ops (returns non-zero, prints nothing) on any failure --
# unreachable backend, missing auth, timeout -- matching how the existing
# WezTerm bubble-up path already does nothing when there's nowhere to
# bubble to.
termnav_vscode_execute_command() {
  local command_id="$1" backend backend_file backend_fn

  backend="${TERMNAV_VSCODE_BACKEND:-mcp}"
  backend_file="$(_termnav_vscode_command_dir)/vscode-backend-${backend}.sh"
  [[ -r "$backend_file" ]] || return 1

  # shellcheck source=/dev/null
  . "$backend_file" || return 1

  backend_fn="termnav_vscode_${backend}_execute_command"
  declare -F "$backend_fn" >/dev/null 2>&1 || return 1

  "$backend_fn" "$command_id"
}
