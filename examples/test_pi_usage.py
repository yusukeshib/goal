"""Focused stdlib tests for pi_usage; intentionally not run during implementation."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ADAPTER = Path(__file__).with_name("pi_usage.py")
spec = importlib.util.spec_from_file_location("pi_usage", ADAPTER)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
PiUsageReporter = module.PiUsageReporter


def event(usage=None, timestamp=1, role="assistant", **extra):
    message = {"role": role, "timestamp": timestamp, **extra}
    if usage is not None:
        message["usage"] = usage
    return json.dumps({"type": "message_end", "message": message})


class UsageTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "usage.json"
        self.reporter = PiUsageReporter(self.path)

    def snapshot(self):
        return json.loads(self.path.read_text())

    def metrics(self):
        return {item["name"]: item["value"] for item in self.snapshot()["metrics"]}

    def test_cumulative_and_duplicates(self):
        first = event({"input": 3, "output": 2, "cost": {"total": 0.25}})
        self.reporter.consume(first)
        self.reporter.consume(first)
        self.reporter.consume(event({"input": 3, "cost": {"total": 0.5}}, timestamp=2))
        for kind in ("message_update", "turn_end", "agent_end"):
            self.reporter.consume(first.replace("message_end", kind))
        self.assertEqual(self.metrics()["input_tokens"], 6)
        self.assertEqual(self.metrics()["usage_records"], 2)
        self.assertEqual(self.snapshot()["costs"][0]["amount"], 0.75)
        self.assertFalse(self.snapshot()["complete"])
        self.reporter.finish(complete=True)
        self.assertTrue(self.snapshot()["complete"])

    def test_nested_final_tool_usage(self):
        usage = {"input": 10, "cost": {"total": 1}}
        record = event(usage, role="toolResult", toolCallId="a",
                       details={"usage": usage, "results": [{"usage": usage}]},
                       content=[{"usage": usage}])
        self.reporter.consume(record)
        self.reporter.consume(record)
        self.reporter.consume(event(role="toolResult", toolCallId="ordinary",
                                    details={"usage": usage}, content=[]))
        self.assertEqual(self.metrics()["usage_records"], 1)
        self.assertNotIn("cost_missing_records", self.metrics())
        self.assertEqual(self.metrics()["cost_reported_records"], 1)
        self.assertEqual(self.metrics()["input_tokens"], 10)
        self.assertEqual(self.snapshot()["costs"][0]["amount"], 1)

    def test_same_timestamp_distinct_finals_and_canonical_duplicates(self):
        first = event({"input": 3, "cost": 1}, content=[{"text": "first"}])
        second = event({"input": 3, "cost": 1}, content=[{"text": "second"}])
        self.reporter.consume(first)
        self.reporter.consume(json.dumps(json.loads(first), sort_keys=True, indent=2))
        self.reporter.consume(second)
        self.reporter.consume(second)
        self.assertEqual(self.metrics()["usage_records"], 2)
        self.assertEqual(self.metrics()["input_tokens"], 6)
        self.assertEqual(self.snapshot()["costs"][0]["amount"], 2)

    def test_unknown_and_explicit_zero(self):
        self.reporter.consume(event({"input": 5}))
        self.assertEqual(self.snapshot()["costs"], [])
        self.reporter.consume(event({"cost": {"total": 0}}, timestamp=2))
        self.assertEqual(self.snapshot()["costs"], [{"currency": "USD", "amount": 0}])
        self.assertEqual(self.metrics()["cost_missing_records"], 1)

    def test_malformed_and_invalid_numbers(self):
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            for record in (b"\xff", "{", "[]", b"x" * (module.MAX_LINE_BYTES + 1)):
                self.reporter.consume(record)
            self.reporter.consume(event({"input": -1, "output": True, "cost": float("inf")}))
            self.reporter.consume(event({"input": float("nan"), "cost": -2}, timestamp=2))
        self.assertEqual(len(stderr.getvalue().splitlines()), 1)
        self.assertEqual(self.snapshot()["costs"], [])
        self.assertNotIn("input_tokens", self.metrics())
        self.assertNotIn("output_tokens", self.metrics())

    def test_atomic_private_replacement_and_failure(self):
        self.path.write_text("old")
        self.path.chmod(0o644)
        self.reporter.consume(event({"cost": 1}))
        self.assertEqual(stat.S_IMODE(self.path.stat().st_mode), 0o600)
        previous = self.path.read_bytes()
        with patch.object(module.os, "replace", side_effect=OSError("failure")):
            with contextlib.redirect_stderr(io.StringIO()):
                self.reporter.finish(True)
        self.assertEqual(self.path.read_bytes(), previous)
        self.assertEqual(list(self.path.parent.iterdir()), [self.path])

    def test_optional_default_path_and_legacy_fallback(self):
        with patch.dict(os.environ, {}, clear=True):
            disabled = PiUsageReporter()
            disabled.consume(event({"cost": 1}))
            disabled.finish()
            self.assertIsNone(disabled.path)
            self.assertFalse(self.path.exists())
            os.environ["GOAL_RESULT_PATH"] = str(self.path.parent / "result.json")
            self.assertEqual(PiUsageReporter().path, str(self.path))
            os.environ["GOAL_USAGE_PATH"] = str(self.path.parent / "other.json")
            self.assertEqual(PiUsageReporter().path, os.environ["GOAL_USAGE_PATH"])
            self.assertEqual(PiUsageReporter(self.path).path, self.path)

    def test_cli_exact_bytes_and_incomplete_eof(self):
        data = event({"input": 1, "cost": 0}).encode() + b"\r\n\xff malformed\nlast"
        env = {key: value for key, value in os.environ.items()
               if key not in ("GOAL_USAGE_PATH", "GOAL_RESULT_PATH")}
        for enabled in (False, True):
            if enabled:
                env["GOAL_RESULT_PATH"] = str(self.path.parent / "result.json")
            result = subprocess.run([sys.executable, str(ADAPTER)], input=data,
                                    capture_output=True, env=env, check=True)
            self.assertEqual(result.stdout, data)
            if not enabled:
                self.assertEqual(result.stderr, b"")
                self.assertFalse(self.path.exists())
            else:
                self.assertFalse(self.snapshot()["complete"])
                self.assertEqual(self.metrics()["input_tokens"], 1)


if __name__ == "__main__":
    unittest.main()
