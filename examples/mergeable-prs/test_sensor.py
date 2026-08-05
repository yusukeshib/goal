import json
import tempfile
import unittest
from pathlib import Path

from sensor import (
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
        for index in range(3):
            events.extend(
                [
                    {
                        "type": "decision",
                        "details": {
                            "action": {
                                "type": "run_task",
                                "task": (
                                    "Fix https://github.com/owner/repo/pull/1 "
                                    f"at head-{index}"
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

        self.assertEqual(
            [failure["reason"] for failure in pull_requests[0]["priorWorkerFailures"]],
            ["external blocker 1", "external blocker 2"],
        )
        self.assertEqual(pull_requests[1]["priorWorkerFailures"], [])


if __name__ == "__main__":
    unittest.main()
