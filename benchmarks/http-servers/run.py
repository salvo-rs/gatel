#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import platform
import re
import statistics
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from datetime import datetime
from pathlib import Path


HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
OUTPUT_ROOT = REPO_ROOT / "target" / "benchmarks" / "http-servers"
SCENARIOS = {
    "static-4k": {
        "description": "Static file serving — 4 KiB text file.",
        "targets": {
            "gatel": "http://gatel-static:8080/payload-4k.txt",
            "ferron": "http://ferron-static:8080/payload-4k.txt",
            "nginx": "http://nginx-static:8080/payload-4k.txt",
            "caddy": "http://caddy-static:8080/payload-4k.txt",
        },
    },
    "static-1m": {
        "description": "Static file serving — 1 MiB binary (chunked streaming).",
        "targets": {
            "gatel": "http://gatel-static:8080/payload-1m.bin",
            "ferron": "http://ferron-static:8080/payload-1m.bin",
            "nginx": "http://nginx-static:8080/payload-1m.bin",
            "caddy": "http://caddy-static:8080/payload-1m.bin",
        },
    },
    "static-10m": {
        "description": "Static file serving — 10 MiB binary (sustained streaming throughput).",
        "targets": {
            "gatel": "http://gatel-static:8080/payload-10m.bin",
            "ferron": "http://ferron-static:8080/payload-10m.bin",
            "nginx": "http://nginx-static:8080/payload-10m.bin",
            "caddy": "http://caddy-static:8080/payload-10m.bin",
        },
    },
    "range-10m": {
        "description": "Range request — first 64 KiB of a 10 MiB file (video seek simulation).",
        "wrk_script": "/workspace/range-64k.lua",
        "targets": {
            "gatel": "http://gatel-static:8080/payload-10m.bin",
            "ferron": "http://ferron-static:8080/payload-10m.bin",
            "nginx": "http://nginx-static:8080/payload-10m.bin",
            "caddy": "http://caddy-static:8080/payload-10m.bin",
        },
    },
    "proxy": {
        "description": "Reverse proxying — 4 KiB upstream response.",
        "targets": {
            "gatel": "http://gatel-proxy:8080/payload-4k.txt",
            "ferron": "http://ferron-proxy:8080/payload-4k.txt",
            "nginx": "http://nginx-proxy:8080/payload-4k.txt",
            "caddy": "http://caddy-proxy:8080/payload-4k.txt",
        },
    },
}
COMPOSE_SERVICES = [
    "bench",
    "backend",
    "gatel-static",
    "ferron-static",
    "nginx-static",
    "caddy-static",
    "gatel-proxy",
    "ferron-proxy",
    "nginx-proxy",
    "caddy-proxy",
]


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


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run HTTP server benchmarks")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--duration", type=int, default=10, help="seconds per round")
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--connections", type=int, default=128)
    parser.add_argument("--warmup", type=int, default=3, help="seconds per target")
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=None,
        help="write results to this directory instead of target/benchmarks/http-servers/<timestamp>",
    )
    return parser.parse_args()


def run_command(
    args: list[str],
    *,
    check: bool = True,
    capture_output: bool = False,
) -> subprocess.CompletedProcess[str]:
    print("$", " ".join(args), flush=True)
    return subprocess.run(
        args,
        cwd=HERE,
        check=check,
        text=True,
        capture_output=capture_output,
    )


def compose(*args: str, check: bool = True, capture_output: bool = False) -> subprocess.CompletedProcess[str]:
    return run_command(
        ["docker", "compose", *args],
        check=check,
        capture_output=capture_output,
    )


def to_milliseconds(value: str) -> float:
    match = re.fullmatch(r"([0-9.]+)(us|ms|s)", value.strip())
    if not match:
        raise ValueError(f"unrecognized latency value: {value!r}")
    amount = float(match.group(1))
    unit = match.group(2)
    if unit == "us":
        return amount / 1000.0
    if unit == "ms":
        return amount
    return amount * 1000.0


