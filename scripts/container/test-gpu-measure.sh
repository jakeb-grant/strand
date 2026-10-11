#!/usr/bin/env bash
# Static checks of scripts/gpu-measure.sh, which is owner-run only and
# never run by an agent or CI (it starts a client on the live session):
# it parses, carries the header, and refuses before anything else
# unless STRAND_OWNER_GPU_MEASURE=1. Nothing here executes it.
# Run: scripts/container/test-gpu-measure.sh

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
F=$ROOT/scripts/gpu-measure.sh
bad=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }

bash -n "$F" && ok "it parses" || no "it parses"
[ "$(sed -n 2p "$F")" = "# owner-run only; never run by an agent." ] &&
  ok "its header says owner-run only" || no "its header says owner-run only"
# The guard is the first command after `set`: no other command before it.
first=$(grep -v '^#' "$F" | grep -v '^$' | sed -n '1,3p')
case "$first" in
  "set -euo pipefail"$'\n''if [ "${STRAND_OWNER_GPU_MEASURE:-}" != 1 ]; then'*) ok "it refuses first without STRAND_OWNER_GPU_MEASURE=1" ;;
  *) no "the owner guard is not the first thing it does: $first" ;;
esac
if grep -q -e '/dev/dri' -e 'docker' "$F"; then
  no "it names /dev/dri or docker (it only runs strand in the owner's session)"
else
  ok "it opens no device and starts no container"
fi

[ "$bad" = 0 ] && echo "test-gpu-measure: all passed"
exit "$bad"
