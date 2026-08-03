#!/bin/sh
set -eu

progress=0
target=${GOAL_FAKE_STEPS:-20}
if test -f progress; then
  progress=$(cat progress)
fi
case "$progress" in
  ''|*[!0-9]*)
    printf 'fake sensor: progress must be a non-negative integer\n' >&2
    exit 1
    ;;
esac
case "$target" in
  ''|*[!0-9]*)
    printf 'fake sensor: GOAL_FAKE_STEPS must be a non-negative integer\n' >&2
    exit 1
    ;;
esac

sleep "${GOAL_FAKE_DELAY_SECONDS:-1}"
if test "$progress" -ge "$target"; then
  completed=true
else
  completed=false
fi
printf '{"kind":"fake_sensor","progress":%s,"target":%s,"completed":%s}\n' \
  "$progress" "$target" "$completed" >&2
printf '{"completed":%s,"progress":%s,"target":%s}\n' \
  "$completed" "$progress" "$target"
