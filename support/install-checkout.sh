#!/usr/bin/env bash
# Install checkout-backed command and manpage links.
#
# Provider-backed launchers resolve their physical source paths to load matching
# libraries. Linking rather than copying therefore keeps each command, its
# implementation assets, and its documentation on one reviewed revision.

set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="${BIN_DIR:-$PREFIX/bin}"
MAN_DIR="${MAN_DIR:-$PREFIX/share/man/man1}"
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)

commands=(
  eza-nvim-links
  nvim-link-host
  nvim-ssh-control-open
  nvim-tmux-open
  termnav-relay
  termnav-switch-tab
  termnav-tmux-context
  tmux-follow-click
  vscode-nvim-focus
  vscode-switch-tab
  wezterm-move-tab
  wezterm-select-pane
  wezterm-switch-tab
)

die() {
  printf 'termnav: %s\n' "$*" >&2
  exit 1
}

refuse_non_symlink() {
  local target="$1"

  if [[ (-e "$target" || -L "$target") && ! -L "$target" ]]; then
    die "refusing to replace non-symlink path: $target"
  fi
}

# Finish every source and ownership check before creating a directory or link.
# A missing artifact or one user-owned destination must leave the installation
# completely unchanged rather than publishing a misleading partial command set.
for command in "${commands[@]}"; do
  [[ -x "$ROOT/bin/$command" ]] ||
    die "required executable is missing: $ROOT/bin/$command"
  [[ -f "$ROOT/man/man1/$command.1" ]] ||
    die "required manpage is missing: $ROOT/man/man1/$command.1"
  refuse_non_symlink "$BIN_DIR/$command"
  refuse_non_symlink "$MAN_DIR/$command.1"
done

link_sources=()
link_targets=()
prior_states=()
prior_targets=()

for command in "${commands[@]}"; do
  link_sources+=("$ROOT/bin/$command" "$ROOT/man/man1/$command.1")
  link_targets+=("$BIN_DIR/$command" "$MAN_DIR/$command.1")
done

# Snapshot every destination before publishing any link. If a later link
# fails, restoring this small journal prevents commands and documentation from
# referring to different checkout revisions.
for target in "${link_targets[@]}"; do
  if [[ -L "$target" ]]; then
    prior_states+=(symlink)
    prior_targets+=("$(readlink "$target")")
  else
    prior_states+=(absent)
    prior_targets+=("")
  fi
done

rollback_links() {
  local count="$1"
  local index target

  for ((index = count - 1; index >= 0; index--)); do
    target="${link_targets[$index]}"
    if [[ "${prior_states[$index]}" == symlink ]]; then
      if ! ln -sfn -- "${prior_targets[$index]}" "$target"; then
        printf 'termnav: rollback failed to restore symlink: %s\n' "$target" >&2
      fi
    elif ! rm -f "$target"; then
      printf 'termnav: rollback failed to remove new link: %s\n' "$target" >&2
    fi
  done

  # Rollback is best effort. Its diagnostics matter, but its status must never
  # replace the original publication failure returned to the caller.
  return 0
}

mkdir -p "$BIN_DIR" "$MAN_DIR"
for ((index = 0; index < ${#link_targets[@]}; index++)); do
  if ln -sfn -- "${link_sources[$index]}" "${link_targets[$index]}"; then
    continue
  else
    status=$?
    rollback_links "$((index + 1))"
    exit "$status"
  fi
done

printf 'installed termnav commands to %s\n' "$BIN_DIR"
printf 'installed termnav manpages to %s\n' "$MAN_DIR"
