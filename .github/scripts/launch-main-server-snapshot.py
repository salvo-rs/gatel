"""Run the main-branch three-server snapshot with balanced target order."""
import argparse
from dataclasses import fields
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import threading
import time

ROOT = Path.cwd()
HERE = ROOT / "benchmarks/http-servers"
OUT = Path(os.environ["BENCHMARK_OUTPUT"])
SOURCE = "f11ffccdaaae6249c1f779160b5b84af1f479ae6"
ORDERS = [("gatel", "nginx", "caddy"), ("nginx", "caddy", "gatel"), ("caddy", "gatel", "nginx")]
SCENARIOS = ["static-4k", "static-64k", "static-65k", "range-10m", "range-offset-10m", "range-65k", "static-1m", "static-10m", "proxy"]

actual = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
assert actual == SOURCE, (actual, SOURCE)
OUT.mkdir(parents=True, exist_ok=True)
metadata = {
    "source_revision": SOURCE,
    "execution_revision": os.environ["GITHUB_SHA"],
    "workflow_run_id": os.environ["GITHUB_RUN_ID"],
    "platform": platform.platform(),
    "logical_cpus": os.cpu_count(),
    "duration_seconds": 20,
    "warmup_seconds": 5,
    "threads": 4,
    "connections": 128,
    "repeats": 3,
    "target_orders": ORDERS,
    "scenarios": SCENARIOS,
    "profile": "Repository defaults; identity encoding; warm page cache; co-located load generator; upstream budgets intentionally unchanged.",
}
(OUT / "provenance.json").write_text(json.dumps(metadata, indent=2))
results = []
software = None
network_settings = None

for repeat, order in enumerate(ORDERS, 1):
    phase = OUT / f"{repeat:02d}-{'-'.join(order)}"
    phase.mkdir()
    stop = threading.Event()
    context = {"stage": "startup", "scenario": None, "target": None}

    def sample():
        with (phase / "resources.jsonl").open("w") as output:
            while not stop.is_set():
                before = dict(context)
                try:
                    sampled = subprocess.run(
                        ["docker", "stats", "--no-stream", "--format", "{{json .}}"],
                        capture_output=True, text=True, timeout=10,
                    )
                    record = {"time": time.time(), "context": before,
                              "containers": [json.loads(line) for line in sampled.stdout.splitlines() if line],
                              "returncode": sampled.returncode, "stderr": sampled.stderr}
                except Exception as error:
                    record = {"time": time.time(), "context": before, "sample_error": str(error)}
                output.write(json.dumps(record) + "\n")
                output.flush()
                stop.wait(2)

    monitor = threading.Thread(target=sample, daemon=True)
    monitor.start()
    command = [sys.executable, "-B", "-u", str(HERE / "run.py"), "--no-build",
               "--rounds", "1", "--duration", "20", "--warmup", "5", "--threads", "4",
               "--connections", "128", "--output-dir", str(phase)]
    for target in order:
        command.extend(["--target", target])
    for scenario in SCENARIOS:
        command.extend(["--scenario", scenario])
    print(f"REPEAT {repeat}/3, target order {order}", flush=True)
    try:
        with (phase / "run.log").open("w") as log:
            process = subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE,
                                       stderr=subprocess.STDOUT, text=True)
            for line in process.stdout:
                match = re.match(r"(warming up|benchmarking) ([^/]+)/([^ ]+)", line.strip())
                if match:
                    context.update(stage="warmup" if match[1] == "warming up" else "measurement",
                                   scenario=match[2], target=match[3])
                print(line, end="", flush=True)
                log.write(line)
                log.flush()
            if process.wait():
                raise RuntimeError(f"repeat {repeat} failed")
    finally:
        stop.set()
        monitor.join(timeout=15)

    settings = json.loads((phase / "network-settings.json").read_text())
    assert settings.get("tcp_rmem_bytes"), settings
    if network_settings is None:
        network_settings = settings
    else:
        assert network_settings == settings, (network_settings, settings)
    metadata["client_network"] = network_settings
    rows = json.loads((phase / "summary.json").read_text())
    proofs = json.loads((phase / "payload-checks.json").read_text())
    assert len(rows) == len(proofs) == 27
    versions = dict(re.findall(r"^  - `([^`]+)`: `([^`]+)`$", (phase / "report.md").read_text(), re.M))
    assert set(versions) == {"gatel", "nginx", "caddy"}, versions
    if software is None:
        software = versions
    else:
        assert software == versions, (software, versions)
    for row in rows:
        row.update(round_id=repeat, target_order=list(order), source_revision=SOURCE,
                   phase_directory=phase.name)
    results.extend(rows)
    (OUT / "summary.json").write_text(json.dumps(results, indent=2))
    metadata["software"] = software
    (OUT / "provenance.json").write_text(json.dumps(metadata, indent=2))

assert len(results) == 81
sys.path.insert(0, str(HERE))
import run as bench

keys = {field.name for field in fields(bench.BenchResult)}
typed = [bench.BenchResult(**{key: row[key] for key in keys}) for row in results]
args = argparse.Namespace(rounds=3, duration=20, warmup=5, threads=4, connections=128)
report = bench.generate_report(OUT, args, software, typed, {name: bench.SCENARIOS[name] for name in SCENARIOS}, network_settings)
report += "\nTarget order rotates once per repeat: gatel/nginx/caddy, nginx/caddy/gatel, caddy/gatel/nginx.\n"
report += "Each repeat recreates the same prebuilt services and performs the same workload-specific warmup.\n"
report += "Percentiles are means of per-round percentiles, not pooled request percentiles.\n"
(OUT / "report.md").write_text(report)
subprocess.run(["git", "diff", "--exit-code"], check=True)
print(f"Completed 81 measurements; reported errors: {sum(row['non_2xx_3xx'] + row['socket_errors'] for row in results)}", flush=True)
print(report, flush=True)
