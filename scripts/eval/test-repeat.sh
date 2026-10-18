#!/usr/bin/env bash
# Tests scripts/eval/repeat.sh with a stand-in `cargo` (no build): runs
# that pass and leave the same digests pass; a run whose scenario digest
# or matrix cell digest differs, a gate that fails and a matrix that
# fails each fail it; a bad
# count is refused.
# Run: scripts/container/run.sh bash scripts/eval/test-repeat.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0

ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }

# The stand-in: `check --gate ... --out D` writes D/<sc>/trace.json (the
# run is counted in $DIR/count), scenario two's digest differing in the
# run DIFFER names; `matrix --out D` writes one cell's D/c@60/c/trace.json,
# differing in the run CELL_DIFFERS names. Each fails in the run
# GATE_FAILS or MATRIX_FAILS names.
mkdir -p "$DIR/bin"
cat >"$DIR/bin/cargo" <<'SH'
#!/usr/bin/env bash
out=
prev=
for a in "$@"; do
  [ "$prev" = --out ] && out=$a
  prev=$a
done
case " $* " in
  *" check "*)
    n=$(($(cat "$STUB/count" 2>/dev/null || echo 0) + 1))
    echo "$n" >"$STUB/count"
    for sc in one two; do
      mkdir -p "$out/$sc"
      d=same
      [ "$sc" = two ] && [ "${DIFFER:-}" = "$n" ] && d=other
      printf '{"digest": "%s"}\n' "$d" >"$out/$sc/trace.json"
    done
    [ "${GATE_FAILS:-}" = "$n" ] && exit 1
    exit 0
    ;;
  *" matrix "*)
    n=$(cat "$STUB/count")
    mkdir -p "$out/c@60/c"
    d=same
    [ "${CELL_DIFFERS:-}" = "$n" ] && d=other
    printf '{"digest": "%s"}\n' "$d" >"$out/c@60/c/trace.json"
    [ "${MATRIX_FAILS:-}" = "$n" ] && exit 1
    exit 0
    ;;
esac
exit 3
SH
chmod +x "$DIR/bin/cargo"

run() {
  rm -f "$DIR/count"
  env PATH="$DIR/bin:$PATH" STUB="$DIR" OUT="$DIR/out" GITHUB_STEP_SUMMARY= "$@" \
    bash "$HERE/repeat.sh" 3 >"$DIR/log" 2>&1
}

if run; then ok "three runs that agree pass"; else no "three runs that agree pass"; cat "$DIR/log"; fi
grep -q "every run's 3 trace digests are run 1's" "$DIR/out/summary.md" &&
  ok "the summary says they agree" || no "the summary says they agree"
if run DIFFER=2; then no "a digest that differs fails"; else ok "a digest that differs fails"; fi
grep -q "run 2: gate/two's trace digest differs" "$DIR/out/summary.md" &&
  ok "the summary names the run and scenario" || no "the summary names the run and scenario"
if run CELL_DIFFERS=3; then no "a cell digest that differs fails"; else ok "a cell digest that differs fails"; fi
grep -q "run 3: matrix/c@60/c's trace digest differs" "$DIR/out/summary.md" &&
  ok "the summary names the cell" || no "the summary names the cell"
if run GATE_FAILS=3; then no "a failing gate fails"; else ok "a failing gate fails"; fi
if run MATRIX_FAILS=1; then no "a failing matrix fails"; else ok "a failing matrix fails"; fi
env OUT="$DIR/out" bash "$HERE/repeat.sh" 0 >/dev/null 2>&1
[ $? = 2 ] && ok "a count of 0 is refused" || no "a count of 0 is refused"
env OUT="$DIR/out" bash "$HERE/repeat.sh" x >/dev/null 2>&1
[ $? = 2 ] && ok "a count that is not a number is refused" || no "a count that is not a number is refused"

[ "$bad" = 0 ] && echo "test-repeat: all pass" || echo "test-repeat: FAILED"
exit "$bad"
