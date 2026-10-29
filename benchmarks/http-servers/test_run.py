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
                              p99_ms=3, non_2xx_3xx=0, socket_errors=0, timeout_errors=0)
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
        self.assertEqual(result.timeout_errors, 4)
        args = argparse.Namespace(rounds=1, duration=10, warmup=0, threads=2, connections=8)
        report = bench.generate_report(Path("results"), args, {"gatel": "test"}, [result],
                                       {"proxy": bench.SCENARIOS["proxy"]}, {"tcp_rmem_bytes": [4096, 131072, 524288]})
        self.assertIn("Client tcp_rmem (bytes): 4096 131072 524288", report)
        self.assertIn("Over-limit completions", report)
        self.assertIn("| 15 | 4 |", report)
        self.assertIn("excluded from its latency statistics", report)
        self.assertIn("Requests still in flight when wrk stops are also absent", report)
        clean = bench.parse_wrk_output("proxy", "gatel", 1,
                                      raw.replace("Socket errors: connect 1, read 2, write 3, timeout 4", ""))
        self.assertEqual(clean.timeout_errors, 0)
        self.assertEqual(clean.socket_errors, 0)
        for malformed in ["Socket errors: connect 1, read 2, write 3",
                          "Socket errors: connect 1, read 2, write 3, timeout bad"]:
            with self.subTest(malformed=malformed), self.assertRaises(RuntimeError):
                bench.parse_wrk_output("proxy", "gatel", 1,
                                      raw.replace("Socket errors: connect 1, read 2, write 3, timeout 4", malformed))
        self.assertEqual(result.non_2xx_3xx, 5)
        self.assertEqual(result.p99_ms, 1000)
        self.assertEqual(result.latency_stdev_ms, 0.7)
        self.assertEqual(result.transfer_bytes_per_sec, 255.75 * 1024)
        with self.assertRaises(RuntimeError):
            bench.parse_wrk_output("proxy", "gatel", 1, raw.replace("Requests/sec:", "Missing:"))

    def test_wrk_minutes_and_hours_keep_duration_and_tail_units(self):
        raw = """Latency 410.75ms 147.85ms 1.36s 71.31%
  50% 435.66ms
  75% 508.69ms
  90% 559.81ms
  99% 789.83ms
16549 requests in 1.00m, 162.33GB read
Requests/sec: 275.38
Transfer/sec: 2.70GB
"""
        minute = bench.parse_wrk_output("static-10m", "gatel", 1, raw)
        self.assertEqual(minute.duration_seconds, 60)
        self.assertEqual(minute.total_requests, 16549)
        self.assertEqual(minute.p99_ms, 789.83)
        hour = bench.parse_wrk_output(
            "static-10m", "gatel", 1,
            raw.replace("1.00m,", "1.50h,").replace("1.36s", "1.36h").replace("789.83ms", "1.00m"),
        )
        self.assertEqual(hour.duration_seconds, 5400)
        self.assertEqual(hour.latency_max_ms, 1.36 * 3600000)
        self.assertEqual(hour.p99_ms, 60000)

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

    def test_no_build_refuses_to_start_without_prebuilt_images(self):
        with tempfile.TemporaryDirectory() as directory:
            args = argparse.Namespace(
                scenario=["static-4k"], target=["gatel"], no_build=True,
                output_dir=Path(directory), rounds=1, duration=1, warmup=0, threads=1, connections=1,
            )

            def compose(*command, **kwargs):
                if command[0] == "up":
                    if "--no-build" not in command:
                        self.fail("compose up could implicitly build missing images")
                    raise RuntimeError("prebuilt image is missing")
                return SimpleNamespace(stdout="", stderr="")

            with patch.object(bench, "parse_args", return_value=args), \
                 patch.object(bench, "ensure_payloads"), \
                 patch.object(bench, "compose", side_effect=compose) as commands:
                with self.assertRaisesRegex(RuntimeError, "prebuilt image is missing"):
                    bench.main()
                self.assertFalse(any(call.args[0] == "build" for call in commands.call_args_list))

    def test_cleanup_runs_when_log_capture_or_log_storage_fails(self):
        for failure in ("capture", "storage"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                args = argparse.Namespace(
                    scenario=["static-4k"], target=["gatel"], no_build=True,
                    output_dir=Path(directory), rounds=1, duration=1, warmup=0, threads=1, connections=1,
                )

                def compose(*command, **kwargs):
                    if command[0] == "logs" and failure == "capture":
                        raise OSError("log capture failed")
                    return SimpleNamespace(stdout="service log", stderr="")

                with patch.object(bench, "parse_args", return_value=args), \
                     patch.object(bench, "ensure_payloads"), \
                     patch.object(bench, "wait_ready", side_effect=RuntimeError("not ready")), \
                     patch.object(bench, "compose", side_effect=compose) as commands, \
                     patch.object(Path, "write_text",
                                  side_effect=OSError("log storage full") if failure == "storage" else None):
                    with self.assertRaises(OSError):
                        bench.main()
                    downs = [call for call in commands.call_args_list if call.args[0] == "down"]
                    self.assertEqual(len(downs), 2, "teardown must run after the initial reset")

    def test_receive_profile_records_actual_values_and_explicit_failures(self):
        with patch.object(bench, "compose") as command:
            command.return_value = SimpleNamespace(returncode=0, stdout="4096\t131072\t524288\n", stderr="")
            self.assertEqual(bench.client_network_settings(), {"tcp_rmem_bytes": [4096, 131072, 524288]})
            for result in [
                SimpleNamespace(returncode=1, stdout="", stderr="not available"),
                SimpleNamespace(returncode=0, stdout="4096 131072 bad", stderr=""),
                SimpleNamespace(returncode=0, stdout="4096 131072", stderr=""),
                SimpleNamespace(returncode=0, stdout="4096 131072 1024", stderr=""),
            ]:
                command.return_value = result
                self.assertIsNone(bench.client_network_settings()["tcp_rmem_bytes"])
            command.assert_called_with("exec", "-T", "bench", "cat", "/proc/sys/net/ipv4/tcp_rmem",
                                       check=False, capture_output=True)

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
