#!/usr/bin/env bash
# The effect matrix (docs/eval.md, "Matrix"): every effect of
# eval/matrix/effects.toml alone and every unordered pair, on
# eval/matrix/fixture.strand.in, at each rate of HZ. Fails when a cell
# fails a check eval/matrix/known.toml does not list for it, when a
# listed check passes (stale: remove the entry), or when a cell cannot
# run.
#
# Usage: scripts/eval/matrix.sh [strand-eval matrix flags...]
#   (flags win over these; run.sh passes no environment but STRAND_*)
#   HZ=60,144      the rates (default 60)
#   EFFECTS=a,b    only these effects (default all)
#   JOBS=N         cells run at once (default: the cores, at most 8)
#   OUT=DIR        the report (default target/eval/matrix-<stamp>)
# In the container: scripts/container/run.sh eval matrix-all [flags...]
# Output: $OUT/index.html (the grid), matrix.json, summary.md; each cell
# that did not pass has its own page under $OUT/<cell>@<hz>/.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
case "${1:-}" in -h | --help) sed -n '2,17p' "$0"; exit 0 ;; esac

out=${OUT:-target/eval/matrix-$(date -u +%Y%m%d-%H%M%S)}
prev=
for a in "$@"; do
  case "$prev" in --out | -o) out=$a ;; esac
  prev=$a
done
args=(--out "$out")
[[ " $* " == *" --hz "* ]] || args+=(--hz "${HZ:-60}")
[ -z "${EFFECTS:-}" ] || args+=(--effects "$EFFECTS")
[ -z "${JOBS:-}" ] || args+=(--jobs "$JOBS")
status=0
cargo run -q --locked --manifest-path eval/Cargo.toml --bin strand-eval -- \
  matrix "${args[@]}" "$@" || status=$?
if [ -n "${GITHUB_STEP_SUMMARY:-}" ] && [ -f "$out/summary.md" ]; then
  cat "$out/summary.md" >>"$GITHUB_STEP_SUMMARY"
fi
exit "$status"
