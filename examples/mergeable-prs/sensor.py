#!/usr/bin/env python3
"""Read-only, paginated GitHub observation for authored open pull requests."""

import contextlib
import fcntl
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
HISTORY_ACTIVITY_LIMIT = 10
HISTORY_ACTIVITY_WINDOW_SECONDS = 60 * 60
HEAD_OID_PATTERN = re.compile(r"(?<![0-9a-fA-F])[0-9a-fA-F]{40}(?![0-9a-fA-F])")
RATE_LIMIT_RETRY_MARKER = "goal-retry-after-seconds="
API_LOCK_PATH = Path.home() / ".cache" / "goal" / "github-api.lock"
API_LOCK_TIMEOUT_SECONDS = 30
API_CALL_TIMEOUT_SECONDS = 120
RATE_LIMIT_CALL_TIMEOUT_SECONDS = 15
LOCK_POLL_SECONDS = 0.1


@contextlib.contextmanager
def api_lock():
    API_LOCK_PATH.parent.mkdir(parents=True, exist_ok=True)
    with API_LOCK_PATH.open("a+") as lock:
        deadline = time.monotonic() + API_LOCK_TIMEOUT_SECONDS
        while True:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise RuntimeError(
                        f"timed out waiting {API_LOCK_TIMEOUT_SECONDS}s for GitHub API lock"
                    )
                time.sleep(LOCK_POLL_SECONDS)
        try:
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


def _emit_rate_limit_retry_hint(message):
    """Tell the controller when GitHub's primary GraphQL limit resets."""
    if "rate limit" not in message.lower():
        return
    try:
        process = subprocess.run(
            ["gh", "api", "rate_limit", "--jq", ".resources.graphql.reset"],
            text=True,
            capture_output=True,
            timeout=RATE_LIMIT_CALL_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        return
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
    try:
        with api_lock():
            process = subprocess.run(
                command,
                text=True,
                capture_output=True,
                timeout=API_CALL_TIMEOUT_SECONDS,
            )
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(
            f"gh api graphql timed out after {API_CALL_TIMEOUT_SECONDS}s"
        ) from error
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


def _history_timestamp_utc(timestamp):
    if not isinstance(timestamp, (int, float)):
        return None
    try:
        return datetime.fromtimestamp(timestamp, timezone.utc).isoformat()
    except (OSError, OverflowError, ValueError):
        return None


def add_prior_worker_failures(
    pull_requests,
    events_path=None,
    limit_per_pr=2,
    activity_limit_per_pr=HISTORY_ACTIVITY_LIMIT,
    activity_since=None,
):
    """Attach bounded worker failures and recent dispatch activity to each PR."""
    if events_path is None:
        default_project_dir = Path(__file__).resolve().parent
        project_dir = Path(os.environ.get("GOAL_PROJECT_DIR", default_project_dir))
        events_path = project_dir / ".goal" / "events.jsonl"
    else:
        events_path = Path(events_path)

    failures_by_url = {pr.get("url"): {} for pr in pull_requests if pr.get("url")}
    activity_by_url = {url: [] for url in failures_by_url}
    pending_action = None
    if events_path.exists():
        with events_path.open(encoding="utf-8") as events:
            for line in events:
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue

                event_type = event.get("type")
                if event_type == "decision":
                    action = (event.get("details") or {}).get("action") or {}
                    pending_action = action if action.get("type") == "run_task" else None
                    continue
                if event_type not in {"worker_completed", "worker_failed"}:
                    continue
                if pending_action is None:
                    continue

                action = pending_action
                pending_action = None
                details = event.get("details") or {}
                completion = details.get("completion") or {}
                completion_type = completion.get("type")
                if event_type == "worker_failed":
                    completion_type = "failure"
                if completion_type not in {"done", "failure"}:
                    continue

                task = action.get("task") or ""
                head_match = HEAD_OID_PATTERN.search(task)
                head_oid = head_match.group(0).lower() if head_match else "unknown"
                recorded_at = event.get("timestamp")
                summary_or_reason = (
                    completion.get("summary")
                    if completion_type == "done"
                    else completion.get("reason") or details.get("error") or ""
                )
                for url, failures_by_head in failures_by_url.items():
                    if url not in task:
                        continue
                    activity = {
                        "recordedAt": recorded_at,
                        "recordedAtUtc": _history_timestamp_utc(recorded_at),
                        "headRefOid": None if head_oid == "unknown" else head_oid,
                        "assignedTask": _truncate_history_text(
                            task, HISTORY_TASK_MAX_CHARS
                        ),
                        "completionType": completion_type,
                    }
                    activity["summary" if completion_type == "done" else "reason"] = (
                        _truncate_history_text(
                            summary_or_reason, HISTORY_REASON_MAX_CHARS
                        )
                    )
                    activity_by_url[url].append(activity)
                    if completion_type == "failure":
                        failures_by_head[head_oid] = {
                            "recordedAt": recorded_at,
                            "headRefOid": activity["headRefOid"],
                            "assignedTask": activity["assignedTask"],
                            "reason": activity["reason"],
                        }

    for pr in pull_requests:
        url = pr.get("url")
        failures = list(failures_by_url.get(url, {}).values())
        failures.sort(key=lambda failure: failure.get("recordedAt") or 0)
        pr["priorWorkerFailures"] = failures[-limit_per_pr:]
        activity = activity_by_url.get(url, [])
        if activity_since is not None:
            activity = [
                item
                for item in activity
                if isinstance(item.get("recordedAt"), (int, float))
                and item["recordedAt"] >= activity_since
            ]
        activity.sort(key=lambda item: item.get("recordedAt") or 0)
        pr["recentWorkerActivity"] = activity[-activity_limit_per_pr:]


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

    add_prior_worker_failures(
        pull_requests,
        activity_since=observed_at.timestamp() - HISTORY_ACTIVITY_WINDOW_SECONDS,
    )

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
