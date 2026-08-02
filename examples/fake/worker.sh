#!/bin/sh
set -eu
if grep -q 'Record blue' "$GOAL_PROMPT_PATH"; then
  printf '%s\n' blue > completed
  result='{"type":"done","summary":"Recorded blue and verified the completion marker exists."}'
else
  result='{"type":"failure","reason":"The assigned task did not specify the deterministic completion marker."}'
fi
temporary="$GOAL_RESULT_PATH.tmp"
printf '%s' "$result" > "$temporary"
mv "$temporary" "$GOAL_RESULT_PATH"
