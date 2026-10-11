#!/usr/bin/env bash
# Static checks of scripts/gpu-measure.sh, which is owner-run only and
# never run by an agent or CI (it starts a client on the live session):
# it parses, carries the header, and refuses before anything else
# unless STRAND_OWNER_GPU_MEASURE=1. Nothing here executes it; only its
# summary-header function runs alone, with no session and a fake PATH.
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

# The summary's header (the `header` function alone, never the script)
# runs to its end under the script's `set -euo pipefail` whatever
# vulkaninfo does: missing, failing, or printing nothing it matches.
fn=$(sed -n '/^header() {$/,/^}$/p' "$F")
if [ -z "$fn" ]; then
  no "no header() function to run alone"
else
  T=$(mktemp -d)
  mkdir -p "$T/bin" "$T/fail" "$T/empty"
  for c in bash date stat uname cat grep sed; do ln -s "$(command -v "$c")" "$T/bin/$c"; done
  printf '#!/bin/sh\nexit 1\n' >"$T/fail/vulkaninfo"
  printf '#!/bin/sh\necho nothing useful\n' >"$T/empty/vulkaninfo"
  chmod +x "$T/fail/vulkaninfo" "$T/empty/vulkaninfo"
  for case in "missing:$T/bin" "failing:$T/fail:$T/bin" "silent:$T/empty:$T/bin"; do
    name=${case%%:*}
    out=$(PATH=${case#*:} BIN=/bin/sh WAYLAND_DISPLAY=none \
      "$T/bin/bash" -c 'set -euo pipefail; eval "$1"; header | cat; echo REACHED' _ "$fn" 2>&1)
    case "$out" in
      *"vulkan: "*REACHED) ok "the header goes on with vulkaninfo $name" ;;
      *) no "the header stops the run with vulkaninfo $name: $out" ;;
    esac
  done
  rm -rf "$T"
fi

[ "$bad" = 0 ] && echo "test-gpu-measure: all passed"
exit "$bad"
