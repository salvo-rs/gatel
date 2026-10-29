#!/usr/bin/env python3
"""Compare gatel with nginx and Caddy under equivalent HTTP workloads.

Every server runs in its own container, configured to do the same work, and is
driven by `wrk` from a third container so that the load generator competes with
all of them identically. The result is a report in
`target/benchmarks/http-servers/<timestamp>/`.

What this is and is not: it is a repeatable comparison on one machine, which is
the only kind of number worth writing down. It is not a claim about anyone
else's machine, and a container on a laptop is not a server under production
traffic. Run it before believing anything about performance, including
anything this project says.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import random
import re
import statistics
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from datetime import datetime
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
OUTPUT_ROOT = REPO_ROOT / "target" / "benchmarks" / "http-servers"
WWW = HERE / "fixtures" / "www"

TARGETS = ("gatel", "nginx", "caddy")
WRK_TIMEOUT_SECONDS = 5

# Generated rather than committed: eleven megabytes of incompressible noise is
# not source, and a repository that carries it pays for it on every clone.
PAYLOADS = {
    "payload-4k.txt": 4 * 1024,
    "payload-64k.bin": 64 * 1024,
    "payload-65k.bin": 65 * 1024,
    "payload-1m.bin": 1024 * 1024,
    "payload-10m.bin": 10 * 1024 * 1024,
}

SCENARIOS = {
    "static-4k": {
        "description": "Static file serving — 4 KiB text file.",
        "service": "{target}-static",
        "path": "/payload-4k.txt",
    },
    "static-1m": {
        "description": "Static file serving — 1 MiB binary (streamed body).",
        "service": "{target}-static",
        "path": "/payload-1m.bin",
    },
    "static-10m": {
        "description": "Static file serving — 10 MiB binary (sustained throughput).",
        "service": "{target}-static",
        "path": "/payload-10m.bin",
    },
    "range-10m": {
        "description": "Range request — first 64 KiB of a 10 MiB file (no seek).",
        "service": "{target}-static",
        "path": "/payload-10m.bin",
        "wrk_script": "/workspace/range-64k.lua",
        "range": (0, 65535),
    },
    "proxy": {
        "description": "Reverse proxying — 4 KiB response from a shared origin.",
        "service": "{target}-proxy",
        "path": "/payload-4k.txt",
    },
}

# Opt-in controls preserve the five historical default scenarios.
DEFAULT_SCENARIOS = tuple(SCENARIOS)
SCENARIOS.update({
    "static-64k": {
        "description": "Complete 64 KiB file — same response length as range-10m.",
        "service": "{target}-static",
        "path": "/payload-64k.bin",
    },
    "static-65k": {
        "description": "Complete 65 KiB file — just above Gatel's inline threshold.",
        "service": "{target}-static",
        "path": "/payload-65k.bin",
    },
    "range-offset-10m": {
        "description": "64 KiB range starting at 1 MiB — includes positioning.",
        "service": "{target}-static",
        "path": "/payload-10m.bin",
        "wrk_script": "/workspace/range-offset-64k.lua",
        "range": (1048576, 1114111),
    },
    "range-65k": {
        "description": "65 KiB range — just above Gatel's inline threshold.",
        "service": "{target}-static",
        "path": "/payload-10m.bin",
        "wrk_script": "/workspace/range-65k.lua",
        "range": (0, 66559),
    },
})


def url_for(scenario: dict, target: str) -> str:
    return "http://{}:8080{}".format(scenario["service"].format(target=target), scenario["path"])


@dataclass
class BenchResult:
    scenario: str
    target: str
    round_id: int
    requests_per_sec: float
    transfer_bytes_per_sec: float
    latency_avg_ms: float
    latency_stdev_ms: float
    latency_max_ms: float
    p50_ms: float
    p75_ms: float
    p90_ms: float
    p99_ms: float
    total_requests: int
    duration_seconds: float
    total_read_bytes: float
    non_2xx_3xx: int
    socket_errors: int
    timeout_errors: int


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Compare gatel with nginx and Caddy")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--duration", type=int, default=10, help="seconds per round")
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--connections", type=int, default=128)
    parser.add_argument("--warmup", type=int, default=3, help="seconds per target")
    parser.add_argument("--target", action="append", choices=TARGETS, help="repeatable; defaults to all three")
    parser.add_argument("--no-build", action="store_true", help="reuse explicitly prebuilt images for A/B runs")
    parser.add_argument(
        "--scenario",
        action="append",
        choices=sorted(SCENARIOS),
        help="run only this scenario; repeatable",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=None,
        help="write results here instead of target/benchmarks/http-servers/<timestamp>",
    )
    return parser.parse_args()


# --------------------------------------------------------------------- setup


def ensure_payloads() -> None:
    """Create the files under test, once, deterministically.

    The bytes come from a fixed seed so that two runs on two machines are
    reading the same thing — and so that the 10 MiB file is genuinely
    incompressible, which keeps a server that quietly compresses it from
    looking fast.
    """
    WWW.mkdir(parents=True, exist_ok=True)
    for name, size in PAYLOADS.items():
        path = WWW / name
        if path.exists() and path.stat().st_size == size:
            continue
        print(f"generating {path.relative_to(HERE)} ({size:,} bytes)", flush=True)
        if name.endswith(".txt"):
            line = ("the quick brown fox jumps over the lazy dog " * 2).encode()
            data = (line * (size // len(line) + 1))[:size]
        else:
            data = random.Random(20240101).randbytes(size)
        path.write_bytes(data)


def run_command(args: list[str], *, check: bool = True, capture_output: bool = False):
    print("$", " ".join(args), flush=True)
    return subprocess.run(args, cwd=HERE, check=check, text=True, capture_output=capture_output)


def compose(*args: str, check: bool = True, capture_output: bool = False):
    return run_command(["docker", "compose", *args], check=check, capture_output=capture_output)


# ------------------------------------------------------------------- parsing


TIME_UNIT = r"us|ms|s|m|h"
TIME_VALUE = rf"[0-9.]+(?:{TIME_UNIT})"


def to_milliseconds(value: str) -> float:
    match = re.fullmatch(rf"([0-9.]+)({TIME_UNIT})", value.strip())
    if not match:
        raise ValueError(f"unrecognized latency value: {value!r}")
    amount = float(match.group(1))
    if match.group(2) == "us":
        return amount / 1000.0
    return amount * {"ms": 1, "s": 1000, "m": 60000, "h": 3600000}[match.group(2)]


def to_bytes(value: str) -> float:
    match = re.fullmatch(r"([0-9.]+)(B|KB|MB|GB|TB)", value.strip())
    if not match:
        raise ValueError(f"unrecognized byte value: {value!r}")
    scale = {"B": 1, "KB": 1024, "MB": 1024**2, "GB": 1024**3, "TB": 1024**4}
    return float(match.group(1)) * scale[match.group(2)]


def parse_wrk_output(scenario: str, target: str, round_id: int, text: str) -> BenchResult:
    where = f"{scenario}/{target}/round-{round_id}"

    def need(pattern: str, what: str):
        match = re.search(pattern, text)
        if not match:
            raise RuntimeError(f"failed to parse {what} for {where}\n{text}")
        return match

    latency = need(
        rf"Latency\s+({TIME_VALUE})\s+({TIME_VALUE})\s+({TIME_VALUE})",
        "the latency line",
    )
    requests = need(
        rf"(\d+)\s+requests in\s+({TIME_VALUE}),\s+([0-9.]+(?:B|KB|MB|GB|TB))\s+read",
        "the request summary",
    )
    per_second = need(r"Requests/sec:\s+([0-9.]+)", "throughput")
    transfer = need(r"Transfer/sec:\s+([0-9.]+(?:B|KB|MB|GB|TB))", "transfer rate")

    def percentile(name: str) -> float:
        return to_milliseconds(need(rf"{name}%\s+({TIME_VALUE})", f"the {name}th percentile").group(1))

    non_2xx = re.search(r"Non-2xx or 3xx responses:\s+(\d+)", text)
    sockets = re.search(
        r"Socket errors:\s+connect\s+(\d+),\s+read\s+(\d+),\s+write\s+(\d+),\s+timeout\s+(\d+)",
        text,
    )

    if "Socket errors:" in text and sockets is None:
        raise RuntimeError(f"failed to parse socket errors for {where}\n{text}")

    return BenchResult(
        scenario=scenario,
        target=target,
        round_id=round_id,
        requests_per_sec=float(per_second.group(1)),
        transfer_bytes_per_sec=to_bytes(transfer.group(1)),
        latency_avg_ms=to_milliseconds(latency.group(1)),
        latency_stdev_ms=to_milliseconds(latency.group(2)),
        latency_max_ms=to_milliseconds(latency.group(3)),
        p50_ms=percentile("50"),
        p75_ms=percentile("75"),
        p90_ms=percentile("90"),
        p99_ms=percentile("99"),
        total_requests=int(requests.group(1)),
        duration_seconds=to_milliseconds(requests.group(2)) / 1000.0,
        total_read_bytes=to_bytes(requests.group(3)),
        non_2xx_3xx=int(non_2xx.group(1)) if non_2xx else 0,
        socket_errors=sum(int(value) for value in sockets.groups()) if sockets else 0,
        timeout_errors=int(sockets.group(4)) if sockets else 0,
    )


# ------------------------------------------------------------------- running


def wait_ready(url: str, timeout_seconds: int = 120) -> None:
    deadline = time.time() + timeout_seconds
    while time.time() < deadline:
        result = compose(
            "exec", "-T", "bench", "curl", "-fsS", "-o", "/dev/null", url,
            check=False, capture_output=True,
        )
        if result.returncode == 0:
            return
        time.sleep(1)
    raise TimeoutError(f"timeout waiting for {url}")


def benchmark_target(url: str, args: argparse.Namespace, wrk_script: str | None) -> str:
    cmd = [
        "exec", "-T", "bench", "wrk", "--latency",
        "-t", str(args.threads),
        "-c", str(args.connections),
        "-d", f"{args.duration}s",
        "--timeout", f"{WRK_TIMEOUT_SECONDS}s",
    ]
    if wrk_script:
        cmd.extend(["-s", wrk_script])
    else:
        # Nothing here is being asked to compress, so nothing is allowed to
        # decide to. A server that returned gzip would be measured on a
        # different response from the others.
        cmd.extend(["-H", "Accept-Encoding: identity"])
    cmd.append(url)
    return compose(*cmd, capture_output=True).stdout


def warmup_target(url: str, args: argparse.Namespace, wrk_script: str | None) -> None:
    if args.warmup:
        warmup = argparse.Namespace(threads=args.threads, connections=args.connections, duration=args.warmup)
        benchmark_target(url, warmup, wrk_script)


def verify_payload(url: str, config: dict) -> dict:
    """Refuse to measure a fast error page or an ignored Range request."""
    full = (WWW / config["path"].lstrip("/")).read_bytes()
    extra = []
    expected = full
    status = 200
    if "range" in config:
        start, end = config["range"]
        extra = ["-H", f"Range: bytes={start}-{end}"]
        expected = full[start:end + 1]
        status = 206
    result = subprocess.run(
        ["docker", "compose", "exec", "-T", "bench", "curl", "-fsS", "-i",
         "-H", "Accept-Encoding: identity", *extra, url],
        cwd=HERE, check=True, capture_output=True, timeout=30,
    )
    head, body = result.stdout.split(b"\r\n\r\n", 1)
    fields = head.split(b"\r\n")
    actual_status = int(fields[0].split()[1])
    headers = {key.strip().lower(): value.strip() for key, value in
               (field.split(b":", 1) for field in fields[1:])}
    if actual_status != status or body != expected:
        raise ValueError(f"wrong status or payload for {url}: {actual_status}, {len(body)} bytes")
    if headers.get(b"content-encoding", b"identity") != b"identity":
        raise ValueError(f"unexpected content encoding for {url}")
    if "range" in config and headers.get(b"content-range") != f"bytes {start}-{end}/{len(full)}".encode():
        raise ValueError(f"wrong Content-Range for {url}")
    return {"url": url, "status": actual_status, "bytes": len(body), "range": config.get("range")}


# ------------------------------------------------------------------ reporting


def git_revision() -> str:
    """Which commit this was, if git will say.

    Never fatal. This is a line in a report header, and it is read after every
    measurement has been taken — throwing away ten minutes of benchmarking
    because git had an opinion about directory ownership would be absurd.
    """

    def ask(*command: str) -> str | None:
        try:
            result = subprocess.run(
                command, cwd=REPO_ROOT, check=True, text=True, capture_output=True
            )
        except (subprocess.CalledProcessError, OSError):
            return None
        return result.stdout.strip()

    head = ask("git", "rev-parse", "--short", "HEAD")
    if head is None:
        return "unknown"
    return f"{head}{' (dirty)' if ask('git', 'status', '--short') else ''}"


def versions(targets=TARGETS) -> dict[str, str]:
    """What was compared.

    gatel's version is read from the workspace manifest and the commit rather
    than from the container: the image has no shell to ask, on purpose, and the
    commit is the more useful answer anyway when the binary was built from this
    tree a moment ago.
    """
    manifest = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
    version = re.search(r'^version\s*=\s*"([^"]+)"', manifest, re.M)
    found = {"gatel": f"{version.group(1) if version else 'unknown'} ({git_revision()})"}

    def from_container(service: str, *command: str) -> str:
        result = compose("exec", "-T", service, *command, check=False, capture_output=True)
        return (result.stdout.strip() or result.stderr.strip() or "unknown").splitlines()[0]

    if "nginx" in targets:
        found["nginx"] = from_container("nginx-static", "nginx", "-v")
    if "caddy" in targets:
        found["caddy"] = from_container("caddy-static", "caddy", "version")
    return {target: found[target] for target in targets}


def mean(values: list[float]) -> float:
    return statistics.fmean(values)


def format_bps(value: float) -> str:
    amount, unit = value, "B/s"
    for unit in ["B/s", "KB/s", "MB/s", "GB/s", "TB/s"]:
        if amount < 1024 or unit == "TB/s":
            break
        amount /= 1024
    return f"{amount:.2f} {unit}"


def generate_report(
    output_dir: Path,
    args: argparse.Namespace,
    software: dict[str, str],
    results: list[BenchResult],
    scenarios: dict,
) -> str:
    grouped: dict[str, list[BenchResult]] = {}
    for result in results:
        grouped.setdefault(result.scenario, []).append(result)

    lines = [
        "# HTTP server benchmark",
        "",
        f"- Generated: {datetime.now().astimezone().isoformat(timespec='seconds')}",
        f"- Host: `{platform.platform()}`, {os.cpu_count() or '?'} logical CPUs",
        "- Compared:",
        *(f"  - `{name}`: `{value}`" for name, value in software.items()),
        "",
        "## Method",
        "",
        "- `wrk` from a container of its own, so it contends with every target alike",
        f"- {args.rounds} round(s) of {args.duration}s each, after a {args.warmup}s warmup",
        f"- {args.threads} threads, {args.connections} connections",
        "- `Accept-Encoding: identity`, so every server returns the same bytes",
        "",
        f"- wrk's latency histogram limit remains {WRK_TIMEOUT_SECONDS}s. Completed requests",
        "  beyond that limit count as timeouts and are excluded from its latency statistics.",
        "  A nonzero over-limit count means p99/Max do not describe the full response tail.",
        "",
        "One machine, one moment. These numbers are for comparing the three",
        "servers against each other here, not for quoting anywhere else.",
        "",
    ]

    for scenario, config in scenarios.items():
        by_target: dict[str, list[BenchResult]] = {}
        for result in grouped.get(scenario, []):
            by_target.setdefault(result.target, []).append(result)
        if not by_target:
            continue
        baseline = mean([item.requests_per_sec for item in by_target.get("gatel", [])] or [0.0])

        rows = sorted(
            (
                {
                    "target": target,
                    "rps": mean([item.requests_per_sec for item in rounds]),
                    "bps": mean([item.transfer_bytes_per_sec for item in rounds]),
                    "latency": mean([item.latency_avg_ms for item in rounds]),
                    "p90": mean([item.p90_ms for item in rounds]),
                    "p99": mean([item.p99_ms for item in rounds]),
                    "errors": sum(item.non_2xx_3xx + item.socket_errors for item in rounds),
                    "timeouts": sum(item.timeout_errors for item in rounds),
                }
                for target, rounds in by_target.items()
            ),
            key=lambda row: row["rps"],
            reverse=True,
        )

        lines += [
            f"## {scenario}",
            "",
            config["description"],
            "",
            "| Target | Avg req/s | vs gatel | Avg latency | Mean round p90 | Mean round p99 | Transfer/sec | Errors | Over-limit completions |",
            "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        ]
        for row in rows:
            ratio = f"{row['rps'] / baseline:.2f}x" if baseline else "N/A"
            lines.append(
                f"| {row['target']} | {row['rps']:,.0f} | {ratio} | "
                f"{row['latency']:.2f} ms | {row['p90']:.2f} ms | {row['p99']:.2f} ms | "
                f"{format_bps(row['bps'])} | {row['errors']} | {row['timeouts']} |"
            )
        lines.append("")

    lines += [
        "## Artifacts",
        "",
        f"- Output: `{output_dir}`",
        "- Raw `wrk` output: `raw/*.txt`",
        "- Machine-readable: `summary.json`",
        "",
    ]
    return "\n".join(lines)


def main() -> int:
    args = parse_args()
    scenarios = {name: SCENARIOS[name] for name in dict.fromkeys(args.scenario or DEFAULT_SCENARIOS)}
    targets = tuple(dict.fromkeys(args.target or TARGETS))

    output_dir = args.output_dir or (OUTPUT_ROOT / datetime.now().strftime("%Y%m%d-%H%M%S"))
    raw_dir = output_dir / "raw"
    raw_dir.mkdir(parents=True, exist_ok=True)

    ensure_payloads()

    results: list[BenchResult] = []
    compose("down", "--volumes", "--remove-orphans", check=False)
    try:
        if not args.no_build:
            builds = ["bench"] + [f"{target}-static" for target in targets if target != "nginx"]
            compose("build", *builds)
        services = ["bench", "backend", *(f"{target}-{kind}" for target in targets for kind in ("static", "proxy"))]
        up = ["up", "-d"]
        if args.no_build:
            up.append("--no-build")
        compose(*up, *services)

        checks = []
        for config in scenarios.values():
            for target in targets:
                url = url_for(config, target)
                wait_ready(url)
                checks.append(verify_payload(url, config))
                (output_dir / "payload-checks.json").write_text(json.dumps(checks, indent=2), encoding="utf-8")

        software = versions(targets)

        for scenario, config in scenarios.items():
            for target in targets:
                url = url_for(config, target)
                print(f"warming up {scenario}/{target}", flush=True)
                warmup_target(url, args, config.get("wrk_script"))
                for round_id in range(1, args.rounds + 1):
                    print(f"benchmarking {scenario}/{target} round {round_id}/{args.rounds}", flush=True)
                    output = benchmark_target(url, args, config.get("wrk_script"))
                    (raw_dir / f"{scenario}-{target}-round{round_id}.txt").write_text(
                        output, encoding="utf-8"
                    )
                    results.append(parse_wrk_output(scenario, target, round_id, output))
                    (output_dir / "summary.json").write_text(
                        json.dumps([asdict(item) for item in results], indent=2), encoding="utf-8"
                    )

        (output_dir / "summary.json").write_text(
            json.dumps([asdict(item) for item in results], indent=2), encoding="utf-8"
        )
        report = generate_report(output_dir, args, software, results, scenarios)
        (output_dir / "report.md").write_text(report, encoding="utf-8")
        print(report)
        print(f"\nReport written to: {output_dir / 'report.md'}")
        return 0
    finally:
        try:
            logs = compose("logs", "--no-color", check=False, capture_output=True)
            (output_dir / "containers.log").write_text(logs.stdout + logs.stderr, encoding="utf-8")
        finally:
            # Logging failures must not leave the benchmark stack running.
            compose("down", "--volumes", "--remove-orphans", check=False)


if __name__ == "__main__":
    sys.exit(main())
