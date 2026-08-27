# Plan: Replace Termnav's executable surface with one Rust binary

**Goal**: Ship one native `termnav` binary implementation for every executable
feature in the repository, remove processes whose only purpose is amortizing
Python startup, retain the connection-scoped process required by SSH reverse
forwarding, and preserve all navigation, focus, link-routing, and
editor-opening behavior.

**Architecture**: A typed Rust library owns policy and platform adapters. One
`termnav` CLI exposes cohesive subcommands. Shell, Lua, tmux, Neovim, and VS
Code files remain thin integration assets and invoke that binary. The SSH
subcommand supervises the real SSH child and serves the per-connection Unix
relay itself; there is no detached hot service and no separate relay child.

**Tech stack**: Rust 2024 with a declared MSRV, Bash/Lua integration assets,
real tmux/PTY integration tests, the shared `cgraf78/actions` Rust CI and release
workflows, and the shared timestamp-plus-commit release identity.

## Decisions and invariants

1. Build and ship exactly one compiled implementation and public CLI:
   `termnav`. Keep only two fixed-name integration adapters that external tools
   cannot express as a command plus arguments:
   - the private PATH shim named `ssh`, which executes `termnav ssh "$@"`;
   - `nvim-link-host`, a symlink/multicall alias because ripgrep's
     `--hostname-bin` accepts only one zero-argument executable name.
   Transitional old-name aliases exist only during the staged rollout and are
   removed after dotfiles has migrated.
2. Keep sourceable shell and Lua modules where behavior must affect the caller
   process or editor. They contain integration policy only; reusable command
   behavior belongs in Rust.
3. Remove `hot-serve`, `warm`, `stop`, the curl-based local RPC, and the
   provider-wide idle daemon. Ordinary navigation gestures execute one native
   process.
4. Remove Neovim's `--stream` worker. Local editor and tmux movement stays
   in-process; only boundary navigation starts the native binary.
5. Keep one Termnav process for each enhanced interactive SSH session because
   OpenSSH `RemoteForward` requires a live local listener. `termnav ssh` owns
   both that listener and the real SSH child in one process. It is never
   detached and has no lifetime beyond the connection.
6. Never open an additional authenticated SSH connection for Termnav. Some
   destinations require an interactive Duo approval for every connection. The
   relay must ride the user's one requested SSH session. A ControlMaster
   cancellation operation may contact only the already-resolved local control
   socket and must fail closed rather than falling back to a network login.
   The same rule applies to remote editor reuse: it may open a new mux channel
   through an existing exact ControlPath, but it must never fall back to a new
   transport connection if that master disappears.
7. Preserve the current relay protocol, terminal commit barrier, exact-client
   validation, fail-closed shared-client rules, arbitrary nesting, and
   per-session ControlMaster forwarding behavior. Local and remote hosts update
   independently, so the Rust and Python implementations must interoperate in
   both directions throughout the rollout.
8. Preserve nested-focus lease semantics initially. Focus watcher and expiry
   behavior becomes Rust subcommands of the same binary. Removing those
   semantic liveness mechanisms is a separate optimization and is not required
   to remove performance-only residency.
9. Use a pure-Rust dependency graph where practical so Linux musl and Android
   packages need no native build toolchain beyond the shared workflow defaults.
10. Model release identity and packaging after `shdeps` and `grafhome-ca`:
   `YYYYMMDD-HHMMSS-<8hex>`, embedded commit/version, vendored shared release
   scripts, generated standalone installer, static Linux archives, native
   macOS archives, and Android AArch64 plus x86_64 artifacts.
11. Keep performance tests product-owned. Reuse shared workflow orchestration,
    but compare the one-shot Rust candidate with the current resident Termnav
    implementation using the same real tmux topology.
12. Keep the owner-protected Unix listener. A unique stale pathname after an
    uncatchable death is safer and simpler than adding a loopback TCP listener,
    secret distribution, and a new incompatible wire contract.
