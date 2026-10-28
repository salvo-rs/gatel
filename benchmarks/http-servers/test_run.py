"""Harness regressions; no Docker daemon is needed."""
import argparse
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import run as bench


class HarnessTests(unittest.TestCase):
    def test_report_without_gatel_does_not_invent_a_baseline_ratio(self):
        args = argparse.Namespace(rounds=1, duration=2, warmup=0, threads=2, connections=8)
        row = SimpleNamespace(scenario="static-64k", target="nginx", requests_per_sec=100,
                              transfer_bytes_per_sec=6553600, latency_avg_ms=1, p90_ms=2,
                              p99_ms=3, non_2xx_3xx=0, socket_errors=0)
        report = bench.generate_report(Path("results"), args, {"nginx": "test"}, [row],
                                       {"static-64k": bench.SCENARIOS["static-64k"]})
        self.assertIn("| nginx | 100 | N/A |", report)

    def test_wrk_errors_and_latency_units_are_not_lost(self):
        raw = """Latency 1.25ms 700.00us 1.10s 90.00%
  50% 800.00us
  75% 1.00ms
  90% 2.00ms
  99% 1.00s
100 requests in 10.01s, 2.50MB read
Socket errors: connect 1, read 2, write 3, timeout 4
Non-2xx or 3xx responses: 5
Requests/sec: 9.99
Transfer/sec: 255.75KB
"""
        result = bench.parse_wrk_output("proxy", "gatel", 1, raw)
        self.assertEqual(result.socket_errors, 10)
        self.assertEqual(result.non_2xx_3xx, 5)
        self.assertEqual(result.p99_ms, 1000)
        self.assertEqual(result.latency_stdev_ms, 0.7)
        self.assertEqual(result.transfer_bytes_per_sec, 255.75 * 1024)
        with self.assertRaises(RuntimeError):
            bench.parse_wrk_output("proxy", "gatel", 1, raw.replace("Requests/sec:", "Missing:"))

    def test_preflight_rejects_wrong_range_encoding_status_and_bytes(self):
        payload = b"0123456789"
        config = {"path": "/file.bin", "range": (2, 5)}
        good = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-5/10\r\n\r\n2345"
        invalid = [
            good.replace(b"206 Partial Content", b"200 OK"),
            good.replace(b"2-5/10", b"0-3/10"),
            good.replace(b"2345", b"wrong"),
            good.replace(b"\r\n\r\n", b"\r\nContent-Encoding: gzip\r\n\r\n"),
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "file.bin").write_bytes(payload)
            with patch.object(bench, "WWW", root), patch.object(bench.subprocess, "run") as fetch:
                fetch.return_value = SimpleNamespace(stdout=good)
                proof = bench.verify_payload("http://gatel-static:8080/file.bin", config)
                self.assertEqual(proof["bytes"], 4)
                for response in invalid:
                    with self.subTest(response=response):
                        fetch.return_value = SimpleNamespace(stdout=response)
                        with self.assertRaises(ValueError):
                            bench.verify_payload("http://gatel-static:8080/file.bin", config)

    def test_warmup_uses_the_measured_range_and_concurrency(self):
        args = argparse.Namespace(warmup=5, threads=4, connections=128)
        with patch.object(bench, "benchmark_target") as load:
            bench.warmup_target("http://example/file", args, "/workspace/range-offset-64k.lua")
            url, warmup, script = load.call_args.args
            self.assertEqual(url, "http://example/file")
            self.assertEqual((warmup.duration, warmup.threads, warmup.connections), (5, 4, 128))
            self.assertEqual(script, "/workspace/range-offset-64k.lua")
            load.reset_mock()
            args.warmup = 0
            bench.warmup_target(url, args, script)
            load.assert_not_called()


if __name__ == "__main__":
    unittest.main()
