#!/usr/bin/env bash
# helpers.sh — shared test framework for termnav tests.
#
# Source this file from test scripts to get assertion helpers,
# temp directory management, and a summary reporter.
#
# Usage:
#   . "./test/helpers.sh"
#   _assert_eq "description" "expected" "actual"
#   ...
#   _test_summary  # prints results, exits 0 or 1

PASS=0
FAIL=0
CLEANUP_DIRS=()

# Mark every suite, including suites run directly, so code that can reach
# outside the mock HOME can avoid touching the real host during WSL tests.
export REPO_TEST=1

# The repo test runner may set TEST_STYLE=1 for child suites when styled output is
# appropriate. Individual suites keep exporting NO_COLOR for deterministic tool
# output, so this opt-in is separate from NO_COLOR and only affects our harness
# status lines.
_TEST_PRETTY=false
[[ "${TEST_STYLE:-0}" = 1 ]] && _TEST_PRETTY=true

_test_style() {
  local color="$1"
  shift
  if $_TEST_PRETTY; then
    local sgr
    case "$color" in
      green) sgr='38;2;63;185;80' ;;
      red) sgr='38;2;248;81;73' ;;
      yellow) sgr='38;2;210;153;34' ;;
      dim) sgr='38;2;139;148;158' ;;
      bold) sgr='1' ;;
      *) sgr='0' ;;
    esac
    printf '\033[%sm%s\033[0m\n' "$sgr" "$*"
  else
    echo "$*"
  fi
}

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

_pass() {
  PASS=$((PASS + 1))
  if $_TEST_PRETTY; then
    _test_style green "  ✓ $1"
  else
    echo "  PASS: $1"
  fi
}
_fail() {
  FAIL=$((FAIL + 1))
  if $_TEST_PRETTY; then
    _test_style red "  ✗ $1" >&2
  else
    echo "  FAIL: $1" >&2
  fi
}

_assert_eq() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    _pass "$desc"
  else
    _fail "$desc (expected '$expected', got '$actual')"
  fi
}

_assert_contains() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$actual" == *"$expected"* ]]; then
    _pass "$desc"
  else
    _fail "$desc (expected to contain '$expected', got '$actual')"
  fi
}

_assert_not_contains() {
  local desc="$1" unexpected="$2" actual="$3"
  if [[ "$actual" != *"$unexpected"* ]]; then
    _pass "$desc"
  else
    _fail "$desc (should not contain '$unexpected')"
  fi
}