13. Use a staged rollout with independently green PRs and releases:
    - Actions PR first, only if required by concrete CI/installer gaps;
    - Termnav provider PR: add the Rust binary and temporary old-name aliases,
      pinning the exact reviewed Actions commit when applicable;
    - dotfiles consumer PR: migrate every caller to the new CLI;
    - Termnav cleanup PR: remove transitional aliases and Python code after the
      supported consumer fleet has actually migrated or an explicitly approved
      compatibility window has ended.
    The permanent `ssh` shim and `nvim-link-host` alias are not compatibility
    debris and remain tested interfaces.

## Public command map

The final public surface is one compiled binary with these subcommands:

| Current executable | New command |
| --- | --- |
| `termnav-navigate ...` | `termnav navigate ...` |
| `termnav-relay send ...` | `termnav relay send ...` |
| `termnav-relay serve ...` | hidden `termnav relay serve ...` |
| `termnav-relay commit ...` | hidden `termnav relay commit ...` |
| `termnav-relay sweep` | hidden `termnav relay sweep` |
| `termnav-relay ssh ...` | `termnav ssh ...` |
| `termnav-tmux-context ...` | `termnav tmux context ...` |
| `termnav-tmux-focus ...` | `termnav tmux focus ...` |
| `tmux-follow-click ...` | `termnav tmux follow-click ...` |
| `nvim-tmux-open ...` | `termnav nvim open ...` |
| `nvim-ssh-control-open ...` | hidden `termnav nvim ssh-open ...` |
| `nvim-link-host` | permanent multicall alias for `termnav nvim link-host` |
| `vscode-nvim-focus ...` | `termnav vscode focus ...` |
| `eza-nvim-links ...` | `termnav eza ...` |

The SSH PATH shim remains a non-user-facing script because interception
requires an executable named `ssh`; it contains only `exec termnav ssh "$@"`.
All other old command names are temporary rollout aliases and are removed in
the cleanup PR.

## Performance acceptance criteria

Measure optimized binaries after five warmups with at least 50 alternating
samples. Record median and p95. CI uses calibrated relative and absolute limits
so shared-runner noise cannot hide a large regression or fail on insignificant
jitter.

- `termnav relay send` with no parent: median no slower than the current warm
  service plus 20%, and p95 no slower than current plus 5 ms.
- dead parent-relay socket: same limits as send-decline; no retry may duplicate
  a gesture.
- stray terminal commit: same limits as the current warm service.
- real two-level tmux boundary: median no slower than the current resident
  path plus 20%, p95 no slower than current plus 10 ms, and hard ceilings of
  25 ms median / 45 ms p95 on the calibrated Ubuntu runner.
- Neovim boundary gesture, measured from key dispatch until the destination
  tmux selection is observable: no slower than the current resident stream
  plus 20%, with hard ceilings of 20 ms median / 35 ms p95 on the calibrated
  Ubuntu runner. Local Neovim window movement remains process-free.
- rapid ordered burst of 100 boundary gestures: no dropped, duplicated, or
  reordered requests; at most one in-flight child and 100 queued requests;
  peak Termnav RSS below 32 MiB; final completion within 2.5 seconds; no child,
  queue item, socket, or lock remains afterward.
- shell activation: starts no Termnav process and performs no Termnav command.
- after any non-SSH command exits, no Termnav child, socket, or lock owned only
  by that invocation remains.

If the first native prototype cannot satisfy the current resident boundary
latency within these budgets, stop before porting the remaining commands and
report the measured blocker.

## Dependency order

| Group | Steps | Dependency |
| --- | --- | --- |
| A | 1-4 | Current `origin/main` only |
| B | 5-14 | Approved plan and characterization commits |
| C | 15-21 | Complete native behavior |
| D | 22-24 | CI, release, and source-install contract |
| E | 25-26 | Any Actions dependency, then additive provider PR |
| F | 27-29 | Dotfiles consumer migration |
| G | 30-31 | Provider cleanup after consumer migration |
| H | 32-36 | Final verification, review, and PR monitoring |

## Implementation steps

### 1. Record the current process and CLI contract

**Files**: `test/suites/process-contract-test`, `test/termnav-test`

