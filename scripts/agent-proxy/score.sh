#!/usr/bin/env bash
# score.sh OUT: scores a proxy session's config (OUT/config, made by
# prepare.sh) against docs/guide/target-checklist.md: the newcomer
# test's desk (headless sway, two outputs, STRAND_MOCK=acceptance) runs
# it, opens the calendar, and reads the inspector's dump
# (crates/strand/tests/m5/newcomer.rs::agent_proxy_score). Prints each
# item, the score and the minutes since OUT/started; exits non-zero
# unless every item passes. Advisory: it gates nothing.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$(cd "${1:?usage: score.sh OUT}" && pwd)
[ -d "$OUT/config" ] || { echo "score.sh: $OUT/config is missing" >&2; exit 2; }
if [ -f "$OUT/started" ]; then
  echo "wall clock: $(( ($(date -u +%s) - $(cat "$OUT/started")) / 60 )) min since prepare.sh"
fi
STRAND_PROXY_CONFIG="$OUT/config" "$ROOT/scripts/container/run.sh" \
  cargo test -q -p strand --test m5 agent_proxy_score -- --ignored --nocapture
