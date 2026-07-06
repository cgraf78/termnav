# shellcheck shell=bash
# MCP backend for termnav_vscode_execute_command: calls the
# nabheet.vscode-ide-mcp extension's local HTTP JSON-RPC API to execute a
# VS Code command by ID.
#
# Contract with dotfiles: the auth token is written by
# ~/.local/lib/dot/core/merge-hooks/vscode.sh to
# ${XDG_STATE_HOME:-$HOME/.local/state}/dot/vscode-mcp-auth-token. This file
# only reads it -- it never generates or manages the token.

_termnav_vscode_mcp_port() {
  printf '%s\n' "${VSCODE_MCP_PORT:-9876}"
}

_termnav_vscode_mcp_auth_token_path() {
  printf '%s\n' "${XDG_STATE_HOME:-$HOME/.local/state}/dot/vscode-mcp-auth-token"
}

_termnav_vscode_mcp_auth_token() {
  local path
  path="$(_termnav_vscode_mcp_auth_token_path)"
  [[ -r "$path" ]] || return 1
  cat "$path"
}

# POST a JSON-RPC request. Args: method, params (raw JSON), token, port.
# Bounded by --max-time so a dead/slow server never hangs the caller --
# tmux's run-shell -b is already non-blocking, but nvim's async jobstart
# still needs this to resolve promptly.
_termnav_vscode_mcp_post() {
  local method="$1" params="$2" token="$3" port="$4"
  curl -sS --max-time 2 -X POST "http://127.0.0.1:${port}/mcp" \
    -H 'Content-Type: application/json' \
    -H "Authorization: Bearer ${token}" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"${method}\",\"params\":${params}}"
}

# Tries tools/call directly first (the server's documented "direct POST,
# backward compat, synchronous" mode). If that response carries a JSON-RPC
# error (observed if a session/initialize step turns out to be required),
# falls back to initialize + one retry of tools/call.
termnav_vscode_mcp_execute_command() {
  local command_id="$1" token port response call_params

  token="$(_termnav_vscode_mcp_auth_token)" || return 1
  port="$(_termnav_vscode_mcp_port)"

  # Construct call params with escaped quotes so the entire JSON-RPC
  # payload has consistent escaping when passed to curl.
  call_params=$(printf '{\"name\":\"execute_command\",\"arguments\":{\"command\":\"%s\"}}' "$command_id")

  response="$(_termnav_vscode_mcp_post 'tools/call' "$call_params" "$token" "$port")" || return 1
  [[ -n "$response" ]] || return 1
  [[ "$response" == *'"error"'* ]] || return 0

  _termnav_vscode_mcp_post 'initialize' '{"protocolVersion":"2024-11-05","capabilities":{}}' "$token" "$port" >/dev/null || return 1
  response="$(_termnav_vscode_mcp_post 'tools/call' "$call_params" "$token" "$port")" || return 1
  [[ -n "$response" && "$response" != *'"error"'* ]]
}
