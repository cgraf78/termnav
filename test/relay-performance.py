#!/usr/bin/env python3
"""Measure navigation hot paths against the PR's parent revision."""

from __future__ import annotations

import argparse
import contextlib
import io
import math
import os
import shlex
import shutil
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


def stop_service(executable: Path, environment: dict[str, str]) -> None:
    """Stop only the benchmark-owned hot service before its runtime is removed."""

    subprocess.run(
        [str(executable), "stop"],
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def measure(
    baseline: Path,
    candidate: Path,
    baseline_arguments: list[str],
    candidate_arguments: list[str],
    baseline_environment: dict[str, str],
    candidate_environment: dict[str, str],
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
        invoke([str(baseline), *baseline_arguments], baseline_environment, expected)
        invoke([str(candidate), *candidate_arguments], candidate_environment, expected)
    for index in range(samples):
        order = (
            (baseline, baseline_arguments, baseline_environment, baseline_values),
            (candidate, candidate_arguments, candidate_environment, candidate_values),
        )
        if index % 2:
            order = tuple(reversed(order))
        for executable, arguments, environment, values in order:
            values.append(invoke([str(executable), *arguments], environment, expected))
    return baseline_values, candidate_values


def compare_cli(
    baseline: Path,
    candidate: Path,
    arguments: list[str],
    baseline_environment: dict[str, str],
    candidate_environment: dict[str, str],
) -> None:
    """Require the optimized executable to retain argparse-visible behavior."""
    results = []
    for executable, environment in (
        (baseline, baseline_environment),
        (candidate, candidate_environment),
    ):
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
        client_environment.pop("TMUX", None)
        client_environment.pop("TMUX_PANE", None)
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


def stream_samples(navigator: Path, environment: dict[str, str], samples: int) -> list[float]:
    """Measure Neovim's resident ordered-router protocol after startup."""

    worker = subprocess.Popen(
        [str(navigator), "--stream"],
        env=environment,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if worker.stdin is None or worker.stdout is None:
        raise RuntimeError("navigation stream did not expose pipes")
    try:
        values = []
        for index in range(samples + 5):
            started = time.perf_counter_ns()
            worker.stdin.write("pane-select left\n")
            worker.stdin.flush()
            result = worker.stdout.readline().strip()
            elapsed = (time.perf_counter_ns() - started) / 1_000_000
            if result != "0":
                raise RuntimeError(f"navigation stream returned {result!r}, expected '0'")
            if index >= 5:
                values.append(elapsed)
        return values
    finally:
        worker.stdin.close()
        try:
            worker.wait(timeout=2)
        except subprocess.TimeoutExpired:
            worker.kill()
            worker.wait()


def tmux_boundary_samples(
    baseline: Path,
    candidate: Path,
    root: Path,
    environment: dict[str, str],
    samples: int,
) -> tuple[list[float], list[float]]:
    """Compare the exact tmux edge form before and after daemon dispatch."""

    with tempfile.TemporaryDirectory(prefix="termnav-boundary-") as raw_tmp:
        tmp = Path(raw_tmp)
        inner_socket = str(tmp / "inner.sock")
        outer_socket = str(tmp / "outer.sock")
        subprocess.run(
            [
                "tmux",
                "-S",
                inner_socket,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "inner",
                "cat",
            ],
            check=True,
            capture_output=True,
        )
        nested = f"exec tmux -S {shlex.quote(inner_socket)} attach-session -t inner"
        subprocess.run(
            [
                "tmux",
                "-S",
                outer_socket,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "outer",
                nested,
            ],
            check=True,
            capture_output=True,
        )
        left = subprocess.run(
            [
                "tmux",
                "-S",
                outer_socket,
                "display-message",
                "-p",
                "-t",
                "outer",
                "#{pane_id}",
            ],
            text=True,
            capture_output=True,
            check=True,
        ).stdout.strip()
        subprocess.run(
            ["tmux", "-S", outer_socket, "split-window", "-h", "-d", "-t", "outer", "cat"],
            check=True,
            capture_output=True,
        )
        pane = subprocess.run(
            [
                "tmux",
                "-S",
                inner_socket,
                "display-message",
                "-p",
                "-t",
                "inner",
                "#{pane_id}",
            ],
            text=True,
            capture_output=True,
            check=True,
        ).stdout.strip()
        client = None
        try:
            client_environment = os.environ.copy()
            client_environment["TERM"] = "xterm-256color"
            client = subprocess.Popen(
                [
                    sys.executable,
                    str(root / "test" / "support" / "tmux-client.py"),
                    "-S",
                    outer_socket,
                    "attach-session",
                    "-t",
                    "outer",
                ],
                env=client_environment,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            deadline = time.monotonic() + 5
            identity = ""
            while time.monotonic() < deadline:
                identity = subprocess.run(
                    [
                        "tmux",
                        "-S",
                        inner_socket,
                        "list-clients",
                        "-F",
                        "#{client_pid}|#{client_tty}|#{client_created}|"
                        "#{client_termtype}|#{session_id}",
                    ],
                    text=True,
                    capture_output=True,
                    check=False,
                ).stdout.strip()
                if identity:
                    break
                time.sleep(0.01)
            else:
                raise RuntimeError("nested tmux boundary client did not attach")
            pid, tty, created, termtype, session = identity.split("|", 4)
            arguments = [
                "pane-select",
                "right",
                "--parent",
                "--client-pid",
                pid,
                "--client-tty",
                tty,
                "--client-created",
                created,
                "--client-termtype",
                termtype,
                "--source-socket",
                inner_socket,
                "--source-pane",
                pane,
                "--source-session",
                session,
            ]
            before: list[float] = []
            after: list[float] = []
            for _ in range(5):
                for executable, command in (
                    (baseline, arguments),
                    (candidate, ["navigate", *arguments]),
                ):
                    subprocess.run(
                        ["tmux", "-S", outer_socket, "select-pane", "-t", left],
                        check=True,
                        capture_output=True,
                    )
                    invoke([str(executable), *command], environment, 0)
            for index in range(samples):
                order = (
                    (baseline, arguments, before),
                    (candidate, ["navigate", *arguments], after),
                )
                if index % 2:
                    order = tuple(reversed(order))
                for executable, command, values in order:
                    subprocess.run(
                        ["tmux", "-S", outer_socket, "select-pane", "-t", left],
                        check=True,
                        capture_output=True,
                    )
                    values.append(invoke([str(executable), *command], environment, 0))
            return before, after
        finally:
            if client is not None:
                client.terminate()
                try:
                    client.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    client.kill()
                    client.wait()
            subprocess.run(
                ["tmux", "-S", outer_socket, "kill-server"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
            subprocess.run(
                ["tmux", "-S", inner_socket, "kill-server"],
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
    # Synthetic clients must not discover the developer's enclosing tmux by
    # walking back through this benchmark process. Real terminal clients have
    # no such inherited parent scope.
    os.environ.pop("TMUX", None)
    os.environ.pop("TMUX_PANE", None)
    root = Path(__file__).resolve().parent.parent
    candidate = root / "bin/termnav-relay"
    navigator = root / "bin/termnav-navigate"

    with contextlib.ExitStack() as stack:
        raw_tmp = stack.enter_context(tempfile.TemporaryDirectory(prefix="termnav-performance-"))
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
                "share/termnav/shell.sh",
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
        baseline_is_hot = "hot-serve" in baseline.read_text(encoding="utf-8")

        common_env = os.environ.copy()
        common_env.pop("TERMNAV_PARENT_RELAY", None)
        common_env.pop("TERMNAV_RELAY_LOG", None)
        baseline_env = {**common_env, "XDG_RUNTIME_DIR": str(tmp / "runtime-baseline")}
        candidate_env = {**common_env, "XDG_RUNTIME_DIR": str(tmp / "runtime-candidate")}
        if baseline_is_hot:
            subprocess.run(
                [str(baseline), "warm"],
                env=baseline_env,
                capture_output=True,
                check=True,
            )
            stack.callback(stop_service, baseline, baseline_env)
        subprocess.run(
            [str(candidate), "warm"],
            env=candidate_env,
            capture_output=True,
            check=True,
        )
        stack.callback(stop_service, candidate, candidate_env)
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
            compare_cli(baseline, candidate, arguments, baseline_env, candidate_env)
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
            ("send decline", send_decline, send_decline, {}, 3),
            (
                "send dead socket",
                send_dead_socket,
                send_dead_socket,
                {"TERMNAV_PARENT_RELAY": str(tmp / "missing.sock")},
                1,
            ),
            (
                "stray commit",
                baseline_stray_commit,
                new_stray_commit,
                {},
                0,
            ),
        ]

        for name, baseline_arguments, candidate_arguments, updates, expected in scenarios:
            baseline_values, candidate_values = measure(
                baseline,
                candidate,
                baseline_arguments,
                candidate_arguments,
                {**baseline_env, **updates},
                {**candidate_env, **updates},
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
            if candidate_median > 35:
                raise RuntimeError(f"{name}: hot median exceeds 35ms")
            if candidate_p95 > 60:
                raise RuntimeError(f"{name}: hot p95 exceeds 60ms")
            if not baseline_is_hot and candidate_median > baseline_median * 0.75:
                raise RuntimeError(f"{name}: startup removal improves median by less than 25%")

        bash = shutil.which("bash")
        if bash is None:
            raise RuntimeError("bash is required for the shell activation benchmark")
        shell_environment = {
            key: value for key, value in common_env.items() if key not in {"REPO_TEST", "DOT_TEST"}
        }
        shell_before, shell_after = measure(
            Path(bash),
            Path(bash),
            [
                "--noprofile",
                "--norc",
                "-c",
                '. "$1"',
                "termnav-shell",
                str(baseline_root / "share" / "termnav" / "shell.sh"),
            ],
            [
                "--noprofile",
                "--norc",
                "-c",
                '. "$1"',
                "termnav-shell",
                str(root / "share" / "termnav" / "shell.sh"),
            ],
            {**shell_environment, "XDG_RUNTIME_DIR": baseline_env["XDG_RUNTIME_DIR"]},
            {**shell_environment, "XDG_RUNTIME_DIR": candidate_env["XDG_RUNTIME_DIR"]},
            0,
            args.samples,
        )
        shell_before_median = statistics.median(shell_before)
        shell_after_median = statistics.median(shell_after)
        shell_before_p95 = percentile(shell_before, 0.95)
        shell_after_p95 = percentile(shell_after, 0.95)
        print(
            f"shell activation: baseline median={shell_before_median:.1f}ms "
            f"p95={shell_before_p95:.1f}ms; candidate median={shell_after_median:.1f}ms "
            f"p95={shell_after_p95:.1f}ms"
        )
        if shell_after_median > 50:
            raise RuntimeError("shell activation: median exceeds 50ms")
        if shell_after_p95 > 100:
            raise RuntimeError("shell activation: p95 exceeds 100ms")
        if not baseline_is_hot and shell_after_median > shell_before_median * 0.75:
            raise RuntimeError("shell activation: median improvement is below 25%")

        navigation_env = candidate_env.copy()
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

        resident_values = stream_samples(navigator, navigation_env, args.samples)
        resident_median = statistics.median(resident_values)
        resident_p95 = percentile(resident_values, 0.95)
        print(
            f"navigation resident stream: median={resident_median:.2f}ms p95={resident_p95:.2f}ms"
        )
        if resident_median > 10:
            raise RuntimeError("navigation resident stream: median exceeds 10ms")
        if resident_p95 > 30:
            raise RuntimeError("navigation resident stream: p95 exceeds 30ms")

        tmux_values = tmux_navigation_samples(navigator, root, args.samples)
        tmux_median = statistics.median(tmux_values)
        tmux_p95 = percentile(tmux_values, 0.95)
        print(f"navigation tmux route: median={tmux_median:.1f}ms p95={tmux_p95:.1f}ms")
        if tmux_median > 250:
            raise RuntimeError("navigation tmux route: median exceeds 250ms")
        if tmux_p95 > 500:
            raise RuntimeError("navigation tmux route: p95 exceeds 500ms")

        boundary_before, boundary_after = tmux_boundary_samples(
            navigator,
            candidate,
            root,
            candidate_env,
            args.samples,
        )
        boundary_before_median = statistics.median(boundary_before)
        boundary_after_median = statistics.median(boundary_after)
        boundary_before_p95 = percentile(boundary_before, 0.95)
        boundary_after_p95 = percentile(boundary_after, 0.95)
        print(
            f"tmux boundary hot service: baseline median={boundary_before_median:.1f}ms "
            f"p95={boundary_before_p95:.1f}ms; candidate median={boundary_after_median:.1f}ms "
            f"p95={boundary_after_p95:.1f}ms"
        )
        if boundary_after_median > 40:
            raise RuntimeError("tmux boundary hot service: median exceeds 40ms")
        if boundary_after_p95 > 70:
            raise RuntimeError("tmux boundary hot service: p95 exceeds 70ms")
        if boundary_after_median > boundary_before_median * 0.5:
            raise RuntimeError("tmux boundary hot service: median improvement is below 50%")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
