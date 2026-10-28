"""One-off remote execution of the existing benchmark with payload checks."""
import hashlib
import json
import os
import platform
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlsplit

ROOT = Path.cwd()
OUT = Path(os.environ['BENCHMARK_OUTPUT'])
OUT.mkdir(parents=True, exist_ok=True)
sys.path.insert(0, str(ROOT / 'benchmarks/http-servers'))
import run as benchmark

override = OUT / 'compose.override.yml'
override.write_text('services:\n  gatel-static:\n    healthcheck:\n      disable: true\n  gatel-proxy:\n    healthcheck:\n      disable: true\n', encoding='utf-8')
os.environ['COMPOSE_FILE'] = f'{benchmark.HERE / "compose.yml"}:{override}'

metadata = {
    'source_revision': '08eb86eb43fcfc51aed5d77bddf36a569124f2ce',
    'execution_revision': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
    'runner_os': os.environ.get('RUNNER_OS'),
    'runner_arch': os.environ.get('RUNNER_ARCH'),
    'platform': platform.platform(),
    'logical_cpus': os.cpu_count(),
    'notes': [
        'GitHub-hosted one-off runner; results apply only to this machine and run.',
        'Existing run.py and production server sources are unchanged.',
        'Only Gatel image healthchecks are disabled to avoid incidental traffic.',
        'wrk shares CPU with servers; target order is the existing script order.',
    ],
}
(OUT / 'provenance.json').write_text(json.dumps(metadata, indent=2), encoding='utf-8')

checked = set()
proofs = []
original_wait = benchmark.wait_ready

def fetch(url, extra=()):
    args = ['docker', 'compose', 'exec', '-T', 'bench', 'curl', '-fsS', '-i', '-H', 'Accept-Encoding: identity', *extra, url]
    response = subprocess.run(args, cwd=benchmark.HERE, check=True, capture_output=True).stdout
    head, body = response.split(b'\r\n\r\n', 1)
    status = int(head.split(b'\r\n', 1)[0].split()[1])
    headers = {line.split(b':', 1)[0].strip().lower(): line.split(b':', 1)[1].strip() for line in head.split(b'\r\n')[1:]}
    return status, headers, body

def check(url, expected, expected_status, extra=()):
    status, headers, body = fetch(url, extra)
    proof = {'url': url, 'range': bool(extra), 'status': status, 'bytes': len(body), 'sha256': hashlib.sha256(body).hexdigest()}
    proofs.append(proof)
    (OUT / 'payload-checks.json').write_text(json.dumps(proofs, indent=2), encoding='utf-8')
    assert status == expected_status, proof
    assert body == expected, proof
    assert headers.get(b'content-encoding', b'identity') == b'identity', proof
    if extra:
        assert headers.get(b'content-range') == b'bytes 0-65535/10485760', proof

def wait_ready_and_verify(url):
    original_wait(url)
    if url in checked:
        return
    payload = (benchmark.WWW / Path(urlsplit(url).path).name).read_bytes()
    check(url, payload, 200)
    if urlsplit(url).path.endswith('payload-10m.bin'):
        check(url, payload[:65536], 206, ('-H', 'Range: bytes=0-65535'))
    checked.add(url)

benchmark.wait_ready = wait_ready_and_verify
try:
    result = benchmark.main()
finally:
    inspection = subprocess.run(['docker', 'image', 'inspect', 'gatel-http-bench-gatel:local', 'nginx:1.27', 'gatel-http-bench-caddy:local', 'gatel-http-bench-runner:local'], capture_output=True, text=True)
    (OUT / 'image-inspect.json').write_text(inspection.stdout or inspection.stderr, encoding='utf-8')
raise SystemExit(result)
