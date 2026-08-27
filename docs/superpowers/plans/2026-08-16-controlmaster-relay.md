# ControlMaster-safe Relay Implementation Plan

> Superseded first by the per-session `SendEnv` transport and then, on
> 2026-08-27, by the single-binary Rust implementation. The paths and Python
> stack below are retained only as historical implementation context; see
> `termnav(1)` for the current contract.
>
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep relay metadata correct for every multiplexed interactive SSH session without another connection or authentication.

**Architecture:** Preserve the per-session remote Unix-socket forward, but deliver its current path through the multiplex protocol's per-session command field rather than master-owned environment configuration. A fixed cross-shell bootstrap uses `env` to set the value and execs the remote login shell.

**Tech Stack:** Python 3 standard library, OpenSSH, Bash test harness, tmux.

## Global Constraints

- Preserve ControlMaster reuse and require no additional authentication attempt.
- Enhance only otherwise commandless interactive default sessions.
- Fail closed and preserve existing delegation for every unsupported mode.
- Support Linux, macOS, WSL, container distributions, and Termux.

---

### Task 1: Reproduce stale master metadata

**Files:**

- Modify: `test/suites/relay-test`

**Steps:**

- [x] Add an argument-level regression proving an enhanced login uses per-session command data rather than only `SetEnv`.
- [x] Model a persistent pre-existing master and sequential relay sessions, then reproduce against real OpenSSH during live verification.
- [x] Run `test/suites/relay-test` and verify the new cases fail for stale or absent relay metadata.

### Task 2: Implement per-session bootstrap

**Files:**

- Modify: `bin/termnav-relay`
- Modify: `man/man1/termnav-relay.1`

**Steps:**

- [x] Produce a fixed bootstrap from the constrained CSPRNG-hex socket path.
- [x] Insert the command and required TTY request only in the already-qualified interactive path.
- [x] Preserve forward cancellation, exit codes, arbitrary destinations, and delegated modes.
- [x] Run `test/suites/relay-test` until all regressions pass.

### Task 3: Edge cases and full verification

**Files:**

- Modify: `test/suites/relay-test`
- Modify: `test/README.md` if the new integration dependency needs documentation.

**Steps:**

- [x] Cover stale-master reuse, option-like destinations, common login-shell parsing, configured commands, forced TTY mode, and SSH failure.
- [x] Run `checkrun format`, `checkrun lint`, and `test/termnav-test`.
- [x] Perform fresh-eyes correctness, security, and portability review.
- [ ] Commit, push explicitly, open a separate PR, monitor CI, and live-test a three-host SSH chain without landing.
