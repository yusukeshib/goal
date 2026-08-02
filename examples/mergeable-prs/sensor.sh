#!/bin/sh
# Read-only GitHub sensor. Requires an authenticated `gh` CLI.
set -eu

gh api graphql -f query='
query GoalAuthoredPullRequests {
  search(query: "is:pr is:open author:@me", type: ISSUE, first: 50) {
    nodes {
      ... on PullRequest {
        url
        title
        number
        isDraft
        mergeable
        mergeStateStatus
        reviewDecision
        repository { nameWithOwner }
        reviewThreads(first: 100) {
          nodes { isResolved }
        }
        commits(last: 1) {
          nodes {
            commit {
              statusCheckRollup {
                state
                contexts(first: 100) {
                  nodes {
                    __typename
                    ... on CheckRun { name conclusion status detailsUrl }
                    ... on StatusContext { context state targetUrl }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}'
