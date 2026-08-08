#!/usr/bin/env python3
"""Read-only, paginated GitHub observation for authored open pull requests."""

import json
import os
import re
import subprocess
import sys
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

SEARCH_QUERY = r"""
query($cursor: String, $searchQuery: String!) {
  search(query: $searchQuery, type: ISSUE, first: 100, after: $cursor) {
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        id url title number isDraft headRefName headRefOid mergeable mergeStateStatus reviewDecision
        repository { nameWithOwner }
        latestReviews(first: 100) {
          nodes { id state body url submittedAt commit { oid } author { login } }
        }
        reviewThreads(first: 100) {
          pageInfo { hasNextPage endCursor }
          nodes {
            id isResolved isOutdated
            comments(last: 1) { nodes { body url createdAt author { login } } }
          }
        }
        commits(last: 1) {
          nodes { commit { statusCheckRollup {
            state
            contexts(first: 100) {
              pageInfo { hasNextPage endCursor }
              nodes {
                __typename
                ... on CheckRun { name conclusion status detailsUrl }
                ... on StatusContext { context state targetUrl }
              }
            }
          } } }
        }
      }
    }
  }
}
"""

THREADS_QUERY = r"""
query($id: ID!, $cursor: String) {
  node(id: $id) { ... on PullRequest {
    reviewThreads(first: 100, after: $cursor) {
      pageInfo { hasNextPage endCursor }
      nodes {
        id isResolved isOutdated
        comments(last: 1) { nodes { body url createdAt author { login } } }
      }
    }
  } }
}
"""

CHECKS_QUERY = r"""
query($id: ID!, $cursor: String) {
  node(id: $id) { ... on PullRequest {
    commits(last: 1) { nodes { commit { statusCheckRollup {
      contexts(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          __typename
          ... on CheckRun { name conclusion status detailsUrl }
          ... on StatusContext { context state targetUrl }
        }
      }
    } } } }
  } }
}
"""

HISTORY_TASK_MAX_CHARS = 2_000
HISTORY_REASON_MAX_CHARS = 4_000
HEAD_OID_PATTERN = re.compile(r"(?<![0-9a-fA-F])[0-9a-fA-F]{40}(?![0-9a-fA-F])")
RATE_LIMIT_RETRY_MARKER = "goal-retry-after-seconds="


def _emit_rate_limit_retry_hint(message):
    """Tell the controller when GitHub's primary GraphQL limit resets."""
    if "rate limit" not in message.lower():
        return
    process = subprocess.run(
        ["gh", "api", "rate_limit", "--jq", ".resources.graphql.reset"],
        text=True,
        capture_output=True,
    )
    if process.returncode != 0:
        return
    try:
        reset_at = int(process.stdout.strip())
    except ValueError:
        return
    retry_after_seconds = max(1, reset_at - int(time.time()) + 5)
    print(f"{RATE_LIMIT_RETRY_MARKER}{retry_after_seconds}", file=sys.stderr)


def graphql(query, **variables):
    command = ["gh", "api", "graphql", "-f", f"query={query}"]
    for name, value in variables.items():
        if value is not None:
            command.extend(["-F", f"{name}={value}"])
    process = subprocess.run(command, text=True, capture_output=True)
    if process.stderr:
        sys.stderr.write(process.stderr)
    if process.returncode != 0:
        _emit_rate_limit_retry_hint(process.stderr)
        raise RuntimeError(f"gh api graphql exited {process.returncode}")
    payload = json.loads(process.stdout)
    if payload.get("errors"):
        errors = json.dumps(payload["errors"], ensure_ascii=False)
        _emit_rate_limit_retry_hint(errors)
        raise RuntimeError(errors)
    return payload["data"]


def paginate_connection(pr, connection_name, query, extractor):
    connection = pr.get(connection_name)
    if not connection:
        return
    page = connection.get("pageInfo") or {}
    while page.get("hasNextPage"):
        data = graphql(query, id=pr["id"], cursor=page.get("endCursor"))
        extra = extractor(data)
        connection.setdefault("nodes", []).extend(extra.get("nodes") or [])
        page = extra.get("pageInfo") or {}
    connection.pop("pageInfo", None)


def remove_resolved_review_threads(pr):
    """Keep only review threads that the sensor can prove are unresolved."""
    connection = pr.get("reviewThreads")
    if not connection:
        pr["unresolvedReviewThreadIds"] = []
        return
    connection["nodes"] = [
        thread
        for thread in connection.get("nodes") or []
        if thread.get("isResolved") is False
    ]
    pr["unresolvedReviewThreadIds"] = [
        thread["id"] for thread in connection["nodes"] if thread.get("id")
    ]