def to_bytes(value: str) -> float:
    match = re.fullmatch(r"([0-9.]+)(B|KB|MB|GB|TB)", value.strip())
    if not match:
        raise ValueError(f"unrecognized byte value: {value!r}")
    amount = float(match.group(1))
    unit = match.group(2)
    scale = {
        "B": 1,
        "KB": 1024,
        "MB": 1024**2,
        "GB": 1024**3,
        "TB": 1024**4,
    }[unit]
    return amount * scale


def parse_wrk_output(scenario: str, target: str, round_id: int, text: str) -> BenchResult:
    latency_match = re.search(
        r"Latency\s+([0-9.]+(?:us|ms|s))\s+([0-9.]+(?:us|ms|s))\s+([0-9.]+(?:us|ms|s))",
        text,
    )
    if not latency_match:
        raise RuntimeError(f"failed to parse latency line for {scenario}/{target}/round-{round_id}")
    requests_match = re.search(
        r"(\d+)\s+requests in\s+([0-9.]+)s,\s+([0-9.]+(?:B|KB|MB|GB|TB))\s+read",
        text,
    )
    if not requests_match:
        raise RuntimeError(f"failed to parse requests summary for {scenario}/{target}/round-{round_id}")
    req_per_sec_match = re.search(r"Requests/sec:\s+([0-9.]+)", text)
    transfer_match = re.search(r"Transfer/sec:\s+([0-9.]+(?:B|KB|MB|GB|TB))", text)
    if not req_per_sec_match or not transfer_match:
        raise RuntimeError(f"failed to parse throughput for {scenario}/{target}/round-{round_id}")

    def percentile(name: str) -> float:
        match = re.search(rf"{name}%\s+([0-9.]+(?:us|ms|s))", text)
        if not match:
            raise RuntimeError(f"failed to parse {name} percentile for {scenario}/{target}/round-{round_id}")
        return to_milliseconds(match.group(1))

    non_2xx_3xx = 0
    non_2xx_match = re.search(r"Non-2xx or 3xx responses:\s+(\d+)", text)
    if non_2xx_match:
        non_2xx_3xx = int(non_2xx_match.group(1))

    socket_errors = 0
    socket_error_match = re.search(
        r"Socket errors:\s+connect\s+(\d+),\s+read\s+(\d+),\s+write\s+(\d+),\s+timeout\s+(\d+)",
        text,
    )
    if socket_error_match:
        socket_errors = sum(int(value) for value in socket_error_match.groups())

    return BenchResult(
        scenario=scenario,
        target=target,
        round_id=round_id,
        requests_per_sec=float(req_per_sec_match.group(1)),
        transfer_bytes_per_sec=to_bytes(transfer_match.group(1)),
        latency_avg_ms=to_milliseconds(latency_match.group(1)),
        latency_stdev_ms=to_milliseconds(latency_match.group(2)),
        latency_max_ms=to_milliseconds(latency_match.group(3)),
        p50_ms=percentile("50"),
        p75_ms=percentile("75"),
        p90_ms=percentile("90"),
        p99_ms=percentile("99"),
        total_requests=int(requests_match.group(1)),
        duration_seconds=float(requests_match.group(2)),
        total_read_bytes=to_bytes(requests_match.group(3)),
        non_2xx_3xx=non_2xx_3xx,
        socket_errors=socket_errors,
    )


def mean(values: list[float]) -> float:
    return statistics.fmean(values)


def format_rate(value: float) -> str:
    return f"{value:,.0f} req/s"


def format_ms(value: float) -> str:
    return f"{value:.2f} ms"


def format_bps(value: float) -> str:
    units = ["B/s", "KB/s", "MB/s", "GB/s", "TB/s"]
    amount = value
    unit = units[0]
    for unit in units:
        if amount < 1024 or unit == units[-1]:
            break
        amount /= 1024
    return f"{amount:.2f} {unit}"