- Add a test that inventories every current executable and classifies whether
  it is expected to leave a process after returning.
- Characterize externally observable navigation ordering, latency measurement
  points, lifecycle ownership, and cleanup without treating the current hot
  service or Neovim stream as required behavior.
- Assert an enhanced SSH session owns exactly the wrapper, SSH child, and relay
  child before the integration change.
- Run the focused suite and confirm it passes against current `origin/main`.
- Commit these characterization tests separately.

### 2. Add failing no-residency acceptance tests

**Files**: `test/suites/process-contract-test`,
`test/suites/nvim-navigation-test`, `test/suites/relay-performance-test`

- Add separate tests for the desired architecture: shell activation creates no
  Termnav process, navigation leaves no detached service, and Neovim boundary
  navigation has no persistent worker.
- Run them against current `origin/main` and record the expected failures before
  changing production code.
- Do not commit production changes until these tests have failed for the
  intended reason.

### 3. Lock public command behavior before changing languages

**Files**: `test/suites/cli-contract-test`, existing `test/suites/*-test`

- Add table-driven black-box cases for help, invalid input, exit status,
  stdout/stderr, environment overrides, and command-specific edge cases.
- Cover every command in the public-command map, including aliases that are
  about to be removed.
- Exercise real programs when practical; use complete boundary fakes only for
  SSH, VS Code sockets, and unavailable GUI APIs.
- Verify the tests pass against current code and commit them separately.

### 4. Lock state and protocol compatibility

**Files**: `test/suites/relay-test`, `test/suites/tmux-focus-test`,
`test/suites/nvim-test`, `test/suites/vscode-focus-test`

- Add missing fixtures for malformed, oversized, truncated, duplicated, and
  reordered relay messages.
- Lock directive-store layout, nonce ownership, PID reuse, permissions,
  symlink rejection, and stale-record recovery.
- Lock focus lease generations, shared inner sessions, multiple outer clients,
  and arbitrary nesting.
- Add black-box mixed-version fixtures that retain the Python implementation
  and define the peer-harness interface. Activate the cross-language cases once
  the Rust relay client/server exist; they cannot pass during characterization.
  Execute:
  - Rust remote client against Python local server;
  - Python remote client against Rust local server;
  - mixed Rust/Python chains at two and three nesting levels;
  - navigation, focus, prepare/abort/commit, lost replies, and exact-client
    provenance;
  - concurrent per-session forwards sharing one ControlMaster.
- Commit characterization coverage separately from the Rust implementation.

### 5. Capture the current performance baseline

**Files**: `test/relay-performance.py`, `test/suites/relay-performance-test`

- Add machine-readable JSON output containing command, commit, environment,
  sample count, median, p95, minimum, and maximum.
- Add explicit process-start, no-parent, dead-socket, active-relay, commit,
  nested-tmux, Neovim boundary, and ordered-burst scenarios.
- Add process-residue assertions outside the timed region.
- Run at least 50 alternating samples against `45b32be` and preserve the
  resulting baseline artifact in the PR description, not as generated source.

### 6. Add the Rust package and typed CLI skeleton

**Files**: `Cargo.toml`, `Cargo.lock`, `build.rs`, `src/main.rs`, `src/cli.rs`,
`src/version.rs`

- Add failing Rust integration tests for `termnav version`, top-level help,
  subcommand help, invalid commands, and stable exit codes.
- Use Rust 2024 and declare the same practical MSRV convention as `shdeps`.
- Embed the timestamp/hash release identity through `build.rs` and
  `src/version.rs` using the shared release-version script contract.
- Keep command parsing in `src/cli.rs`; subcommands call typed library APIs.
- Run `cargo fmt`, `cargo test --locked`, Clippy, and rustdoc.

### 7. Add reusable platform and process adapters

**Files**: `src/process.rs`, `src/fs.rs`, `src/tmux.rs`, tests in each module

- Write failing tests for procfs and BSD `ps` snapshots, environment parsing,
  PID reuse checks, tty validation, command timeouts, and explicit tmux sockets.