def add_local_checkout_candidates(pr, checkout_roots=None):
    """Expose bounded configured checkout candidates without scanning the filesystem."""
    if checkout_roots is None:
        configured_roots = os.environ.get("GOAL_CHECKOUT_ROOTS", "")
        checkout_roots = filter(None, configured_roots.split(os.pathsep))
    checkout_roots = [Path(root).expanduser() for root in checkout_roots]

    repository = (pr.get("repository") or {}).get("nameWithOwner") or ""
    repository_name = repository.rsplit("/", 1)[-1]
    if not repository_name:
        pr["localCheckoutCandidates"] = []
        return

    candidates = [root / repository_name for root in checkout_roots]
    pr["localCheckoutCandidates"] = [
        str(candidate) for candidate in candidates if (candidate / ".git").exists()
    ]


def _truncate_history_text(text, maximum):
    if len(text) <= maximum:
        return text
    return text[: maximum - 1].rstrip() + "…"


def add_prior_worker_failures(pull_requests, events_path=None, limit_per_pr=2):
    """Attach the latest bounded failure for each recently observed PR head."""
    if events_path is None:
        default_project_dir = Path(__file__).resolve().parent
        project_dir = Path(os.environ.get("GOAL_PROJECT_DIR", default_project_dir))
        events_path = project_dir / ".goal" / "events.jsonl"
    else:
        events_path = Path(events_path)

    failures_by_url = {pr.get("url"): {} for pr in pull_requests if pr.get("url")}
    pending_action = None
    if events_path.exists():
        with events_path.open(encoding="utf-8") as events:
            for line in events:
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue

                if event.get("type") == "decision":
                    action = (event.get("details") or {}).get("action") or {}
                    pending_action = action if action.get("type") == "run_task" else None
                    continue
                if event.get("type") != "worker_completed" or pending_action is None:
                    continue

                action = pending_action
                pending_action = None
                completion = (event.get("details") or {}).get("completion") or {}
                if completion.get("type") != "failure":
                    continue
                task = action.get("task") or ""
                for url, failures_by_head in failures_by_url.items():
                    if url not in task:
                        continue
                    head_match = HEAD_OID_PATTERN.search(task)
                    head_oid = head_match.group(0).lower() if head_match else "unknown"
                    failures_by_head[head_oid] = {
                        "recordedAt": event.get("timestamp"),
                        "headRefOid": None if head_oid == "unknown" else head_oid,
                        "assignedTask": _truncate_history_text(
                            task, HISTORY_TASK_MAX_CHARS
                        ),
                        "reason": _truncate_history_text(
                            completion.get("reason") or "", HISTORY_REASON_MAX_CHARS
                        ),
                    }

    for pr in pull_requests:
        failures = list(failures_by_url.get(pr.get("url"), {}).values())
        failures.sort(key=lambda failure: failure.get("recordedAt") or 0)
        pr["priorWorkerFailures"] = failures[-limit_per_pr:]


def main():
    pull_requests = []
    cursor = None
    observed_at = datetime.now(timezone.utc)
    updated_since = (observed_at - timedelta(days=3)).strftime("%Y-%m-%dT%H:%M:%SZ")
    search_query = f"is:pr is:open author:@me updated:>={updated_since}"
    while True:
        search = graphql(SEARCH_QUERY, cursor=cursor, searchQuery=search_query)["search"]
        pull_requests.extend(node for node in search.get("nodes", []) if node)
        page = search["pageInfo"]
        if not page["hasNextPage"]:
            break
        cursor = page["endCursor"]

    for pr in pull_requests:
        paginate_connection(
            pr,
            "reviewThreads",
            THREADS_QUERY,
            lambda data: data["node"]["reviewThreads"],
        )
        commits = (pr.get("commits") or {}).get("nodes") or []
        rollup = commits[-1]["commit"].get("statusCheckRollup") if commits else None
        if rollup and rollup.get("contexts"):
            context_holder = {"id": pr["id"], "contexts": rollup["contexts"]}
            paginate_connection(
                context_holder,
                "contexts",
                CHECKS_QUERY,
                lambda data: data["node"]["commits"]["nodes"][-1]["commit"]["statusCheckRollup"]["contexts"],
            )
        remove_resolved_review_threads(pr)
        add_local_checkout_candidates(pr)

    add_prior_worker_failures(pull_requests)

    observation = {
        "observed_at": observed_at.isoformat(),
        "scope": {
            "owner": "authenticated gh user",
            "query": search_query,
            "window": "updated within the previous 72 hours",
            "feedback_definition": "unresolved GitHub review threads and review states",
            "review_thread_filter": "reviewThreads.nodes contains only isResolved=false threads; unresolvedReviewThreadIds repeats the exact actionable IDs",
            "pagination": "all search, review-thread, and check-context pages",
        },
        "pull_requests": pull_requests,
    }
    json.dump(observation, sys.stdout, ensure_ascii=False, separators=(",", ":"))
    sys.stdout.write("\n")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"sensor failed: {error}", file=sys.stderr)
        raise SystemExit(1)
