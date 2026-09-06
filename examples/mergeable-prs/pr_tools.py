#!/usr/bin/env python3
"""Deterministic mutation guards for the mergeable-PR worker."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any


class ToolError(RuntimeError):
    pass


def _run(
    argv: list[str],
    *,
    cwd: Path | None = None,
    timeout: int = 120,
) -> subprocess.CompletedProcess[str]:
    process = subprocess.run(
        argv,
        cwd=cwd,
        text=True,
        capture_output=True,
        timeout=timeout,
    )
    if process.returncode != 0:
        detail = (process.stderr or process.stdout or "").strip()
        raise ToolError(f"{' '.join(argv[:4])} exited {process.returncode}: {detail}")
    return process


def _git_executable() -> str:
    executable = os.environ.get("GOAL_REAL_GIT") or shutil.which("git")
    if not executable:
        raise ToolError("git executable is unavailable")
    return executable


def git(args: list[str], *, cwd: Path, timeout: int = 120) -> str:
    return _run([_git_executable(), *args], cwd=cwd, timeout=timeout).stdout.strip()


def gh_json(endpoint: str) -> Any:
    output = _run(["gh", "api", endpoint], timeout=120).stdout
    try:
        return json.loads(output)
    except json.JSONDecodeError as error:
        raise ToolError(f"gh returned invalid JSON: {error}") from error


def _require_oid(value: str, label: str) -> None:
    if not re.fullmatch(r"[0-9a-f]{40}", value):
        raise ToolError(f"{label} must be a 40-character lowercase hexadecimal OID")


def _repository_from_remote(url: str) -> str | None:
    patterns = (
        r"https://github\.com/([^/]+/[^/]+?)(?:\.git)?$",
        r"git@github\.com:([^/]+/[^/]+?)(?:\.git)?$",
        r"ssh://git@github\.com/([^/]+/[^/]+?)(?:\.git)?$",
    )
    for pattern in patterns:
        match = re.fullmatch(pattern, url)
        if match:
            return match.group(1)
    return None


def _single_remote_oid(output: str, ref: str) -> str:
    lines = [line.split() for line in output.splitlines() if line.strip()]
    matches = [fields[0] for fields in lines if len(fields) == 2 and fields[1] == ref]
    if len(matches) != 1 or not re.fullmatch(r"[0-9a-f]{40}", matches[0]):
        raise ToolError(f"expected exactly one remote OID for {ref}, got {matches!r}")
    return matches[0]


def verify_pr(
    pr: dict[str, Any],
    user: dict[str, Any],
    *,
    repo: str,
    number: int,
    head_ref: str,
    expected_head_oid: str,
    base_ref: str,
    expected_base_oid: str,
) -> str:
    head = pr.get("head") or {}
    base = pr.get("base") or {}
    head_repo = (head.get("repo") or {}).get("full_name")
    failures = []
    if pr.get("number") != number:
        failures.append(f"number={pr.get('number')!r}")
    if ((base.get("repo") or {}).get("full_name")) != repo:
        failures.append(f"repository={((base.get('repo') or {}).get('full_name'))!r}")
    if pr.get("state") != "open":
        failures.append(f"state={pr.get('state')!r}")
    if (pr.get("user") or {}).get("login") != user.get("login"):
        failures.append(
            f"author={(pr.get('user') or {}).get('login')!r} authenticated_user={user.get('login')!r}"
        )
    if head.get("ref") != head_ref:
        failures.append(f"headRefName={head.get('ref')!r}")
    if head.get("sha") != expected_head_oid:
        failures.append(f"headRefOid={head.get('sha')!r}")
    if base.get("ref") != base_ref:
        failures.append(f"baseRefName={base.get('ref')!r}")
    if base.get("sha") != expected_base_oid:
        failures.append(f"baseRefOid={base.get('sha')!r}")
    if not head_repo:
        failures.append("headRepository is missing")
    if failures:
        raise ToolError("PR identity verification failed: " + ", ".join(failures))
    return head_repo


def command_guarded_push(args: argparse.Namespace) -> None:
    _require_oid(args.expected_head_oid, "expected head OID")
    _require_oid(args.expected_base_oid, "expected base OID")
    work_root = Path(os.environ.get("GOAL_WORK_DIR", "")).resolve()
    checkout = Path(args.checkout).resolve()
    if not work_root.is_absolute() or checkout == work_root or work_root not in checkout.parents:
        raise ToolError("checkout must be a child of the absolute GOAL_WORK_DIR")
    if not (checkout / ".git").exists():
        raise ToolError(f"checkout is not a Git worktree: {checkout}")
    if git(["status", "--porcelain"], cwd=checkout):
        raise ToolError("checkout has uncommitted changes")
    if git(["ls-files", "-u"], cwd=checkout):
        raise ToolError("checkout has unresolved index entries")

    local_oid = git(["rev-parse", "HEAD"], cwd=checkout)
    _require_oid(local_oid, "local HEAD")
    git(["merge-base", "--is-ancestor", args.expected_head_oid, local_oid], cwd=checkout)

    # Fetch all mutable identity immediately before the fixed, non-force push.
    pr = gh_json(f"repos/{args.repo}/pulls/{args.pr}")
    user = gh_json("user")
    head_repo = verify_pr(
        pr,
        user,
        repo=args.repo,
        number=args.pr,
        head_ref=args.head_ref,
        expected_head_oid=args.expected_head_oid,
        base_ref=args.base_ref,
        expected_base_oid=args.expected_base_oid,
    )
    origin_url = git(["remote", "get-url", "origin"], cwd=checkout)
    if _repository_from_remote(origin_url) != head_repo:
        raise ToolError(
            f"origin repository {_repository_from_remote(origin_url)!r} does not match PR head repository {head_repo!r}"
        )

    head_remote_ref = f"refs/heads/{args.head_ref}"
    remote_head = _single_remote_oid(
        git(["ls-remote", "origin", head_remote_ref], cwd=checkout), head_remote_ref
    )
    if remote_head != args.expected_head_oid:
        raise ToolError(f"remote head changed to {remote_head}")
    base_remote_ref = f"refs/heads/{args.base_ref}"
    base_url = f"https://github.com/{args.repo}.git"
    remote_base = _single_remote_oid(
        git(["ls-remote", base_url, base_remote_ref], cwd=checkout), base_remote_ref
    )
    if remote_base != args.expected_base_oid:
        raise ToolError(f"remote base changed to {remote_base}")

    git(["push", "origin", f"HEAD:{head_remote_ref}"], cwd=checkout, timeout=300)
    pushed_oid = _single_remote_oid(
        git(["ls-remote", "origin", head_remote_ref], cwd=checkout), head_remote_ref
    )
    if pushed_oid != local_oid:
        raise ToolError(f"push verification failed: remote={pushed_oid} local={local_oid}")

    current = gh_json(f"repos/{args.repo}/pulls/{args.pr}")
    current_head = (current.get("head") or {}).get("sha")
    if current.get("state") != "open" or current_head != local_oid:
        raise ToolError(
            f"PR did not retain the pushed head: state={current.get('state')!r} head={current_head!r}"
        )
    print(
        json.dumps(
            {
                "url": current.get("html_url"),
                "headRefName": args.head_ref,
                "previousHeadRefOid": args.expected_head_oid,
                "headRefOid": local_oid,
                "baseRefName": args.base_ref,
                "baseRefOid": args.expected_base_oid,
            },
            separators=(",", ":"),
        )
    )


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser()
    commands = root.add_subparsers(dest="command", required=True)
    push = commands.add_parser("guarded-push")
    push.add_argument("--repo", required=True)
    push.add_argument("--pr", required=True, type=int)
    push.add_argument("--head-ref", required=True)
    push.add_argument("--expected-head-oid", required=True)
    push.add_argument("--base-ref", required=True)
    push.add_argument("--expected-base-oid", required=True)
    push.add_argument("--checkout", required=True)
    push.set_defaults(handler=command_guarded_push)
    return root


def main() -> None:
    args = parser().parse_args()
    try:
        args.handler(args)
    except (ToolError, subprocess.TimeoutExpired) as error:
        print(f"pr_tools: {error}", file=sys.stderr)
        raise SystemExit(2) from error


if __name__ == "__main__":
    main()
