#!/bin/sh
set -eu
if test -f completed; then
  printf '{"completed":true}\n'
else
  printf '{"completed":false}\n'
fi
