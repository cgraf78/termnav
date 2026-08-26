#!/usr/bin/env python3
"""Measure relay subprocess startup against the PR's parent revision."""

from __future__ import annotations

import argparse
import io
import math
import os
import statistics
import subprocess
import sys
import tarfile
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
    baseline_arguments: list[str],
    candidate_arguments: list[str],
    environment: dict[str, str],
    expected: int,
    samples: int,
) -> tuple[list[float], list[float]]:
    """Alternate equivalent old/new invocations to limit runner drift.

    Performance comparison spans the PR boundary, so a deliberately changed
    CLI may need different arguments on each side. Keeping that translation in
    the benchmark avoids carrying obsolete syntax in the production parser.
    """

    baseline_values: list[float] = []
    candidate_values: list[float] = []
    for _ in range(5):
        invoke([str(baseline), *baseline_arguments], environment, expected)
        invoke([str(candidate), *candidate_arguments], environment, expected)
    for index in range(samples):
        order = (
            (baseline, baseline_arguments, baseline_values),
            (candidate, candidate_arguments, candidate_values),
        )
        if index % 2:
            order = tuple(reversed(order))
        for executable, arguments, values in order:
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


def tmux_navigation_samples(navigator: Path, root: Path, samples: int) -> list[float]:
    """Measure the Python boundary path against a real attached tmux client."""

    name = f"termnav-performance-{os.getpid()}"
    subprocess.run(
        ["tmux", "-L", name, "-f", "/dev/null", "new-session", "-d", "-s", "perf"],
        check=True,
        capture_output=True,
    )
    client = None
    try:
        socket = subprocess.run(
            ["tmux", "-L", name, "display-message", "-p", "#{socket_path}"],
            text=True,
            capture_output=True,
            check=True,
        ).stdout.strip()
        left = subprocess.run(
            ["tmux", "-L", name, "display-message", "-p", "#{pane_id}"],
            text=True,
            capture_output=True,
            check=True,
        ).stdout.strip()
        subprocess.run(
            ["tmux", "-L", name, "split-window", "-h", "-d", "-t", "perf"],
            check=True,
            capture_output=True,
        )
        client_environment = os.environ.copy()
        client_environment["TERM"] = "xterm-256color"
        client = subprocess.Popen(
            [
                sys.executable,
                str(root / "test" / "support" / "tmux-client.py"),
                "-L",
                name,
                "attach-session",
                "-t",
                "perf",
            ],
            env=client_environment,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            attached = subprocess.run(
                ["tmux", "-L", name, "list-clients"],
                capture_output=True,
                check=False,
            )
            if attached.returncode == 0 and attached.stdout:
                break
            time.sleep(0.01)
        else:
            raise RuntimeError("tmux-backed navigation client did not attach")

        environment = os.environ.copy()
        environment.update({"TMUX": f"{socket},0,0", "TMUX_PANE": left})
        values = []
        for _ in range(samples):
            subprocess.run(
                ["tmux", "-L", name, "select-pane", "-t", left],
                check=True,
                capture_output=True,
            )
            values.append(
                invoke(
                    [str(navigator), "pane-select", "right"],
                    environment,
                    0,
                )
            )
        return values
    finally:
        if client is not None:
            client.terminate()
            try:
                client.wait(timeout=2)
            except subprocess.TimeoutExpired:
                client.kill()
                client.wait()
        subprocess.run(
            ["tmux", "-L", name, "kill-server"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )


def main() -> int:
    """Benchmark calibrated hot paths and enforce noise-tolerant budgets."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-ref", required=True)
    parser.add_argument("--samples", type=int, default=50)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    candidate = root / "bin/termnav-relay"
    navigator = root / "bin/termnav-navigate"

    with tempfile.TemporaryDirectory(prefix="termnav-performance-") as raw_tmp:
        tmp = Path(raw_tmp)
        baseline_root = tmp / "baseline"
        archive = subprocess.run(
            [
                "git",
                "-C",
                str(root),
                "archive",
                "--format=tar",
                args.baseline_ref,
                "bin/termnav-relay",
                "lib/termnav",
            ],
            stdout=subprocess.PIPE,
            check=True,
        ).stdout
        # Materialize the executable with the library tree from the same
        # revision. Benchmark setup must evolve with the implementation rather
        # than forcing the production CLI to retain obsolete self-contained
        # layouts solely for future comparisons.
        with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as source:
            for member in source:
                if not member.isfile():
                    continue
                target = baseline_root / member.name
                target.parent.mkdir(parents=True, exist_ok=True)
                payload = source.extractfile(member)
                if payload is None:
                    raise RuntimeError(f"cannot read baseline member: {member.name}")
                target.write_bytes(payload.read())
        baseline = baseline_root / "bin" / "termnav-relay"
        baseline.chmod(0o700)

        base_env = os.environ.copy()
        base_env.pop("TERMNAV_PARENT_RELAY", None)
        base_env.pop("TERMNAV_RELAY_LOG", None)
        cli_cases = [
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

        send_decline = ["send", "pane", "left"]
        send_dead_socket = ["send", "pane", "left"]
        old_stray_commit = [
            "commit",
            "--tmux-socket",
            str(tmp / "tmux.sock"),
            "--client-tty",
            "/dev/null",
            "--client-pid",
            "1",
        ]
        new_stray_commit = [*old_stray_commit, "--client-created", "1"]
        baseline_stray_commit = (
            new_stray_commit
            if "--client-created" in baseline.read_text(encoding="utf-8")
            else old_stray_commit
        )
        scenarios = [
            ("send decline", send_decline, send_decline, base_env, 3),
            (
                "send dead socket",
                send_dead_socket,
                send_dead_socket,
                {**base_env, "TERMNAV_PARENT_RELAY": str(tmp / "missing.sock")},
                1,
            ),
            (
                "stray commit",
                baseline_stray_commit,
                new_stray_commit,
                {
                    **base_env,
                    "XDG_RUNTIME_DIR": str(tmp / "runtime"),
                },
                0,
            ),
        ]

        for name, baseline_arguments, candidate_arguments, environment, expected in scenarios:
            baseline_values, candidate_values = measure(
                baseline,
                candidate,
                baseline_arguments,
                candidate_arguments,
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

        navigation_env = base_env.copy()
        navigation_env.pop("TMUX", None)
        navigation_env.pop("TMUX_PANE", None)
        startup = invoke(
            [str(navigator), "pane-select", "left"],
            navigation_env,
            0,
        )
        launcher_values = []
        navigation_values = []
        for _ in range(args.samples):
            launcher_values.append(
                invoke(["/usr/bin/env", "python3", "-c", "pass"], navigation_env, 0)
            )
            navigation_values.append(
                invoke(
                    [str(navigator), "pane-select", "left"],
                    navigation_env,
                    0,
                )
            )
        launcher_median = statistics.median(launcher_values)
        navigation_median = statistics.median(navigation_values)
        launcher_p95 = percentile(launcher_values, 0.95)
        navigation_p95 = percentile(navigation_values, 0.95)
        median_overhead = navigation_median - launcher_median
        p95_overhead = navigation_p95 - launcher_p95
        print(
            f"navigation boundary: cold={startup:.1f}ms; "
            f"launcher median={launcher_median:.1f}ms p95={launcher_p95:.1f}ms; "
            f"router median={navigation_median:.1f}ms p95={navigation_p95:.1f}ms; "
            f"overhead median={median_overhead:.1f}ms p95={p95_overhead:.1f}ms"
        )
        if startup > 250:
            raise RuntimeError("navigation boundary: cold startup exceeds 250ms")
        if median_overhead > 75:
            raise RuntimeError("navigation boundary: median overhead exceeds 75ms")
        if p95_overhead > 110:
            raise RuntimeError("navigation boundary: p95 overhead exceeds 110ms")

        tmux_values = tmux_navigation_samples(navigator, root, args.samples)
        tmux_median = statistics.median(tmux_values)
        tmux_p95 = percentile(tmux_values, 0.95)
        print(f"navigation tmux route: median={tmux_median:.1f}ms p95={tmux_p95:.1f}ms")
        if tmux_median > 250:
            raise RuntimeError("navigation tmux route: median exceeds 250ms")
        if tmux_p95 > 500:
            raise RuntimeError("navigation tmux route: p95 exceeds 500ms")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