def collect_versions() -> dict[str, str]:
    versions = {}
    versions["gatel"] = compose(
        "exec",
        "-T",
        "gatel-static",
        "gatel",
        "--version",
        capture_output=True,
    ).stdout.strip()
    versions["nginx"] = compose(
        "exec",
        "-T",
        "nginx-static",
        "nginx",
        "-v",
        capture_output=True,
    ).stderr.strip() or compose(
        "exec",
        "-T",
        "nginx-static",
        "nginx",
        "-v",
        capture_output=True,
    ).stdout.strip()
    versions["caddy"] = compose(
        "exec",
        "-T",
        "caddy-static",
        "caddy",
        "version",
        capture_output=True,
    ).stdout.strip()
    ferron_result = compose(
        "exec",
        "-T",
        "ferron-static",
        "/usr/sbin/ferron",
        "--version",
        capture_output=True,
        check=False,
    )
    versions["ferron"] = (ferron_result.stdout.strip() or ferron_result.stderr.strip() or "unknown")
    return versions


def git_revision() -> str:
    head = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"],
        cwd=REPO_ROOT,
        check=True,
        text=True,
        capture_output=True,
    ).stdout.strip()
    dirty = subprocess.run(
        ["git", "status", "--short"],
        cwd=REPO_ROOT,
        check=True,
        text=True,
        capture_output=True,
    ).stdout.strip()
    return f"{head}{' (dirty)' if dirty else ''}"


def host_summary() -> dict[str, str]:
    return {
        "platform": platform.platform(),
        "python": platform.python_version(),
        "cpu_count": str(os.cpu_count() or "unknown"),
    }


def wait_ready(url: str, timeout_seconds: int = 90) -> None:
    deadline = time.time() + timeout_seconds
    while time.time() < deadline:
        result = compose(
            "exec",
            "-T",
            "bench",
            "curl",
            "-fsS",
            "-o",
            "/dev/null",
            url,
            check=False,
            capture_output=True,
        )
        if result.returncode == 0:
            return
        time.sleep(1)
    raise TimeoutError(f"timeout waiting for {url}")


def benchmark_target(
    url: str,
    duration: int,
    threads: int,
    connections: int,
    wrk_script: str | None = None,
) -> str:
    cmd = [
        "exec",
        "-T",
        "bench",
        "wrk",
        "--latency",
        "-t",
        str(threads),
        "-c",
        str(connections),
        "-d",
        f"{duration}s",
        "--timeout",
        "5s",
    ]
    if wrk_script:
        cmd.extend(["-s", wrk_script])
    else:
        cmd.extend(["-H", "Accept-Encoding: identity"])
    cmd.append(url)
    result = compose(*cmd, capture_output=True)
    return result.stdout


def warmup_target(url: str, duration: int) -> None:
    compose(
        "exec",
        "-T",
        "bench",
        "wrk",
        "-t",
        "2",
        "-c",
        "32",
        "-d",
        f"{duration}s",
        "-H",
        "Accept-Encoding: identity",
        url,
        capture_output=True,
    )


