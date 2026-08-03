#!/bin/sh
set -eu
sleep "${GOAL_FAKE_DELAY_SECONDS:-2}"
if test -f completed; then
  printf '{"completed":true}\n'
else
  printf '{"completed":false}\n'
fi
