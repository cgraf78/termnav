# ControlMaster-safe relay transport

> Superseded on 2026-08-17 by the per-session `SendEnv` transport. The shipped
> implementation no longer occupies the remote command channel or launches a
> replacement login shell; see `termnav-relay(1)` for the current contract.

## Problem

`termnav-relay ssh` currently publishes a per-session remote relay socket with
OpenSSH `SetEnv`. A reused ControlMaster retains the environment configuration
from the master connection, so later multiplexed sessions receive a missing or
stale `TERMNAV_PARENT_RELAY`. The request then fails closed even though the new
remote forwarding exists.

## Design

Keep the user's configured ControlMaster and the existing per-session Unix
socket forwarding. For commandless interactive sessions only, carry the current
remote socket in a safely quoted remote bootstrap command, force the TTY mode
that the original commandless login would have requested, set
`TERMNAV_PARENT_RELAY` through `env`, and immediately execute the remote login
shell.

The bootstrap must not depend on a pre-existing remote Termnav installation.
It must use the command syntax shared by sh, zsh, fish, and csh-family login
shells and preserve the SSH process's exit status. Explicit or configured
commands, subsystems, forwarding/control modes, and noninteractive sessions
remain ordinary byte-for-byte SSH invocations.

`SetEnv` is retained temporarily only if required for compatibility with a
fresh non-multiplexed connection; the bootstrap value is authoritative. If the
bootstrap or forwarding cannot be established, navigation remains fail closed.

## Safety and compatibility

- Never disable ControlMaster or create a second authentication attempt.
- Never run a separate SSH probe before the user's requested connection.
- Do not interpret or execute user-provided strings locally.
- Restrict the generated remote socket to the fixed CSPRNG-hex path format.
- Preserve arbitrary SSH destinations and option ordering.
- Do not enhance explicit commands, `-N`, `-T`, `-W`, `-O`, `-G`, `-f`,
  subsystem sessions, or effective `RequestTTY no` configurations.
- Continue cancelling the exact remote forwarding after the session ends.
- Document that a server-side `ForceCommand` sees the bootstrap in
  `SSH_ORIGINAL_COMMAND` and may reject the enhanced login.

## Verification

Add a deterministic integration fixture that models a persistent master created
before Termnav, opens sequential relay sessions through it, and proves each
remote session sees its own live socket without another authentication or
connection. Retain argument-contract tests for delegated modes. Run the full
Termnav suite, reproduce the stale-master case against real OpenSSH, and repeat
the NAS to Bevo2 to Taylor live chord matrix.
