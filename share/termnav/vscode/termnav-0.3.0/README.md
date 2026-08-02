# Termnav for VS Code

This companion adapter connects VS Code terminal windows to
[Termnav](https://github.com/cgraf78/termnav). It publishes a private,
per-window Unix socket and capability into newly created integrated terminals,
routes terminal-tab requests back to their originating VS Code window, and
maintains the `termnav.nvimFocused` context used by Termnav keybindings.

The adapter is intended to be installed with the Termnav command-line and
Neovim integration. Existing integrated terminals must be relaunched after the
extension is installed or its extension host restarts so they inherit the
current socket and capability.

The extension makes no external network requests, downloads no executables,
collects no telemetry, and does not inspect workspace files. Its HTTP parser is
bound only to an owner-readable Unix-domain socket under
`/tmp/termnav-vscode-UID`.
