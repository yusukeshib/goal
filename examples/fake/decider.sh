#!/bin/sh
set -eu
if grep -q '"completed": true' "$GOAL_PROMPT_PATH"; then
  result='{"type":"complete","summary":"The fresh observation reports completion."}'
else
  result='{"type":"run_task","task":"Record blue in the completion marker."}'
fi
temporary="$GOAL_RESULT_PATH.tmp"
printf '%s' "$result" > "$temporary"
mv "$temporary" "$GOAL_RESULT_PATH"
