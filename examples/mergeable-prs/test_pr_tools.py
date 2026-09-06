import argparse
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

import pr_tools


class GuardedPushTests(unittest.TestCase):
    def args(self, checkout):
        return argparse.Namespace(
            repo="owner/repo",
            pr=42,
            head_ref="feature",
            expected_head_oid="a" * 40,
            base_ref="main",
            expected_base_oid="b" * 40,
            checkout=str(checkout),
        )

    def pull_request(self, *, state="open", head_oid=None):
        return {
            "number": 42,
            "html_url": "https://github.com/owner/repo/pull/42",
            "state": state,
            "user": {"login": "author"},
            "head": {
                "ref": "feature",
                "sha": head_oid or "a" * 40,
                "repo": {"full_name": "owner/repo"},
            },
            "base": {
                "ref": "main",
                "sha": "b" * 40,
                "repo": {"full_name": "owner/repo"},
            },
        }

    def test_closed_pr_blocks_before_push(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checkout = root / "checkout"
            (checkout / ".git").mkdir(parents=True)
            git_outputs = ["", "", "c" * 40, ""]
            with patch.dict(os.environ, {"GOAL_WORK_DIR": str(root)}), patch.object(
                pr_tools, "git", side_effect=git_outputs
            ) as git, patch.object(
                pr_tools, "gh_json", return_value=self.pull_request(state="closed")
            ), self.assertRaisesRegex(pr_tools.ToolError, "state='closed'"):
                pr_tools.command_guarded_push(self.args(checkout))

        self.assertNotIn("push", [invocation.args[0][0] for invocation in git.call_args_list])

    def test_verified_identity_uses_fixed_non_force_push_and_rechecks_pr(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checkout = root / "checkout"
            (checkout / ".git").mkdir(parents=True)
            local_oid = "c" * 40
            initial = self.pull_request()
            current = self.pull_request(head_oid=local_oid)
            git_outputs = [
                "",  # status
                "",  # unmerged index
                local_oid,
                "",  # expected head is an ancestor
                "https://github.com/owner/repo.git",
                f"{'a' * 40}\trefs/heads/feature",
                f"{'b' * 40}\trefs/heads/main",
                "",  # push
                f"{local_oid}\trefs/heads/feature",
            ]
            with patch.dict(os.environ, {"GOAL_WORK_DIR": str(root)}), patch.object(
                pr_tools, "git", side_effect=git_outputs
            ) as git, patch.object(
                pr_tools,
                "gh_json",
                side_effect=[initial, {"login": "author"}, current],
            ), patch("builtins.print"):
                pr_tools.command_guarded_push(self.args(checkout))

        self.assertIn(
            call(["push", "origin", "HEAD:refs/heads/feature"], cwd=checkout, timeout=300),
            git.call_args_list,
        )
        push_args = next(
            invocation.args[0]
            for invocation in git.call_args_list
            if invocation.args[0][0] == "push"
        )
        self.assertFalse(any(argument in {"--force", "-f"} for argument in push_args))

    def test_checkout_must_be_inside_worker_work_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checkout = root.parent / "outside"
            with patch.dict(os.environ, {"GOAL_WORK_DIR": str(root)}), self.assertRaisesRegex(
                pr_tools.ToolError, "child of"
            ):
                pr_tools.command_guarded_push(self.args(checkout))


if __name__ == "__main__":
    unittest.main()
