# shellcheck shell=bash
# Teach `termnav tmux follow-click` one host-defined token without forking it.
#
# Extension files run inside the helper. A detector claims a token by setting
# `target` and `target_kind`, then returning success. Non-matches must return
# nonzero so Termnav can try later detectors and its public fallback rules.

_termnav_example_ticket_detector() {
  local token="$1" number

  case "$token" in
    TICKET-*) number=${token#TICKET-} ;;
    *) return 1 ;;
  esac
  case "$number" in
    "" | *[!0-9]*) return 1 ;;
  esac

  # These are documented output variables consumed by Termnav after
  # the sourced detector returns; they are intentionally not read in this file.
  # shellcheck disable=SC2034
  target="https://issues.example.com/$token"
  # shellcheck disable=SC2034
  target_kind="url"
  return 0
}

tmux_follow_register_token_detector _termnav_example_ticket_detector
