#!/bin/sh
set -eu

cd "$(dirname "$0")"
rm -f completed progress progress.tmp
exec ../../target/debug/goal up ./goal.toml --foreground "$@"