- Implement typed process identities and a tmux runner that never inherits the
  caller's `TMUX` accidentally.
- Keep filesystem permission, symlink, ownership, and bounded-path validation
  in one module.

### 8. Port navigation policy without subprocess details

**Files**: `src/navigation/model.rs`, `src/navigation/policy.rs`, unit tests

- Translate current Python navigation model and decision tests into Rust before
  wiring real tmux calls.
- Preserve local-scope-first ordering, exact-client revalidation, shared-session
  handling, recent-focus bounds, process-cycle detection, and arbitrary depth.
- Use enums for actions, directions, outcomes, and client provenance.

### 9. Wire the one-shot navigation CLI

**Files**: `src/navigation/backend.rs`, `src/commands/navigate.rs`, integration tests

- Add failing real-tmux tests invoking `termnav navigate` for local selection,
  parent traversal, tab selection/movement, inactive source rejection, and
  multiple attached clients.
- Implement the backend using explicit tmux argument vectors.
- Prove no daemon or worker is started by any navigation command.

### 10. Port relay wire validation and client transport

**Files**: `src/relay/protocol.rs`, `src/relay/client.rs`, unit tests

- Add failing tests for every protocol operation, framing limit, timeout,
  malformed JSON, unexpected replies, and tri-state status mapping.
- Implement bounded Unix-socket request/reply handling with typed messages.
- Preserve fail-closed behavior after ambiguous receive-side failures.

### 11. Port the directive store and terminal commit barrier

**Files**: `src/relay/store.rs`, `src/relay/commit.rs`, unit/integration tests

- Translate the existing store tests first, including concurrency, poisoning,
  atomic publication, permissions, lost replies, and reused client identity.
- Implement locked, owner-only state and the DECRQM/User8-User13 commit path.
- Run real tmux/PTY tests at one, two, and three nesting levels.

### 12. Port relay routing and the connection server

**Files**: `src/relay/server.rs`, `src/commands/relay.rs`, integration tests

- Add failing tests for local handling, parent forwarding, prepare/abort/commit
  ordering, stalled peers, fragmented messages, and concurrent clients.
- Implement a bounded Unix listener with ordered navigation and independent
  focus handling.
- Keep `relay serve` hidden for tests and internal composition.
- Activate the bidirectional and multi-depth Rust/Python peer tests defined in
  Step 4 before the additive provider PR. Treat failures as protocol blockers,
  not as reasons to weaken the fixtures.

### 13. Integrate relay serving into `termnav ssh`

**Files**: `src/ssh.rs`, `src/commands/ssh.rs`, SSH lifecycle tests

- Add failing tests for option parsing, configured TTY policy, remote commands,
  ControlMaster sessions, SendEnv, forward cancellation, and ordinary SSH
  passthrough.
- Prove every enhanced invocation launches exactly one user-requested SSH
  session command. It may open one network connection or reuse an existing
  ControlMaster, but Termnav never launches a second ordinary SSH command.
  Test that ControlMaster cancellation addresses only the existing local
  control socket, and that an unavailable control socket never triggers a
  fallback connection or authentication attempt. The failure-path fixture must
  install a network/`ProxyCommand` sentinel and fail if it is touched, proving
  that Termnav cannot cause a second Duo prompt.
- Add lifecycle tests for normal exit, startup failure, SSH child failure,
  wrapper `INT`, wrapper `TERM`, wrapper `KILL`, and two concurrent sessions.
- Add a shared-ControlMaster case with two live Termnav sessions and prove
  exact-forward cancellation cannot cancel or unlink the sibling session.
- Implement one foreground supervisor that owns the listener and SSH child.
- Define an explicit signal/process-group state machine. Do not double-forward
  terminal-generated signals when the SSH child already shares the foreground
  process group.
- On normal exit and catchable signals, reap the exact child, cancel only the
  exact owned remote forward, and remove only the owned local socket.
