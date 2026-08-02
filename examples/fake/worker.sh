#!/bin/sh
set -eu
if grep -q 'Record the selected color:' "$GOAL_PROMPT_PATH"; then
  answer=$(awk -F': ' '/Record the selected color:/{print $2; exit}' "$GOAL_PROMPT_PATH")
  printf '%s\n' "$answer" > completed
  result='{"type":"done","summary":"Recorded the selected color and verified the completion marker exists."}'
else
  result='{"type":"needs_input","question":"Which color should be recorded?","context":"The fake goal needs one arbitrary color before it can complete.","resume_hint":"Tell the next worker to record the answer in the completed file."}'
fi
temporary="$GOAL_RESULT_PATH.tmp"
printf '%s' "$result" > "$temporary"
mv "$temporary" "$GOAL_RESULT_PATH"
