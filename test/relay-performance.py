#!/usr/bin/env python3
"""Measure relay subprocess startup against the PR's parent revision."""

from __future__ import annotations

import argparse
import math
import os
import statistics
import subprocess
import tempfile
import time
from pathlib import Path


def percentile(values: list[float], fraction: float) -> float:
    """Return a nearest-rank percentile from a non-empty sample."""
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def invoke(command: list[str], environment: dict[str, str], expected: int) -> float:
    """Run one timed subprocess and reject faster-but-broken results."""
    started = time.perf_counter_ns()
    result = subprocess.run(
        command,
        env=environment,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        check=False,
    )
    elapsed = (time.perf_counter_ns() - started) / 1_000_000
    if result.returncode != expected:
        raise RuntimeError(
            f"{' '.join(command)} exited {result.returncode}, expected {expected}: "
            f"{result.stderr.decode(errors='replace')}"
        )
    return elapsed


def measure(
    baseline: Path,
    candidate: Path,
    arguments: list[str],
    environment: dict[str, str],
    expected: int,
    samples: int,
) -> tuple[list[float], list[float]]:
    """Alternate baseline/candidate order to limit shared-runner drift."""
    baseline_values: list[float] = []
    candidate_values: list[float] = []
    for _ in range(5):
        invoke([str(baseline), *arguments], environment, expected)
        invoke([str(candidate), *arguments], environment, expected)
    for index in range(samples):
        order = ((baseline, baseline_values), (candidate, candidate_values))
        if index % 2:
            order = tuple(reversed(order))
        for executable, values in order:
            values.append(invoke([str(executable), *arguments], environment, expected))
    return baseline_values, candidate_values


def compare_cli(
    baseline: Path,
    candidate: Path,
    arguments: list[str],
    environment: dict[str, str],
) -> None:
    """Require the optimized executable to retain argparse-visible behavior."""
    results = []
    for executable in (baseline, candidate):
        results.append(
            subprocess.run(
                [str(executable), *arguments],
                env=environment,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                check=False,
            )
        )
    before, after = results
    if (before.returncode, before.stdout, before.stderr) != (
        after.returncode,
        after.stdout,
        after.stderr,
    ):
        raise RuntimeError(f"CLI behavior changed for arguments: {arguments!r}")


def main() -> int:
    """Benchmark calibrated hot paths and enforce noise-tolerant budgets."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-ref", required=True)
    parser.add_argument("--samples", type=int, default=50)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    candidate = root / "bin/termnav-relay"

    with tempfile.TemporaryDirectory(prefix="termnav-performance-") as raw_tmp:
        tmp = Path(raw_tmp)
        baseline = tmp / "termnav-relay-baseline"
        content = subprocess.run(
            ["git", "-C", str(root), "show", f"{args.baseline_ref}:bin/termnav-relay"],
            stdout=subprocess.PIPE,
            check=True,
        ).stdout
        baseline.write_bytes(content)
        baseline.chmod(0o700)

        base_env = os.environ.copy()
        base_env.pop("TERMNAV_PARENT_RELAY", None)
        base_env.pop("TERMNAV_RELAY_LOG", None)
        base_env.pop("TERMNAV_RELAY_TEST_LOG", None)
        cli_cases = [
            ["--help"],
            ["send", "--help"],
            ["send", "invalid", "left"],
            ["send", "pane", "diagonal"],
            [
                "send",
                "--client-pid",
                "1",
                "--client-tty",
                "/dev/null",
                "pane",
                "left",
            ],
            [
                "send",
                "pane",
                "left",
                "--client-tty",
                "/dev/null",
                "--client-pid",
                "1",
            ],
        ]
        for arguments in cli_cases:
            compare_cli(baseline, candidate, arguments, base_env)
        print(f"CLI parity: {len(cli_cases)} argparse and fallback forms matched")

        scenarios = [
            ("send decline", ["send", "pane", "left"], base_env, 3),
            (
                "send dead socket",
                ["send", "pane", "left"],
                {**base_env, "TERMNAV_PARENT_RELAY": str(tmp / "missing.sock")},
                1,
            ),
            (
                "stray commit",
                [
                    "commit",
                    "--tmux-socket",
                    str(tmp / "tmux.sock"),
                    "--client-tty",
                    "/dev/null",
                    "--client-pid",
                    "1",
                ],
                {
                    **base_env,
                    "XDG_RUNTIME_DIR": str(tmp / "runtime"),
                    "TERMNAV_RELAY_TEST_SKIP_TTY_VALIDATION": "1",
                },
                0,
            ),
        ]

        for name, arguments, environment, expected in scenarios:
            baseline_values, candidate_values = measure(
                baseline,
                candidate,
                arguments,
                environment,
                expected,
                args.samples,
            )
            baseline_median = statistics.median(baseline_values)
            candidate_median = statistics.median(candidate_values)
            baseline_p95 = percentile(baseline_values, 0.95)
            candidate_p95 = percentile(candidate_values, 0.95)
            delta = baseline_median - candidate_median
            percent = delta / baseline_median * 100
            print(
                f"{name}: baseline median={baseline_median:.1f}ms "
                f"p95={baseline_p95:.1f}ms; candidate median={candidate_median:.1f}ms "
                f"p95={candidate_p95:.1f}ms; improvement={delta:.1f}ms ({percent:.1f}%)"
            )
            if candidate_median > baseline_median * 1.10 + 3:
                raise RuntimeError(f"{name}: median regression exceeds budget")
            if candidate_p95 > baseline_p95 * 1.20 + 5:
                raise RuntimeError(f"{name}: p95 regression exceeds budget")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