- On supervisor `SIGKILL`, assert only what the kernel can guarantee: the relay
  listener FD closes and no Termnav relay descendant remains. The OpenSSH child
  may survive according to OpenSSH/ControlMaster ownership, and the inert
  unique Unix pathname may remain. The next invocation safely identifies and
  sweeps only stale Termnav-owned paths; it never performs network cleanup.

### 14. Remove the provider-wide hot service

**Files**: delete `lib/termnav/hot_service.py`; simplify `src/commands/relay.rs`,
`share/termnav/shell.sh`, tests and documentation

- Add a failing assertion that shell activation starts no Termnav process and
  creates no hot-service socket.
- Remove warm/stop/hot-serve commands, curl transport, provider hashing,
  staleness checks, idle retirement, and hot-service tests.
- Keep stale per-SSH socket sweeping only where the connection lifecycle needs
  crash recovery.

### 15. Remove Neovim's resident navigation worker

**Files**: `lib/termnav/nvim/navigation.lua`,
`test/support/nvim-navigation.lua`, `test/suites/nvim-navigation-test`

- Add failing tests that boundary navigation invokes `termnav navigate` once
  and retains ordered rapid-key behavior without a persistent job.
- Replace stream creation with a bounded FIFO dispatcher that permits exactly
  one in-flight one-shot process. New boundary gestures enqueue in key order;
  completion starts the next item. Rejecting work after the explicit queue
  bound must be visible and must never silently reorder or duplicate gestures.
- Verify local Neovim window movement still starts no external process.
- Verify editor shutdown has no Termnav worker to reap.

### 16. Port nested tmux focus commands

**Files**: `src/focus/model.rs`, `src/focus/state.rs`,
`src/commands/tmux_focus.rs`, Rust and existing real-tmux tests

- Translate focus claim/release/restore tests before implementation.
- Preserve one-hop composition, exact tokens, lease bounds, pane-local style
  restoration, refocus races, and shared-session independence.
- Keep watcher and expirer processes connection/client scoped for this port;
  verify deduplication and bounded cleanup rather than changing semantics.

### 17. Port tmux context publication

**Files**: `src/commands/tmux_context.rs`, integration tests

- Add failing tests for direct terminals, nested passthrough, screen terminfo,
  control-mode clients, invalid tty metadata, and exact emitted bytes.
- Implement the current output framing without shell subprocess parsing.

### 18. Port Neovim opening and remote-control commands

**Files**: `src/nvim/*`, `src/commands/nvim.rs`, integration tests

- Translate registry selection, atomic current-owner publication, symlink
  rejection, RPC fallback ordering, path escaping, line/column parsing,
  existing-ControlMaster requirements, and remote PATH behavior.
- Replace the current check-then-session ControlMaster sequence with one
  race-free mux-only session attempt: resolve the exact ControlPath once, pass
  it explicitly, and override the ordinary connection path with a
  Termnav-controlled deterministic local-failure `ProxyCommand`. Also disable
  canonicalization, proxy jumping, forwarding, and every authentication method
  on that invocation. If the master vanishes before or during the request, the
  local failure command runs and the command returns the existing fallback
  status. The
  user's configured `ProxyCommand`, ProxyJump, DNS/canonicalization path,
  destination socket, and authentication providers are never reached.
- Test both an absent master and a master removed at the former check/session
  race point. Assert that the Termnav local failure command runs, while
  independent sentinels fail the test if the user's proxy/jump, destination
  network, or any interactive authentication path is attempted.
- Keep sourceable `lib/termnav/nvim-open/launcher.sh` only where it must decide
  whether the caller shell should `exec` a new editor; delegate reuse attempts
  to `termnav nvim open`.
- Delete obsolete executable shell scripts after all callers move.

### 19. Port click, link, eza, and VS Code commands

**Files**: `src/links/*`, `src/commands/eza.rs`,
`src/commands/follow_click.rs`, `src/commands/vscode.rs`, integration tests

- Preserve URL/file/token precedence, wrapped-line reconstruction, hyperlink
  decoding, remote host handling, terminal width, eza color/grid behavior,
  VS Code authentication, ancestry validation, and bounded errors.
