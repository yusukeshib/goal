#!/bin/sh
set -eu

sleep "${GOAL_FAKE_DELAY_SECONDS:-1}"
if grep -q 'Advance fake progress' "$GOAL_PROMPT_PATH"; then
  progress=0
  if test -f progress; then
    progress=$(cat progress)
  fi
  case "$progress" in
    ''|*[!0-9]*)
      printf 'fake worker: progress must be a non-negative integer\n' >&2
      exit 1
      ;;
  esac
  next=$((progress + 1))
  temporary_progress=progress.tmp
  printf '%s\n' "$next" > "$temporary_progress"
  mv "$temporary_progress" progress
  printf '{"kind":"fake_worker","advanced_from":%s,"advanced_to":%s}\n' \
    "$progress" "$next"
  result=$(printf \
    '{"type":"done","summary":"Advanced deterministic fake progress from %s to %s."}' \
    "$progress" "$next")
else
  result='{"type":"failure","reason":"The assigned task did not request one fake progress step."}'
fi
temporary="$GOAL_RESULT_PATH.tmp"
printf '%s' "$result" > "$temporary"
mv "$temporary" "$GOAL_RESULT_PATH"
