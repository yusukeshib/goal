#!/bin/sh
set -eu
if grep -q '"completed": true' "$GOAL_PROMPT_PATH"; then
  result='{"type":"complete","summary":"The fresh observation reports completion."}'
elif grep -q 'Human answer:' "$GOAL_PROMPT_PATH"; then
  answer=$(awk -F': ' '/Human answer:/{print $2; exit}' "$GOAL_PROMPT_PATH")
  result=$(printf '{"type":"run_task","task":"Record the selected color: %s"}' "$answer")
else
  result='{"type":"run_task","task":"Ask the human which color to record, then stop safely."}'
fi
temporary="$GOAL_RESULT_PATH.tmp"
printf '%s' "$result" > "$temporary"
mv "$temporary" "$GOAL_RESULT_PATH"