- Invoke optional shell token detectors through a documented data-oriented
  subprocess boundary; do not duplicate their policy in Rust.
- Delete the replaced `bin/` programs once the new subcommands pass their
  characterization suites.

### 20. Collapse executable documentation

**Files**: `README.md`, `lib/termnav/README.md`, `man/man1/termnav.1`, examples

- Replace ten executable man pages with one `termnav(1)` page organized by
  subcommand.
- Update examples and integration documentation to use `termnav ...`.
- Document that only `termnav ssh` and focus liveness commands may remain alive,
  always attached to a concrete connection/client lease.

### 21. Add native performance regression coverage

**Files**: `test/relay-performance.py`, `test/suites/relay-performance-test`,
`.github/workflows/test.yml`

- Compare the optimized Rust binary against the parent commit's warmed Python
  implementation using alternating samples and identical topology.
- Add absolute one-shot startup, relay-send, commit, parent navigation,
  Neovim-boundary, and burst budgets.
- Fail if a non-SSH benchmark leaves a process or socket after its command.
- Keep performance CI separate and serialized so other test jobs do not distort
  the samples.

### 22. Adopt shared Rust CI

**Files**: `.github/workflows/test.yml`, `.github/dependabot.yml`,
`.github/cgraf78-actions.lock`, `.github/shellcheck-files.txt`, CI helper scripts

- Invoke `cgraf78/actions/.github/workflows/rust-ci.yml` at the locked Actions
  commit with full platform coverage because process and tty behavior is
  platform-sensitive.
- Run Cargo tests plus applicable shell/Lua/tmux integration tests against the
  built binary on Linux, macOS, WSL, and Termux.
- Keep a focused shared Shell CI call for sourceable shell assets and ShellCheck
  only where it adds coverage not duplicated by Rust CI.
- Retain current-eza compatibility and the product-specific performance job.
- Add Cargo Dependabot updates alongside the existing Actions updates.
- Use this explicit platform contract:

  | Platform | Setup | Required Termnav coverage |
  | --- | --- | --- |
  | Ubuntu/glibc | shared profiles for Python, zsh, Lua, Neovim, tmux, OpenSSH, netcat, lsof, procps, eza | full Rust, shell/Lua, real tmux/PTY, fake-network SSH, mixed-version, performance |
  | Ubuntu/musl package | shared Rust package job | archive/install smoke, CLI/version, no native dependencies |
  | macOS x86_64/arm64 | Homebrew packages selected by the same named profiles | Rust, shell/Lua, real tmux/PTY, fake-network SSH, process/`ps` adapters |
  | WSL | existing WSL runner plus the named integration profiles | Rust, shell/Lua, real tmux/PTY, fake-network SSH, path/socket semantics |
  | Android package | shared cross build | archive layout, binary identity, dependency audit; no tmux/SSH runtime claim |
  | Termux x86_64 | `pkg install` script for tmux, neovim, lua, openssh, zsh, procps, eza | native Rust tests selected for Termux, CLI, shell/Lua, real tmux; boundary fakes for unavailable GUI surfaces |

- Add an Actions PR only for concrete reusable gaps: named dependency profiles
  in Rust CI and transactional binary-alias installation. Reuse the existing
  `shell-ci-prereqs` package knowledge rather than copying package-manager logic
  into Termnav.

### 23. Adopt shared release/version infrastructure

**Files**: `build.rs`, `src/version.rs`, `.github/workflows/release.yml`,
`scripts/release.conf`, synchronized shared scripts, `install.sh`

- Run `consumer-ci/sync.sh` from the clean, reviewed Actions checkout.
- Configure one binary named `termnav`, standalone installer generation, all
  required shell/Lua/VS Code/example assets, and one `termnav(1)` man page.
- Publish Linux musl x86_64/AArch64, macOS x86_64/AArch64, and Android
  x86_64/AArch64 archives.
- Add release config, archive layout, embedded version, local archive install,
  and smoke tests modeled on `shdeps`, `hive-memory`, and `grafhome-ca`.
