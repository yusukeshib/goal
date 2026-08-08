import json
import tempfile
import unittest
from contextlib import redirect_stderr
from io import StringIO
from pathlib import Path
from unittest.mock import Mock, patch

from sensor import (
    HISTORY_REASON_MAX_CHARS,
    RATE_LIMIT_RETRY_MARKER,
    _emit_rate_limit_retry_hint,
    add_local_checkout_candidates,
    add_prior_worker_failures,
    remove_resolved_review_threads,
)


class SensorNormalizationTests(unittest.TestCase):
    def test_remove_resolved_review_threads_keeps_only_explicitly_unresolved(self):
        pull_request = {
            "reviewThreads": {
                "nodes": [
                    {"id": "unresolved", "isResolved": False},
                    {"id": "resolved", "isResolved": True},
                    {"id": "unknown"},
                ]
            }
        }

        remove_resolved_review_threads(pull_request)

        self.assertEqual(
            pull_request["reviewThreads"]["nodes"],
            [{"id": "unresolved", "isResolved": False}],
        )
        self.assertEqual(pull_request["unresolvedReviewThreadIds"], ["unresolved"])

    def test_remove_resolved_review_threads_handles_missing_connection(self):
        pull_request = {}

        remove_resolved_review_threads(pull_request)

        self.assertEqual(pull_request["unresolvedReviewThreadIds"], [])

    def test_add_local_checkout_candidates_returns_bounded_existing_paths(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            first_checkout = root / "first" / "repo"
            second_checkout = root / "second" / "repo"
            for checkout in (first_checkout, second_checkout):
                (checkout / ".git").mkdir(parents=True)

            pull_request = {"repository": {"nameWithOwner": "owner/repo"}}
            add_local_checkout_candidates(
                pull_request,
                checkout_roots=[root / "first", root / "second"],
            )

            self.assertEqual(
                pull_request["localCheckoutCandidates"],
                [str(first_checkout), str(second_checkout)],
            )

    def test_add_local_checkout_candidates_handles_missing_repository(self):
        pull_request = {}

        add_local_checkout_candidates(pull_request, checkout_roots=[])

        self.assertEqual(pull_request["localCheckoutCandidates"], [])

    def test_add_prior_worker_failures_keeps_bounded_matching_history(self):
        pull_requests = [
            {"url": "https://github.com/owner/repo/pull/1"},
            {"url": "https://github.com/owner/repo/pull/2"},
        ]
        events = []
        heads = ["1" * 40, "2" * 40, "2" * 40]
        for index, head in enumerate(heads):
            events.extend(
                [
                    {
                        "type": "decision",
                        "details": {
                            "action": {
                                "type": "run_task",
                                "task": (
                                    "Fix https://github.com/owner/repo/pull/1 "
                                    f"at observed head {head}"
                                ),
                            }
                        },
                    },
                    {
                        "type": "worker_completed",
                        "timestamp": index,
                        "details": {
                            "completion": {
                                "type": "failure",
                                "reason": f"external blocker {index}",
                            }
                        },
                    },
                ]
            )

        with tempfile.TemporaryDirectory() as temporary_directory:
            events_path = Path(temporary_directory) / "events.jsonl"
            events_path.write_text(
                "\n".join(json.dumps(event) for event in events) + "\n{partial",
                encoding="utf-8",
            )
            add_prior_worker_failures(
                pull_requests, events_path=events_path, limit_per_pr=2
            )

        failures = pull_requests[0]["priorWorkerFailures"]
        self.assertEqual(
            [failure["reason"] for failure in failures],
            ["external blocker 0", "external blocker 2"],
        )
        self.assertEqual([failure["headRefOid"] for failure in failures], heads[:2])
        self.assertEqual(pull_requests[1]["priorWorkerFailures"], [])

    def test_add_prior_worker_failures_truncates_large_reasons(self):
        url = "https://github.com/owner/repo/pull/1"
        head = "a" * 40
        events = [
            {
                "type": "decision",
                "details": {
                    "action": {
                        "type": "run_task",
                        "task": f"Fix {url} at observed head {head}",
                    }
                },
            },
            {
                "type": "worker_completed",
                "timestamp": 1,
                "details": {
                    "completion": {
                        "type": "failure",
                        "reason": "x" * (HISTORY_REASON_MAX_CHARS + 100),
                    }
                },
            },
        ]
        with tempfile.TemporaryDirectory() as temporary_directory:
            events_path = Path(temporary_directory) / "events.jsonl"
            events_path.write_text(
                "\n".join(json.dumps(event) for event in events), encoding="utf-8"
            )
            pull_requests = [{"url": url}]
            add_prior_worker_failures(pull_requests, events_path=events_path)

        reason = pull_requests[0]["priorWorkerFailures"][0]["reason"]
        self.assertEqual(len(reason), HISTORY_REASON_MAX_CHARS)
        self.assertTrue(reason.endswith("…"))

    @patch("sensor.time.time", return_value=1_000)
    @patch("sensor.subprocess.run")
    def test_rate_limit_failure_emits_reset_hint(self, run, _time):
        run.return_value = Mock(returncode=0, stdout="1120\n", stderr="")
        stderr = StringIO()

        with redirect_stderr(stderr):
            _emit_rate_limit_retry_hint("API rate limit already exceeded")

        self.assertEqual(stderr.getvalue(), f"{RATE_LIMIT_RETRY_MARKER}125\n")

    @patch("sensor.time.time", return_value=1_000)
    @patch("sensor.subprocess.run")
    def test_past_rate_limit_reset_requests_minimum_delay(self, run, _time):
        run.return_value = Mock(returncode=0, stdout="900\n", stderr="")
        stderr = StringIO()

        with redirect_stderr(stderr):
            _emit_rate_limit_retry_hint("rate limit exceeded")

        self.assertEqual(stderr.getvalue(), f"{RATE_LIMIT_RETRY_MARKER}1\n")

    @patch("sensor.subprocess.run")
    def test_missing_rate_limit_reset_falls_back_without_hint(self, run):
        run.return_value = Mock(returncode=0, stdout="null\n", stderr="")
        stderr = StringIO()

        with redirect_stderr(stderr):
            _emit_rate_limit_retry_hint("rate limit exceeded")

        self.assertEqual(stderr.getvalue(), "")


if __name__ == "__main__":
    unittest.main()
