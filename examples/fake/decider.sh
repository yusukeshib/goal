#!/bin/sh
set -eu

sleep "${GOAL_FAKE_DELAY_SECONDS:-1}"
if grep -q '"completed": true' "$GOAL_PROMPT_PATH"; then
  printf '{"kind":"fake_decider","decision":"complete"}\n'
  result='{"type":"complete","summary":"The configured fake progress target was reached."}'
else
  printf '{"kind":"fake_decider","decision":"run_task"}\n'
  result='{"type":"run_task","task":"Advance fake progress by exactly one step."}'
fi
temporary="$GOAL_RESULT_PATH.tmp"
printf '%s' "$result" > "$temporary"
mv "$temporary" "$GOAL_RESULT_PATH"
