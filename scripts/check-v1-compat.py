#!/usr/bin/env python3
"""Compare a bounded common HTTP profile using real v1 and v2 executables.

Only temporary loopback servers are started. Neither executable is installed.
Logs and a JSON report are kept in --work-dir, including on failure.
"""

import argparse
import contextlib
import hashlib
import http.client
import http.server
import json
import pathlib
import socket
import subprocess
import threading
import time


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def exchange(address, method, path, body=None, headers=None):
    connection = http.client.HTTPConnection("127.0.0.1", address, timeout=3)
    try:
        connection.request(method, path, body, headers or {})
        response = connection.getresponse()
        selected = {key: response.getheader(key) for key in ("content-range",)}
        return {"status": response.status, "headers": selected, "body": response.read().hex()}
    finally:
        connection.close()


@contextlib.contextmanager
def gateway(binary, version, config, directory):
    address = port()
    source = directory / (version + ".kdl")
    source.write_text(config.replace("@PORT@", str(address)), encoding="utf-8")
    command = [str(binary), "run"]
    if version == "v1":
        command.extend(["--config", str(source)])
    else:
        command.append(str(source))
    # A file avoids a full stdout/stderr pipe blocking the gateway under test.
    with (directory / (version + ".log")).open("wb") as log:
        process = subprocess.Popen(
            command, cwd=directory, stdout=log, stderr=subprocess.STDOUT,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
        try:
            deadline = time.monotonic() + 15
            while True:
                if process.poll() is not None:
                    raise RuntimeError(f"{version} exited {process.returncode}; see {version}.log")
                try:
                    # Readiness is an actual request, not only a successful TCP connect.
                    exchange(address, "GET", "/ready")
                    break
                except (OSError, http.client.HTTPException):
                    if time.monotonic() >= deadline:
                        raise RuntimeError(f"{version} did not serve; see {version}.log")
                    time.sleep(0.05)
            yield address
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


class Origin(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.respond()

    def do_POST(self):
        self.respond()

    def respond(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        payload = self.command.encode() + b":" + body
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_args):
        pass


def run(v1, v2, directory):
    assets = directory / "assets"
    assets.mkdir()
    (assets / "asset.txt").write_bytes(b"abcdef")
    root = json.dumps(assets.as_posix(), ensure_ascii=False)
    origin = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Origin)
    worker = threading.Thread(target=origin.serve_forever, daemon=True)
    worker.start()
    backend = f"127.0.0.1:{origin.server_port}"
    common = '''
        route "/*" { respond "fallback" }
        route "/ready" { respond "ready" }
        route "/exact" { respond "exact" }
        route "/created" { respond "created" status=201 }
    '''
    configurations = {
        "v1": f'''global {{ http "127.0.0.1:@PORT@" }}
site "*" {{ {common}
    route "/method" {{ match method="POST"; respond "posted" }}
    route "/static/*" {{ root {root}; rewrite {{ strip-prefix "/static" }}; file-server }}
    route "/proxy/*" {{ proxy {{ upstream "{backend}" }} }}
}}
''',
        "v2": f'''global {{ http "127.0.0.1:@PORT@" }}
site "*" {{ {common}
    route "/method" {{ method "POST"; respond "posted" }}
    route "/static/*" {{ rewrite strip-prefix="/static"; file-server root={root} }}
    route "/proxy/*" {{ proxy "{backend}" }}
}}
''',
    }
    cases = [
        ("exact beats earlier prefix", "GET", "/exact", None, {}, 200, b"exact"),
        ("prefix fallback", "GET", "/other", None, {}, 200, b"fallback"),
        ("explicit status", "GET", "/created", None, {}, 201, b"created"),
        ("HEAD has no body", "HEAD", "/exact", None, {}, 200, b""),
        ("POST method match", "POST", "/method", None, {}, 200, b"posted"),
        ("method mismatch falls through", "GET", "/method", None, {}, 200, b"fallback"),
        ("static bytes", "GET", "/static/asset.txt", None, {}, 200, b"abcdef"),
        ("static HEAD", "HEAD", "/static/asset.txt", None, {}, 200, b""),
        ("static range", "GET", "/static/asset.txt", None, {"Range": "bytes=1-3"}, 206, b"bcd"),
        ("proxy GET", "GET", "/proxy/test", None, {}, 200, b"GET:"),
        ("proxy POST body", "POST", "/proxy/test", b"payload", {}, 200, b"POST:payload"),
    ]
    report = {"profile": "common-http-v1", "binaries": {}, "cases": []}
    failed = False
    try:
        for version, binary in (("v1", v1), ("v2", v2)):
            identity = subprocess.run(
                [str(binary), "--version"], capture_output=True, timeout=5, check=True,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            )
            with binary.open("rb") as executable:
                digest = hashlib.file_digest(executable, "sha256").hexdigest()
            report["binaries"][version] = {
                "path": str(binary), "version": identity.stdout.decode().strip(), "sha256": digest,
            }
        with gateway(v1, "v1", configurations["v1"], directory) as old:
            with gateway(v2, "v2", configurations["v2"], directory) as new:
                for name, method, path, body, headers, status, expected in cases:
                    before = exchange(old, method, path, body, headers)
                    after = exchange(new, method, path, body, headers)
                    passed = before == after and after["status"] == status and after["body"] == expected.hex()
                    if name == "static range":
                        passed = passed and after["headers"]["content-range"] == "bytes 1-3/6"
                    report["cases"].append({"name": name, "passed": passed, "v1": before, "v2": after})
                    print(f"{'PASS' if passed else 'FAIL'} {name}")
                    failed |= not passed
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        origin.shutdown()
        origin.server_close()
        worker.join(timeout=5)
        (directory / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return 1 if failed else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--v1", type=pathlib.Path, required=True)
    parser.add_argument("--v2", type=pathlib.Path, required=True)
    parser.add_argument("--work-dir", type=pathlib.Path, required=True, help="new output directory")
    args = parser.parse_args()
    for binary in (args.v1, args.v2):
        if not binary.is_file():
            parser.error(f"executable not found: {binary}")
    directory = args.work_dir.resolve()
    directory.mkdir(parents=True, exist_ok=False)
    try:
        return run(args.v1.resolve(), args.v2.resolve(), directory)
    except (OSError, RuntimeError, subprocess.SubprocessError, http.client.HTTPException) as error:
        print(f"ERROR: {error}")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