- Configure installer-created links transactionally for permanent
  `nvim-link-host` and, only in the additive rollout release, the temporary old
  names. Package the private `share/termnav/shims/ssh` adapter as data.

### 24. Verify source and release installation

**Files**: `test/suites/install-test`, `tests/shell/install-test`, release tests

- Treat `cargo install --path . --locked` as a binary-only smoke test, not a
  complete supported installation.
- Test generated `install.sh --archive` transactionality, collisions,
  idempotence, payload availability, command execution, and uninstallation if
  supported by the shared installer contract.
- Replace `support/install-checkout.sh` with a locked source installer that
  builds the release binary and installs all required assets and fixed-name
  links. Test it from a clean checkout with an empty Cargo target/cache path.
- Smoke the packaged binary's version and a no-side-effect command.

### 25. Publish any required Actions PR

- Resolve this conditional dependency before publishing the additive Termnav
  provider PR. If Steps 22-24 prove the named-profile or transactional-alias
  gaps, implement them generically with consumer-sync tests and a
  representative fixture.
- Create, verify, and monitor the independent Actions PR to green without
  merging it. Do not add Termnav-specific policy to the shared repository.
- Termnav may pin the exact reviewed branch commit for PR validation, but no
  dependent Termnav release may publish until that Actions commit has landed on
  its default branch and the lock has been refreshed to the final immutable SHA.

### 26. Publish the additive Termnav provider PR

- Retain the Python implementation as the mixed-version oracle until the
  cleanup PR, but make the Rust binary authoritative for the new CLI.
- Install temporary multicall aliases for all old command names so existing
  dotfiles continue to function during rollout.
- Create, verify, and monitor the PR to green without merging it. After merge
  authorization in a later turn, publish and smoke a release before migrating
  dotfiles consumers. If Step 25 produced an Actions dependency, do not publish
  this release until that dependency has landed and Termnav pins its final SHA.

### 27. Update dotfiles to the unified command surface

**Repository**: dotfiles, separate worktree and PR based on current
`origin/main` after the Termnav PR is published.

**Files**: `.config/tmux/tmux.conf`, `.config/nvim/lua/config/termnav.lua`,
`.config/wezterm/termnav-module.lua`, `.config/ripgrep/config`,
`.config/shell/interactive.d/50-aliases.sh`, tests and READMEs

- Replace every old executable reference with the corresponding `termnav`
  subcommand, except the permanent ripgrep `nvim-link-host` interface and the
  private SSH shim.
- Keep local navigation fast paths and only invoke the binary at scope
  boundaries.
- Remove assumptions about the warmed daemon and old executable inventory.

### 28. Add the mandatory dotfiles source-checkout build hook

**Files**: `.config/shdeps/hooks.d/cgraf78/termnav.sh`, shdeps hook tests

- Confirm the `github` method selects release artifacts once Termnav releases
  exist.
- For both automatic `github` to `github:repo` fallback and explicitly selected
  development checkouts, invoke Termnav's provider-owned locked source installer
  and expose the resulting binary, assets, permanent adapters, and temporary
  rollout aliases through the normal shdeps command path.
- Keep release installs on the generic shdeps path; the hook must not duplicate
  installer or platform-selection logic.

### 29. Run paired provider/consumer tests

**Commands**:

```bash
DOT_TEST_TERMNAV_ROOT=<termnav-worktree> dot test
cargo test --locked
test/termnav-test
test/suites/relay-performance-test
```

- Exercise dotfiles directly against the candidate binary before any release
  exists.
- Verify tmux configuration reload, Neovim Lua behavior, WezTerm modules,
  shell activation, SSH shim resolution, and old-command absence.

### 30. Publish the Termnav cleanup PR

- Base it on the additive provider work after the dotfiles migration is ready.
- Delete the Python implementation, temporary old-name aliases, obsolete tests,
  and compatibility-only packaging. Retain only `termnav`, `nvim-link-host`,
  the private SSH shim, and non-executable integration assets.
