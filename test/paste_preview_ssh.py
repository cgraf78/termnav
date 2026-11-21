"""One authenticated SSH hop for the image paste GUI test, using local pipes."""

from __future__ import annotations

import json
import os
import pwd
import shlex
import subprocess
import tempfile
from pathlib import Path


def ssh_command(root: Path, command: list[str], environment: dict[str, str]) -> list[str]:
    """Prepare a private, single-connection sshd and return its PTY client command.

    The caller owns the temporary root and runs the client inside the terminal
    under test. The proxy owns sshd's stdin/stdout, so closing the client also
    closes the transport without a listener or a separate server to stop.
    """
    fixture = Path(tempfile.mkdtemp(prefix="ssh-", dir=root)).resolve()
    user = pwd.getpwuid(os.getuid()).pw_name
    host = "paste-preview.invalid"
    for key in ("host", "user"):
        subprocess.run(
            ["/usr/bin/ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(fixture / key)],
            check=True,
            timeout=10,
        )
    known_hosts = fixture / "known_hosts"
    known_hosts.write_text(f"{host} {(fixture / 'host.pub').read_text()}")

    # Set the isolated home before sshd starts the account shell. A shell can
    # read its startup files even when it only executes a remote command.
    home = environment.get("HOME", str(fixture))
    remote_env = {"HOME": home, "ZDOTDIR": environment.get("ZDOTDIR", home)}
    for name in (
        "LANG",
        "PATH",
        "TMPDIR",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        "XDG_STATE_HOME",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
    ):
        if name in environment:
            remote_env[name] = environment[name]
    # OpenSSH config arguments use double quotes for paths containing spaces.
    settings = " ".join(
        json.dumps(f"{name}={value}", ensure_ascii=False) for name, value in remote_env.items()
    )

    def config_path(path: Path, *, expands_tokens: bool = False) -> str:
        value = str(path).replace("%", "%%") if expands_tokens else str(path)
        return json.dumps(value, ensure_ascii=False)

    config = fixture / "sshd_config"
    config.write_text(
        f"HostKey {config_path(fixture / 'host')}\n"
        f"AuthorizedKeysFile {config_path(fixture / 'user.pub', expands_tokens=True)}\n"
        f"AllowUsers {user}\n"
        # The generated keys are in a mode-0700 directory below the shared /tmp.
        "StrictModes no\n"
        "UsePAM no\n"
        "AuthenticationMethods publickey\n"
        "PermitUserRC no\n"
        "DisableForwarding yes\n"
        f"SetEnv {settings}\n"
    )
    # ProxyCommand also expands percent tokens after shell quoting.
    proxy = shlex.join(["/usr/sbin/sshd", "-i", "-e", "-f", str(config)])
    proxy += " 2>" + shlex.quote(str(fixture / "sshd.log"))
    return [
        "/usr/bin/ssh",
        "-F",
        "/dev/null",
        "-tt",
        "-o",
        "BatchMode=yes",
        "-o",
        "IdentityAgent=none",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "GlobalKnownHostsFile=/dev/null",
        "-o",
        "UserKnownHostsFile=" + config_path(known_hosts, expands_tokens=True),
        "-o",
        "UpdateHostKeys=no",
        "-o",
        "ProxyCommand=" + proxy.replace("%", "%%"),
        "-o",
        "IdentityFile=" + config_path(fixture / "user", expands_tokens=True),
        f"{user}@{host}",
        'cd "$HOME" && exec ' + shlex.join(command),
    ]
