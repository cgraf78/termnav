#!/usr/bin/env python3
"""Exercise the SSH planner with an explicit stdin-tty observation."""

from __future__ import annotations

import importlib.util
import os
import pathlib
import sys
from importlib.machinery import SourceFileLoader

ROOT = pathlib.Path(__file__).resolve().parents[2]
RELAY = ROOT / "bin" / "termnav-relay"
SPEC = importlib.util.spec_from_loader(
    "termnav_relay_ssh_test",
    SourceFileLoader("termnav_relay_ssh_test", str(RELAY)),
)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {RELAY}")
relay = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = relay
SPEC.loader.exec_module(relay)

raise SystemExit(
    relay.ssh_command(
        sys.argv[1:],
        stdin_tty=os.environ.get("TERMNAV_TEST_STDIN_TTY") == "1",
    )
)
