#!/usr/bin/env bash
# The scenario gate and the effect matrix run N times in one job, for the
# cross-machine stability criterion (docs/eval.md, "Stability"): every run
# must pass on its own (each scenario and cell matching its approved
# reference, so a run whose recording differs from it fails as CHANGED),
# and every run's scenarios and matrix cells must leave the same trace
# digests as the first run's (the determinism check's digest, across runs
# rather than within one). GitHub's eval-matrix workflow runs it with its `repeat`
# input on a runner, so a recording that holds on the laptop's container
# but wavers on another machine shows up.
#
# Usage: scripts/eval/repeat.sh N [matrix flags...]
#   HZ=60,144 (default) and EFFECTS=a,b as scripts/eval/matrix.sh takes
#   them; OUT=DIR (default target/eval/repeat) gets <i>/gate and
#   <i>/matrix per run and summary.md.
# Exits non-zero when any run fails or any run's digests differ.

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT" || exit 2
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
n=${1:-}
case "$n" in
  '' | *[!0-9]* | 0)
    echo "usage: scripts/eval/repeat.sh N [matrix flags...] (N >= 1)" >&2
    exit 2
    ;;
esac
shift
out=${OUT:-target/eval/repeat}
rm -rf "$out"
mkdir -p "$out"
summary="$out/summary.md"
echo "## eval: the gate and the matrix, $n runs" >"$summary"
bad=0
for i in $(seq 1 "$n"); do
  echo "=== repeat: run $i of $n"
  if cargo run -q --locked --manifest-path eval/Cargo.toml --bin strand-eval -- \
    check --gate eval/scenarios/curated eval/scenarios/regressions eval/scenarios/docs --out "$out/$i/gate"; then
    echo "- run $i: gate passed" >>"$summary"
  else
    echo "- run $i: gate FAILED (see $out/$i/gate)" >>"$summary"
    bad=1
  fi
  if OUT="$out/$i/matrix" HZ=${HZ:-60,144} GITHUB_STEP_SUMMARY='' \
    bash scripts/eval/matrix.sh "$@"; then
    echo "- run $i: matrix passed" >>"$summary"
  else
    echo "- run $i: matrix FAILED (see $out/$i/matrix)" >>"$summary"
    bad=1
  fi
done
# Each scenario's and cell's trace digest, run by run: one that differs
# from the first run's (or is missing from one run) is a run that differs.
if ! python3 -I - "$out" "$n" >>"$summary" <<'PY'; then
import json, pathlib, sys

out, n = pathlib.Path(sys.argv[1]), int(sys.argv[2])
def digests(i):
    d = {}
    run = out / str(i)
    for t in sorted(run.glob("*/**/trace.json")):
        key = str(t.parent.relative_to(run))
        try:
            d[key] = json.loads(t.read_text()).get("digest")
        except (OSError, ValueError):
            d[key] = None
    return d
first = digests(1)
differ = []
for i in range(2, n + 1):
    di = digests(i)
    for sc in sorted(set(first) | set(di)):
        if first.get(sc) != di.get(sc):
            differ.append(f"- run {i}: {sc}'s trace digest differs from run 1's")
print("\n".join(differ) if differ else f"- every run's {len(first)} trace digests are run 1's")
sys.exit(1 if differ else 0)
PY
  bad=1
fi
cat "$summary"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  cat "$summary" >>"$GITHUB_STEP_SUMMARY"
fi
[ "$bad" = 0 ] && echo "=== repeat: all $n runs pass and agree" || echo "=== repeat: FAILED"
exit "$bad"
