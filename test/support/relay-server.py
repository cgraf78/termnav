"""Test-owned relay server with a deterministic navigation handler."""

from __future__ import annotations

import argparse
import importlib.util
import pathlib
import sys
import time
from importlib.machinery import SourceFileLoader

ROOT = pathlib.Path(__file__).resolve().parents[2]
RELAY = ROOT / "lib" / "termnav" / "relay.py"
# `relay.py` deliberately lazy-loads its heavier routing modules after startup.
# Put the production library first now so those later imports cannot resolve to
# this directory's same-named test adapters and form a circular import.
sys.path.insert(0, str(RELAY.parent))
spec = importlib.util.spec_from_loader(
    "termnav_relay_server_test",
    SourceFileLoader("termnav_relay_server_test", str(RELAY)),
)
if spec is None or spec.loader is None:
    raise RuntimeError(f"cannot import {RELAY}")
relay = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = relay
spec.loader.exec_module(relay)


def main() -> int:
    """Serve the real socket protocol around an intentionally fake router."""

    parser = argparse.ArgumentParser()
    parser.add_argument("--socket", required=True)
    parser.add_argument("--log", required=True)
    parser.add_argument("--delay", type=float, default=0)
    parser.add_argument("--result", choices=("armed", "declined", "error"), default="armed")
    arguments = parser.parse_args()
    log = pathlib.Path(arguments.log)

    def navigate(request: dict) -> dict:
        with log.open("a", encoding="utf-8") as output:
            output.write(f"{request.get('scope')} {request.get('direction')}\n")
        if arguments.delay > 0:
            time.sleep(arguments.delay)
            with log.open("a", encoding="utf-8") as output:
                output.write(f"completed {request.get('scope')} {request.get('direction')}\n")
        return {"v": 2, "result": arguments.result}

    relay.handle_navigate = navigate
    return relay.serve(arguments.socket)


if __name__ == "__main__":
    raise SystemExit(main())
