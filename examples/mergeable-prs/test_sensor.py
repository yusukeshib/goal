import json
import subprocess
import tempfile
import unittest
from contextlib import nullcontext, redirect_stderr
from io import StringIO
from pathlib import Path
from unittest.mock import Mock, patch

from sensor import (
    HISTORY_REASON_MAX_CHARS,
    RATE_LIMIT_RETRY_MARKER,
    _emit_rate_limit_retry_hint,
    add_local_checkout_candidates,
    api_lock,
    add_prior_worker_failures,
    graphql,
    remove_resolved_review_threads,
)


class SensorNormalizationTests(unittest.TestCase):
    def test_api_lock_wait_has_bounded_timeout(self):
        with tempfile.TemporaryDirectory() as directory, patch(
            "sensor.API_LOCK_PATH", Path(directory) / "api.lock"
        ), patch(
            "sensor.fcntl.flock", side_effect=BlockingIOError
        ), patch(
            "sensor.time.monotonic", side_effect=[0, 31]
        ), patch(
            "sensor.time.sleep"
        ), self.assertRaisesRegex(RuntimeError, "timed out waiting 30s"):
            with api_lock():
                self.fail("lock should not have been acquired")

    def test_graphql_api_call_has_bounded_timeout(self):
        process = Mock(
            returncode=0,
            stdout='{"data":{"ok":true}}',
            stderr="",
        )
        with patch("sensor.api_lock", return_value=nullcontext()), patch(
            "sensor.subprocess.run", return_value=process
        ) as run:
            self.assertEqual(graphql("query { ok }"), {"ok": True})

        self.assertEqual(run.call_args.kwargs["timeout"], 120)

    def test_graphql_api_timeout_is_reported(self):
        timeout = subprocess.TimeoutExpired(["gh", "api"], 120)
        with patch("sensor.api_lock", return_value=nullcontext()), patch(
            "sensor.subprocess.run", side_effect=timeout
        ), self.assertRaisesRegex(RuntimeError, "timed out after 120s"):
            graphql("query { ok }")

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
        self.assertEqual(
            [item["completionType"] for item in pull_requests[0]["recentWorkerActivity"]],
            ["failure", "failure", "failure"],
        )
        self.assertEqual(pull_requests[1]["priorWorkerFailures"], [])
        self.assertEqual(pull_requests[1]["recentWorkerActivity"], [])

    def test_worker_failed_is_failure_history_and_activity_is_bounded(self):
        url = "https://github.com/owner/repo/pull/1"
        heads = [character * 40 for character in "abc"]
        events = []
        completions = [
            (
                "worker_completed",
                {"completion": {"type": "done", "summary": "first pass"}},
            ),
            (
                "worker_failed",
                {
                    "error": "process timed out",
                    "completion": {
                        "type": "failure",
                        "reason": "invocation may have changed external state",
                    },
                },
            ),
            (
                "worker_completed",
                {"completion": {"type": "done", "summary": "third pass"}},
            ),
        ]
        for index, ((event_type, details), head) in enumerate(
            zip(completions, heads, strict=True), start=1
        ):
            events.extend(
                [
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
                        "type": event_type,
                        "timestamp": index,
                        "details": details,
                    },
                ]
            )

        with tempfile.TemporaryDirectory() as temporary_directory:
            events_path = Path(temporary_directory) / "events.jsonl"
            events_path.write_text(
                "\n".join(json.dumps(event) for event in events), encoding="utf-8"
            )
            pull_requests = [{"url": url}]
            add_prior_worker_failures(
                pull_requests,
                events_path=events_path,
                activity_limit_per_pr=2,
            )

        self.assertEqual(
            pull_requests[0]["priorWorkerFailures"],
            [
                {
                    "recordedAt": 2,
                    "headRefOid": heads[1],
                    "assignedTask": f"Fix {url} at observed head {heads[1]}",
                    "reason": "invocation may have changed external state",
                }
            ],
        )
        activity = pull_requests[0]["recentWorkerActivity"]
        self.assertEqual([item["completionType"] for item in activity], ["failure", "done"])
        self.assertEqual(activity[0]["recordedAtUtc"], "1970-01-01T00:00:02+00:00")
        self.assertEqual(activity[1]["summary"], "third pass")

    def test_recent_worker_activity_excludes_entries_before_rolling_hour(self):
        url = "https://github.com/owner/repo/pull/1"
        events = []
        for recorded_at in (99, 100, 200):
            events.extend(
                [
                    {
                        "type": "decision",
                        "details": {
                            "action": {
                                "type": "run_task",
                                "task": f"Fix {url} at observed head {'a' * 40}",
                            }
                        },
                    },
                    {
                        "type": "worker_completed",
                        "timestamp": recorded_at,
                        "details": {
                            "completion": {
                                "type": "done",
                                "summary": f"completion at {recorded_at}",
                            }
                        },
                    },
                ]
            )

        with tempfile.TemporaryDirectory() as temporary_directory:
            events_path = Path(temporary_directory) / "events.jsonl"
            events_path.write_text(
                "\n".join(json.dumps(event) for event in events), encoding="utf-8"
            )
            pull_requests = [{"url": url}]
            add_prior_worker_failures(
                pull_requests, events_path=events_path, activity_since=100
            )

        self.assertEqual(
            [item["recordedAt"] for item in pull_requests[0]["recentWorkerActivity"]],
            [100, 200],
        )

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
