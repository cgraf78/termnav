# shellcheck shell=bash
# The tmux version rule for the shell tests: function definitions only.
# test/termnav-test sources this file on its own, before any suite runs, so
# it must not run commands, set variables or traps, or export anything at
# load time; helpers.sh, which does all of those for each suite, sources it
# too. test-isolation-test checks both.

# The tmux floor, read from its one source, src/tmux_version.rs, as
# "MAJOR MINOR". The pattern is the const's exact text, so a reworded const
# fails here, by name, rather than as an arithmetic error downstream.
_tmux_floor() {
  local source=${BASH_SOURCE[0]%/*}/../src/tmux_version.rs floor
  floor=$(sed -n 's/^pub(crate) const TMUX_MINIMUM: (u32, u32) = (\([0-9][0-9]*\), \([0-9][0-9]*\));$/\1 \2/p' \
    "$source")
  if [[ ! $floor =~ ^[0-9]+\ [0-9]+$ ]]; then
    printf 'tmux-version.sh: no TMUX_MINIMUM in %s\n' "$source" >&2
    return 1
  fi
  printf '%s\n' "$floor"
}

# How a tmux version reads, as src/tmux_version.rs reads it: "MAJOR MINOR",
# "newest" for master and openbsd-*, or "unknown".
_tmux_release() {
  local version=${1#tmux } major minor
  case $version in master | openbsd-*)
    echo newest
    return
    ;;
  esac
  version=${version#next-}
  major=${version%%.*}
  minor=${version#*.}
  minor=${minor%%[!0-9]*}
  if [[ $version == *.* && $major =~ ^[0-9]+$ && -n $minor ]]; then
    echo "$((10#$major)) $((10#$minor))"
  else
    echo unknown
  fi
}

# Whether tmux VERSION meets the floor; newest and unknown builds do.
_tmux_supported() {
  local release floor
  release=$(_tmux_release "$1")
  case $release in newest | unknown) return 0 ;; esac
  floor=$(_tmux_floor) || return 1
  # shellcheck disable=SC2086 # Split into the four numbers.
  set -- $release $floor
  (($1 > $3 || ($1 == $3 && $2 >= $4)))
}

# "ok", "missing", or "unsupported VERSION" for the tmux on PATH.
# test/termnav-test settles this once in TERMNAV_TEST_TMUX for every suite.
_termnav_tmux_state() {
  local version
  command -v tmux >/dev/null 2>&1 || {
    echo missing
    return
  }
  version=$(tmux -V 2>/dev/null)
  version=${version#tmux }
  if _tmux_supported "$version"; then
    echo ok
  else
    echo "unsupported $version"
  fi
}

# Whether this run has a supported tmux; suites skip their tmux cases when it
# does not.
_tmux_usable() {
  [[ ${TERMNAV_TEST_TMUX:-$(_termnav_tmux_state)} == ok ]]
}
