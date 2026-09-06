#!/bin/sh
set -eu

cd "$(dirname "$0")"
rm -f completed progress progress.tmp
# Keep this disposable demo out of the user's registered goals.
GOAL_STATE_DIR=$(mktemp -d "${TMPDIR:-/tmp}/goal-fake.XXXXXX")
export GOAL_STATE_DIR
trap 'rm -rf "$GOAL_STATE_DIR"' EXIT
../../target/debug/goal add ./goal.toml --id fake
../../target/debug/goal up fake --foreground "$@"
