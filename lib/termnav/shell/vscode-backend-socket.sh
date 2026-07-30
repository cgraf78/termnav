# shellcheck shell=bash
# Window-scoped backend for termnav_vscode_execute_command.
#
# The local cgraf.termnav extension publishes an owner-only Unix socket into
# terminals created by its window. Posting a direction to that socket avoids
# process-global ports, custom URI trust policy, and product-specific CLIs.

termnav_vscode_socket_execute_command() {
  local command_id="$1" curl direction payload

  case "$command_id" in
    workbench.action.terminal.focusNext) direction="next" ;;
    workbench.action.terminal.focusPrevious) direction="previous" ;;
    *) return 1 ;;
  esac

  [[ -n "${TERMNAV_VSCODE_SOCKET:-}" ]] || return 1

  curl="${TERMNAV_VSCODE_CURL:-curl}"
  command -v "$curl" >/dev/null 2>&1 || return 1
  payload="{\"direction\":\"$direction\"}"
  "$curl" --silent --show-error --fail --max-time 2 \
    --unix-socket "$TERMNAV_VSCODE_SOCKET" \
    --header 'Content-Type: application/json' \
    --header 'Accept: application/json' \
    --data-binary "$payload" http://localhost/switch-tab >/dev/null 2>&1
}