- Before deletion, freeze the minimum Python relay client/server needed for
  stateful mixed-version coverage under `test/support/python-peer/`. Continue
  executing both interop directions, two- and three-level mixed chains,
  prepare/abort/commit, lost replies, exact-client provenance, and concurrent
  ControlMaster forwards in CI. The frozen peer is never packaged, installed,
  or reachable from production dispatch.
- Add a repository-wide code/docs/tests/config/CI/generated-facing reference
  gate with an explicit allowlist for the two permanent fixed-name adapters and
  the frozen test-only Python peer.
- Create, verify, and monitor the cleanup PR to green without merging it.
- Mark merge and release as deployment-gated. They require explicit
  confirmation that every supported dotfiles installation has consumed the
  migration; “the dotfiles PR is merged” or “the release is available” is not
  sufficient. Until then, the additive provider release with old aliases
  remains the latest published Termnav version.

### 31. Perform final process-lifecycle audit

- Run real isolated tmux/Neovim/SSH fixtures without user sessions.
- Assert no hot service exists, no Neovim worker exists, and each SSH/focus
  helper is uniquely owned and removed on normal exit, failure, `INT`, `TERM`,
  and `KILL` where the kernel permits cleanup.
- Verify concurrent sessions cannot terminate or unlink one another.

### 32. Perform final performance audit

- Run at least 100 alternating samples locally against the current resident
  baseline and report median, p95, minimum, maximum, and percent change.
- Run under representative tmux nesting and rapid repeated chords.
- Inspect process count, peak RSS, and syscall/process-launch summaries outside
  the timed benchmark.
- Reject the migration if meaningful gesture paths regress beyond the stated
  budgets.

### 33. Run final quality gates

- `cargo fmt --check`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`
- `RUSTDOCFLAGS='-D missing-docs' cargo doc --locked --no-deps`
- `cargo test --locked`
- repository ShellCheck inventory
- full `test/termnav-test`
- release package and installer smoke tests
- full `dot test` against the Termnav worktree
- `git diff --check` and privacy/secret review

### 34. Obtain blocker-free implementation review

- Request a fresh-eyes review covering semantics, arbitrary nesting, shared
  clients, process ownership, security, performance methodology, packaging,
  portability, and consumer migration.
- Resolve every blocking or important finding and rerun affected verification.
- Repeat until the final diffs receive no blocking findings.

### 35. Verify staged rollout order

- Demonstrate in isolated install roots that each supported state works:
  old dotfiles plus additive provider; new dotfiles plus additive provider; new
  dotfiles plus cleanup provider; old local host plus new remote host; new local
  host plus old remote host.
- Deliberately classify old dotfiles plus cleanup provider as unsupported and
  prove the release gate prevents that state from being published to hosts that
  resolve bare `github` to the latest Termnav release.
- Prove that no supported state launches an extra SSH connection or performs an
  additional authentication/Duo flow.
- Document the required merge/release order in every dependent PR without
  merging any PR during this goal.

### 36. Publish and monitor pull requests

- Create the additive Termnav provider PR, any proven Actions PR, the dotfiles
  consumer PR, and the Termnav cleanup PR described above.
- Verify every remote branch and PR head after pushing.
- Monitor all required checks to success, investigate real failures, rerun only
  classified infrastructure failures, and leave every PR open and unmerged.

## Final acceptance checklist

- Exactly one compiled implementation and public CLI: `termnav`.
- Only the required `nvim-link-host` multicall alias and private `ssh` shim
  remain as fixed-name executable integration points.
- No detached Termnav performance daemon.
- No persistent Neovim navigation worker.
- SSH relay listener exists only inside the connection-owned `termnav ssh`
  supervisor.
- Navigation and focus semantics match the existing tests across arbitrary
  nesting and shared-client topologies.
- Performance matches or improves on the current resident implementation.
- Release identity, archives, installer, and workflows follow the shared Rust
  repository contracts.
- Termnav and dotfiles local suites pass, including `dot test`.
- Mixed Rust/Python versions work across independent host update orderings.
- Termnav never opens a second authenticated SSH connection; failure to reach a
  resolved ControlMaster socket fails closed without touching the network.
- All PR checks are green and no PR is merged.