def generate_report(
    output_dir: Path,
    args: argparse.Namespace,
    versions: dict[str, str],
    results: list[BenchResult],
) -> str:
    grouped: dict[str, list[BenchResult]] = {}
    for result in results:
        grouped.setdefault(result.scenario, []).append(result)

    report_lines = [
        "# HTTP Server Benchmark Report",
        "",
        f"- Generated at: {datetime.now().astimezone().isoformat(timespec='seconds')}",
        f"- Git revision: `{git_revision()}`",
        f"- Host platform: `{host_summary()['platform']}`",
        f"- Host logical CPUs: `{host_summary()['cpu_count']}`",
        f"- Python: `{host_summary()['python']}`",
        "- Compared software:",
        f"  - `gatel`: `{versions['gatel']}`",
        f"  - `ferron`: `{versions.get('ferron', 'unknown')}`",
        f"  - `nginx`: `{versions['nginx']}`",
        f"  - `caddy`: `{versions['caddy']}`",
        "",
        "## Method",
        "",
        "- Tool: `wrk` inside a dedicated benchmark container",
        f"- Rounds per target: `{args.rounds}`",
        f"- Duration per round: `{args.duration}s`",
        f"- Threads: `{args.threads}`",
        f"- Connections: `{args.connections}`",
        f"- Warmup per target: `{args.warmup}s`",
        "- Request header override: `Accept-Encoding: identity`",
        "",
    ]

    for scenario, scenario_config in SCENARIOS.items():
        scenario_results = grouped[scenario]
        by_target: dict[str, list[BenchResult]] = {}
        for result in scenario_results:
            by_target.setdefault(result.target, []).append(result)

        baseline = mean([item.requests_per_sec for item in by_target["gatel"]])
        ranking = []
        for target, rounds in by_target.items():
            ranking.append(
                {
                    "target": target,
                    "requests_per_sec": mean([item.requests_per_sec for item in rounds]),
                    "transfer_bytes_per_sec": mean([item.transfer_bytes_per_sec for item in rounds]),
                    "latency_avg_ms": mean([item.latency_avg_ms for item in rounds]),
                    "p90_ms": mean([item.p90_ms for item in rounds]),
                    "p99_ms": mean([item.p99_ms for item in rounds]),
                    "errors": sum(item.non_2xx_3xx + item.socket_errors for item in rounds),
                }
            )
        ranking.sort(key=lambda item: item["requests_per_sec"], reverse=True)

        report_lines.extend(
            [
                f"## {scenario.title()}",
                "",
                scenario_config["description"],
                "",
                "| Target | Avg req/s | vs gatel | Avg latency | Avg p90 | Avg p99 | Transfer/sec | Errors |",
                "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
            ]
        )
        for row in ranking:
            ratio = row["requests_per_sec"] / baseline if baseline else 0.0
            report_lines.append(
                "| {target} | {rps} | {ratio:.2f}x | {lat} | {p90} | {p99} | {bps} | {errors} |".format(
                    target=row["target"],
                    rps=format_rate(row["requests_per_sec"]),
                    ratio=ratio,
                    lat=format_ms(row["latency_avg_ms"]),
                    p90=format_ms(row["p90_ms"]),
                    p99=format_ms(row["p99_ms"]),
                    bps=format_bps(row["transfer_bytes_per_sec"]),
                    errors=row["errors"],
                )
            )
        report_lines.append("")

    report_lines.extend(
        [
            "## Artifacts",
            "",
            f"- Output directory: `{output_dir}`",
            "- Raw `wrk` logs: `raw/*.txt`",
            "- Machine-readable summary: `summary.json`",
            "",
        ]
    )

    return "\n".join(report_lines)


def main() -> int:
    args = parse_args()
    timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    output_dir = args.output_dir or (OUTPUT_ROOT / timestamp)
    raw_dir = output_dir / "raw"
    raw_dir.mkdir(parents=True, exist_ok=True)

    results: list[BenchResult] = []
    versions: dict[str, str] = {}

    compose("down", "--volumes", "--remove-orphans", check=False)
    try:
        compose("build", "bench", "gatel-static", "gatel-proxy")
        compose("up", "-d", *COMPOSE_SERVICES)

        for scenario_config in SCENARIOS.values():
            for url in scenario_config["targets"].values():
                wait_ready(url)

        versions = collect_versions()

        for scenario, scenario_config in SCENARIOS.items():
            wrk_script = scenario_config.get("wrk_script")
            for target, url in scenario_config["targets"].items():
                print(f"warming up {scenario}/{target}", flush=True)
                warmup_target(url, args.warmup)
                for round_id in range(1, args.rounds + 1):
                    print(f"benchmarking {scenario}/{target} round {round_id}/{args.rounds}", flush=True)
                    output = benchmark_target(url, args.duration, args.threads, args.connections, wrk_script)
                    raw_path = raw_dir / f"{scenario}-{target}-round{round_id}.txt"
                    raw_path.write_text(output, encoding="utf-8")
                    results.append(parse_wrk_output(scenario, target, round_id, output))

        summary_path = output_dir / "summary.json"
        summary_path.write_text(
            json.dumps([asdict(item) for item in results], indent=2),
            encoding="utf-8",
        )
        report_text = generate_report(output_dir, args, versions, results)
        report_path = output_dir / "report.md"
        report_path.write_text(report_text, encoding="utf-8")
        print(report_text)
        print(f"\nReport written to: {report_path}")
        return 0
    finally:
        compose("down", "--volumes", "--remove-orphans", check=False)


if __name__ == "__main__":
    sys.exit(main())