_assert_colon_list_values_aligned() {
  local desc="$1" content="$2" marker="$3"
  local in_list=0 expected_col="" row_count=0
  local line label after_colon spaces col

  while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$line" == "$marker" ]]; then
      in_list=1
      continue
    fi

    [[ "$in_list" -eq 1 ]] || continue
    [[ -n "$line" ]] || break

    if [[ "$line" != "  "*:* ]]; then
      _fail "$desc (unexpected list row '$line')"
      return
    fi

    label=${line%%:*}
    after_colon=${line#*:}
    spaces=${after_colon%%[! ]*}

    if [[ -z "$spaces" || "$spaces" == "$after_colon" ]]; then
      _fail "$desc (missing list spacing after '$label:')"
      return
    fi

    col=$((${#label} + 1 + ${#spaces}))
    if [[ -z "$expected_col" ]]; then
      expected_col=$col
    elif [[ "$col" -ne "$expected_col" ]]; then
      _fail "$desc (list starts at column $col, expected $expected_col: '$line')"
      return
    fi

    row_count=$((row_count + 1))
  done <<<"$content"

  if [[ "$row_count" -eq 0 ]]; then
    _fail "$desc (no rows found after '$marker')"
  else
    _pass "$desc"
  fi
}

_assert_exit() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$expected" -eq "$actual" ]]; then
    _pass "$desc"
  else
    _fail "$desc (expected exit $expected, got $actual)"
  fi
}

_assert_file_exists() {
  local desc="$1" path="$2"
  if [[ -f "$path" ]]; then
    _pass "$desc"
  else
    _fail "$desc (file not found: $path)"
  fi
}

_assert_file_missing() {
  local desc="$1" path="$2"
  if [[ ! -f "$path" ]]; then
    _pass "$desc"
  else
    _fail "$desc (file should not exist: $path)"
  fi
}

_assert_file_content() {
  local desc="$1" expected="$2" path="$3"
  if [[ -f "$path" ]]; then
    local actual
    actual=$(cat "$path")
    if [[ "$actual" == "$expected" ]]; then
      _pass "$desc"
    else
      _fail "$desc (expected content '$expected', got '$actual')"
    fi
  else
    _fail "$desc (file not found: $path)"
  fi
}

# ---------------------------------------------------------------------------
# Temp directory management
# ---------------------------------------------------------------------------

_TEST_TMP_ROOT=$(mktemp -d) || {
  echo "failed to create test temp root" >&2
  exit 1
}
if [[ -z "$_TEST_TMP_ROOT" || ! -d "$_TEST_TMP_ROOT" ]]; then
  echo "mktemp returned invalid test temp root: $_TEST_TMP_ROOT" >&2
  exit 1
fi
CLEANUP_DIRS+=("$_TEST_TMP_ROOT")

# A managed dotfiles installation exposes `nvim` through a HOME-aware launcher.
# Resolve its real payload before replacing HOME, so repository tests remain
# isolated without accidentally turning the user's launcher into the test
# subject. Distribution CI already resolves directly to its packaged binary.
if [[ -z ${TERMNAV_TEST_NVIM_BINARY:-} ]]; then
  if [[ -x ${HOME:-}/.local/share/neovim/neovim/bin/nvim ]]; then
    TERMNAV_TEST_NVIM_BINARY=$HOME/.local/share/neovim/neovim/bin/nvim
  else
    TERMNAV_TEST_NVIM_BINARY=$(command -v nvim 2>/dev/null || true)
  fi
fi
if [[ -n $TERMNAV_TEST_NVIM_BINARY ]]; then
  export TERMNAV_TEST_NVIM_BINARY
  PATH="$(dirname -- "$TERMNAV_TEST_NVIM_BINARY"):$PATH"
  export PATH
fi
if [[ -n ${TERMNAV_TEST_BINARY:-} ]]; then
  PATH="$(cd -- "$(dirname -- "$TERMNAV_TEST_BINARY")" && pwd -P):$PATH"
  export PATH
fi

# Native Termnav commands can reach tmux clients, Neovim sockets, and SSH
# transports directly. A forgotten mock must therefore fail inside disposable
# state instead of falling through to the developer's live terminal topology.
# Put every conventional state root under this suite's owned directory and
# clear routing metadata before any test command can observe it. Individual
# cases opt back into tmux, SSH, or editor context explicitly with their own
# fixtures.
export HOME="$_TEST_TMP_ROOT/home"
export XDG_CONFIG_HOME="$_TEST_TMP_ROOT/config"
export XDG_DATA_HOME="$_TEST_TMP_ROOT/data"
export XDG_STATE_HOME="$_TEST_TMP_ROOT/state"
export XDG_CACHE_HOME="$_TEST_TMP_ROOT/cache"
export XDG_RUNTIME_DIR="$_TEST_TMP_ROOT/runtime"
export TMPDIR="$_TEST_TMP_ROOT/tmp"
mkdir -p \
  "$HOME" \
  "$XDG_CONFIG_HOME" \
  "$XDG_DATA_HOME" \
  "$XDG_STATE_HOME" \
  "$XDG_CACHE_HOME" \
  "$XDG_RUNTIME_DIR" \
  "$TMPDIR"
chmod 0700 "$XDG_RUNTIME_DIR"

unset \
  TMUX TMUX_PANE \
  NVIM NVIM_LISTEN_ADDRESS \
  SSH_CLIENT SSH_CONNECTION SSH_TTY \
  WEZTERM_PANE TERM_PROGRAM \
  TERMNAV_REMOTE_CWD TERMNAV_REMOTE_LINK_HOST TERMNAV_REMOTE_TMUX

# Git and subprocesses launched by a test share the same isolation boundary.
# An ordinary file is intentional: tests may safely exercise global writes,
# whereas /dev/null would turn those into unrelated failures.
export GIT_CONFIG_GLOBAL="$_TEST_TMP_ROOT/gitconfig"
touch "$GIT_CONFIG_GLOBAL"

_tmpdir() {
  local d
  d=$(mktemp -d "$_TEST_TMP_ROOT/tmp.XXXXXX") || {
    echo "failed to create test temp directory" >&2
    exit 1
  }
  if [[ -z "$d" || "$d" != "$_TEST_TMP_ROOT"/* || ! -d "$d" ]]; then
    echo "mktemp returned invalid test temp directory: $d" >&2
    exit 1
  fi
  echo "$d"
}

_cleanup() {
  for d in "${CLEANUP_DIRS[@]+"${CLEANUP_DIRS[@]}"}"; do
    rm -rf "$d"
  done
}
trap _cleanup EXIT

# ---------------------------------------------------------------------------
# Common test setup
# ---------------------------------------------------------------------------

# Create a mock HOME, saving the original. Sets TEST_HOME, REAL_HOME, HOME.
_mock_home() {
  # shellcheck disable=SC2034  # REAL_HOME is used by callers
  REAL_HOME="$HOME"
  TEST_HOME=$(_tmpdir)
  export HOME="$TEST_HOME"
  # Isolate tests from the real global git config (e.g. core.fsmonitor
  # would spawn daemons watching temp work-trees and hang).  Use an
  # empty file, not /dev/null, so git config --global writes succeed.
  export GIT_CONFIG_GLOBAL="$TEST_HOME/.gitconfig-test"
  touch "$GIT_CONFIG_GLOBAL"
}

# Create a temp bin directory for mock commands. Returns the path.
# IMPORTANT: callers must also run `export PATH="$dir:$PATH"` since
# $() runs in a subshell and the export here won't affect the caller.
_mock_bin() {
  local d
  d=$(_tmpdir)
  echo "$d"
}

# Create a test-only executable adapter for a unified Termnav subcommand.
# Production installs intentionally expose no historical command wrappers;
# adapters let long-lived black-box fixtures keep exercising the same argument
# and process boundaries while the asserted executable is the native binary.
_termnav_command_adapter() {
  local target=$1 binary=$2 argument
  shift 2

  {
    printf '#!/usr/bin/env bash\n'
    printf 'exec %q' "$binary"
    for argument in "$@"; do
      printf ' %q' "$argument"
    done
    printf ' "$@"\n'
  } >"$target"
  chmod 0700 "$target"
}

# ---------------------------------------------------------------------------
# Portable timeout wrapper. Python owns a new process group and escalates TERM
# to KILL, giving every supported CI platform the same exit status and cleanup
# contract. GNU timeout remains a bootstrap fallback for environments without
# Python, but it also needs an explicit kill-after bound: TERM alone can leave a
# deliberately stubborn fixture alive forever.
# ---------------------------------------------------------------------------

_with_timeout() {
  local secs="$1"
  shift
  if command -v python3 &>/dev/null; then
    if python3 - "$secs" "$@" <<'PY'; then
import os
import signal
import subprocess
import sys
import time


def seconds(value: str) -> float:
    scale = {"s": 1.0, "m": 60.0, "h": 3600.0}
    if value[-1:] in scale:
        return float(value[:-1]) * scale[value[-1]]
    return float(value)


class SupervisorSignal(Exception):
    def __init__(self, signum: int):
        self.signum = signum


def interrupted(signum: int, _frame: object) -> None:
    raise SupervisorSignal(signum)


def group_alive(group: int) -> bool:
    try:
        os.killpg(group, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def signal_group(group: int, signum: int) -> None:
    try:
        os.killpg(group, signum)
    except ProcessLookupError:
        pass


def stop_group(process: subprocess.Popen[bytes], first_signal: int) -> None:
    # Once cleanup starts, a second terminal signal must not interrupt the only
    # code responsible for the child group. The group identifier remains valid
    # while any descendant survives, even after the direct leader exits.
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal_group(process.pid, first_signal)
    deadline = time.monotonic() + 1.0
    while group_alive(process.pid) and time.monotonic() < deadline:
        process.poll()
        time.sleep(0.01)
    if group_alive(process.pid):
        signal_group(process.pid, signal.SIGKILL)
    process.wait()


signal.signal(signal.SIGINT, interrupted)
signal.signal(signal.SIGTERM, interrupted)
process = None
try:
    blocked = {signal.SIGINT, signal.SIGTERM}
    previous_mask = signal.pthread_sigmask(signal.SIG_BLOCK, blocked)
    try:
        process = subprocess.Popen(
            sys.argv[2:],
            stdin=subprocess.DEVNULL,
            start_new_session=True,
        )
    finally:
        # A pending signal is delivered only after `process` owns the new group,
        # closing the otherwise unavoidable spawn-before-assignment leak window.
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
    try:
        raise SystemExit(process.wait(timeout=seconds(sys.argv[1])))
    except subprocess.TimeoutExpired:
        stop_group(process, signal.SIGTERM)
        raise SystemExit(124)
    except SupervisorSignal as caught:
        stop_group(process, caught.signum)
        raise SystemExit(128 + caught.signum)
except SupervisorSignal as caught:
    if process is not None:
        stop_group(process, caught.signum)
    raise SystemExit(128 + caught.signum)
PY
      return 0
    else
      return $?
    fi
  elif command -v timeout &>/dev/null; then
    if timeout --kill-after=1s "$secs" "$@"; then
      return 0
    else
      local status=$?
      [[ $status -eq 137 ]] && return 124
      return "$status"
    fi
  elif command -v gtimeout &>/dev/null; then
    if gtimeout --kill-after=1s "$secs" "$@"; then
      return 0
    else
      local status=$?
      [[ $status -eq 137 ]] && return 124
      return "$status"
    fi
  else
    printf 'test timeout requires timeout, gtimeout, or python3\n' >&2
    return 127
  fi
}

# Wait until an AF_UNIX stream socket accepts connections. A socket path exists
# as soon as bind() returns, which can precede listen() under scheduler load.
_wait_for_unix_listener() {
  local path="$1" timeout_seconds="${2:-2}"

  python3 - "$path" "$timeout_seconds" <<'PY'
import socket
import sys
import time

path = sys.argv[1]
deadline = time.monotonic() + float(sys.argv[2])
last_error = "listener did not become ready"

while True:
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.connect(path)
        raise SystemExit(0)
    except OSError as error:
        last_error = str(error)

    if time.monotonic() >= deadline:
        print(
            f"timed out waiting for Unix listener {path}: {last_error}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    time.sleep(0.02)
PY
}

# ---------------------------------------------------------------------------
# Platform checks
# ---------------------------------------------------------------------------

# Check if prebuilt tool binaries will work on this platform. macOS
# ships native binaries; the concern is musl-based Linux (Alpine)
# where glibc-linked binaries fail.
_has_compatible_libc() {
  [[ "$(uname -s)" != "Linux" ]] && return 0
  # Do not use `grep -q` here: with pipefail enabled, grep can exit early
  # after a match and make verbose `ldd` implementations fail with SIGPIPE.
  ldd --version 2>&1 | grep -iE 'glibc|gnu libc' >/dev/null 2>&1
}
# Skip the entire test suite only on Linux libc variants that cannot run the
# prebuilt tools used by these fixtures. macOS remains in coverage.
_require_compatible_libc() {
  if ! _has_compatible_libc; then
    echo "SKIP: $1 (requires glibc-compatible Linux libc)"
    exit 0
  fi
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

_test_summary() {
  echo ""
  if $_TEST_PRETTY; then
    local summary_color=green
    [[ $FAIL -ne 0 ]] && summary_color=red
    _test_style "$summary_color" "────────────────────────────────"
    if [[ $FAIL -eq 0 ]]; then
      _test_style green "✓ Results: $PASS passed, $FAIL failed"
    else
      _test_style red "✗ Results: $PASS passed, $FAIL failed"
    fi
    _test_style "$summary_color" "────────────────────────────────"
  else
    echo "================================"
    echo "Results: $PASS passed, $FAIL failed"
    echo "================================"
  fi
  [[ $FAIL -eq 0 ]] && exit 0 || exit 1
}
