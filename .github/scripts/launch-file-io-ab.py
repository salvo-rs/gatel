"""One-off same-machine ABBAAB comparison; product Dockerfiles are unchanged."""
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import threading
import time

ROOT = Path.cwd()
OUT = Path(os.environ['BENCHMARK_OUTPUT'])
OUT.mkdir(parents=True, exist_ok=True)
BASE = '08eb86eb43fcfc51aed5d77bddf36a569124f2ce'
CANDIDATE = '1a001087cf87b20c4a3dbb600a8cbda6ca620199'
HERE = ROOT / 'benchmarks/http-servers'
SCENARIOS = ['static-10m']
ORDER = ['base', 'candidate', 'candidate', 'base', 'base', 'candidate']

def run(args, **kwargs):
    print('$', ' '.join(map(str, args)), flush=True)
    return subprocess.run(list(map(str, args)), check=True, **kwargs)

metadata = {'base_revision': BASE, 'candidate_revision': CANDIDATE,
            'execution_revision': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
            'platform': platform.platform(), 'logical_cpus': os.cpu_count(),
            'order': ORDER, 'duration': 60, 'warmup': 5, 'threads': 4, 'connections': 128,
            'scenarios': SCENARIOS, 'note': 'Same candidate harness and default upstream configuration for both product images; Isolated 10 MiB stream tail-latency recheck; neither revision runs the small-response optimization in this scenario; 60 seconds per phase.'}
(OUT / 'provenance.json').write_text(json.dumps(metadata, indent=2))
run(['docker', 'compose', 'build', 'bench'], cwd=HERE)
run(['docker', 'build', '-t', 'gatel-file-io:base', ROOT.parent / 'baseline'])
run(['docker', 'build', '-t', 'gatel-file-io:candidate', ROOT])
inspect = run(['docker', 'image', 'inspect', 'gatel-file-io:base', 'gatel-file-io:candidate', 'gatel-http-bench-runner:local'], capture_output=True, text=True)
(OUT / 'image-inspect.json').write_text(inspect.stdout)

all_results = []
for phase_id, version in enumerate(ORDER, 1):
    phase = OUT / f'{phase_id:02d}-{version}'
    phase.mkdir()
    override = phase / 'compose.override.yml'
    override.write_text(f'services:\n  gatel-static:\n    image: gatel-file-io:{version}\n  gatel-proxy:\n    image: gatel-file-io:{version}\n')
    env = dict(os.environ, COMPOSE_FILE=f'{HERE / "compose.yml"}:{override}')
    stopped = threading.Event()

    def sample():
        with (phase / 'resources.jsonl').open('w') as output:
            while not stopped.is_set():
                try:
                    sampled = subprocess.run(['docker', 'stats', '--no-stream', '--format', '{{json .}}'], capture_output=True, text=True, timeout=10)
                    record = {'time': time.time(), 'containers': [json.loads(line) for line in sampled.stdout.splitlines() if line], 'stderr': sampled.stderr}
                    output.write(json.dumps(record) + '\n')
                    output.flush()
                except Exception as error:
                    output.write(json.dumps({'time': time.time(), 'sample_error': str(error)}) + '\n')
                    output.flush()
                stopped.wait(2)

    monitor = threading.Thread(target=sample, daemon=True)
    monitor.start()
    command = [sys.executable, '-u', HERE / 'run.py', '--target', 'gatel', '--no-build',
               '--rounds', '1', '--duration', '60', '--warmup', '5', '--threads', '4', '--connections', '128', '--output-dir', phase]
    for scenario in SCENARIOS:
        command.extend(['--scenario', scenario])
    print(f'PHASE {phase_id}/{len(ORDER)}: {version}', flush=True)
    try:
        with (phase / 'run.log').open('w') as log:
            process = subprocess.Popen(list(map(str, command)), env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            for line in process.stdout:
                print(line, end='', flush=True)
                log.write(line)
                log.flush()
            if process.wait():
                raise RuntimeError(f'phase {phase_id}/{version} failed')
    finally:
        stopped.set()
        monitor.join(timeout=15)
    results = json.loads((phase / 'summary.json').read_text())
    assert len(results) == len(SCENARIOS)
    for row in results:
        row.update(version=version, phase=phase_id, source_revision=BASE if version == 'base' else CANDIDATE)
        all_results.append(row)
    (OUT / 'comparison.json').write_text(json.dumps(all_results, indent=2))

assert len(all_results) == 6
print('Completed all six isolated streamed-file measurements.', flush=True)
