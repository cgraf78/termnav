# ZLE adapter for the pane mark written by `termnav paste-image receive`.

_termnav_bracketed_paste() {
  local left=$LBUFFER right=$RBUFFER saved_status=0
  zle _termnav_bracketed_paste_orig "$@" || saved_status=$?

  # Compare literally even when a user's Zsh options change pattern matching.
  emulate -L zsh
  local pane=${TMUX_PANE-} option=@termnav-paste-preview
  local inserted mark mode image literal consumed
  [[ $CONTEXT == start && $RBUFFER == "$right" && $LBUFFER == "$left"?* &&
    -n ${TMUX-} && $pane == %<-> ]] || return "$saved_status"
  command -v termnav >/dev/null 2>&1 || return "$saved_status"

  inserted=${LBUFFER#"$left"}
  mark=$(command tmux show-options -pqv -t "$pane" "$option" 2>/dev/null) || return "$saved_status"
  case $mark in
    sixel:?* | blocks:?*) ;;
    *) return "$saved_status" ;;
  esac
  mode=${mark%%:*}
  image=${mark#*:}
  [[ $inserted == "$image" || ${(Q)inserted} == "$image" ]] || return "$saved_status"

  # Compare and consume on the server so a later paste's mark survives.
  # Escape tmux format delimiters in the value. The command branch contains
  # only a validated numeric pane id and the fixed option name.
  literal=${mark//\#/##}
  literal=${literal//,/#,}
  literal=${literal//\}/#\}}
  # Keep `#[` in a path from being parsed as a tmux style directive.
  literal=${literal//\[/\#\{l:[\}}
  consumed=$(command tmux if-shell -F -t "$pane" "#{==:#{${option}},${literal}}" \
    "set-option -pu -t $pane $option; display-message -p 1" 2>/dev/null) || return "$saved_status"
  [[ $consumed == 1 ]] || return "$saved_status"

  # Refresh to measure the current wrapping, even if the original widget
  # already yielded the terminal. ZLE runs POSTEDIT after moving below the
  # buffer. Reuse those rows before running the user's existing POSTEDIT.
  # Editing LBUFFER here would cancel pending completion state.
  zle -R || return "$saved_status"
  local POSTEDIT=$'\e['${BUFFERLINES}$'A\r\e[J'${POSTEDIT-}
  zle -I || return "$saved_status"
  command termnav paste-image preview --mode "$mode" -- "$image" 2>/dev/null </dev/null >/dev/tty || true
  return "$saved_status"
}

_termnav_paste_preview_bind() {
  # Keep the binding guard in this shell even when ALLEXPORT is enabled.
  emulate -L zsh
  add-zsh-hook -d precmd _termnav_paste_preview_bind
  [[ -n ${TMUX_PANE-} ]] || return 0
  if zle -A bracketed-paste _termnav_bracketed_paste_orig &&
    zle -N bracketed-paste _termnav_bracketed_paste; then
    # A plugin may later wrap this widget. Keep its original saved alias when
    # re-sourcing the loader, even after the public widget's name has changed.
    typeset -g _termnav_paste_preview_bound=1
  fi
  return 0
}

if [[ -o interactive && -n ${TMUX_PANE-} && ${_termnav_paste_preview_bound:-0} != 1 ]]; then
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd _termnav_paste_preview_bind
fi
