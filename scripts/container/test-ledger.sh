#!/usr/bin/env bash
# Tests ledger.sh on hand-written ledger files.
# Run: scripts/container/test-ledger.sh (no build needed)

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0

# expect NAME WANT: ledger.sh on the file on stdin prints WANT.
expect() {
  local name=$1 want=$2 got
  cat >"$DIR/ledger"
  got=$(env -u GITHUB_STEP_SUMMARY "$HERE/ledger.sh" "$DIR/ledger")
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    printf 'FAIL %s\n     got:  %s\n     want: %s\n' "$name" "$got" "$want"
    bad=1
  fi
}

expect "both builds, CPU-only first whatever the order" \
  ".text CPU-only 15,700,000 (Δ +42,329 vs 15,657,671; gate 15,925,248) · GPU 19,000,000 (Δ -41,095 vs 19,041,095; gate 19,398,656)" <<'X'
gpu GPU 19,000,000 (Δ -41,095 vs 19,041,095; gate 19,398,656)
cpu CPU-only 15,700,000 (Δ +42,329 vs 15,657,671; gate 15,925,248)
X

expect "the latest half of a build wins" \
  ".text CPU-only 2 (Δ +2 vs 0; gate 9) · GPU 3 (Δ +3 vs 0; gate 9)" <<'X'
cpu CPU-only 1 (Δ +1 vs 0; gate 9)
gpu GPU 3 (Δ +3 vs 0; gate 9)
cpu CPU-only 2 (Δ +2 vs 0; gate 9)
X

expect "a build that did not run" \
  ".text CPU-only 1 (Δ +1 vs 0; gate 9) · GPU not measured" <<'X'
cpu CPU-only 1 (Δ +1 vs 0; gate 9)
X

got=$(env -u GITHUB_STEP_SUMMARY "$HERE/ledger.sh" "$DIR/missing")
if [ "$got" = ".text CPU-only not measured · GPU not measured" ]; then
  echo "ok   no ledger file"
else
  echo "FAIL no ledger file: $got"; bad=1
fi

echo "cpu CPU-only 1 (Δ +1 vs 0; gate 9)" >"$DIR/ledger"
GITHUB_STEP_SUMMARY=$DIR/summary "$HERE/ledger.sh" "$DIR/ledger" >/dev/null
if grep -q '^\.text CPU-only 1 ' "$DIR/summary" 2>/dev/null; then
  echo "ok   the job summary gets the line"
else
  echo "FAIL the job summary gets the line"; bad=1
fi

[ "$bad" = 0 ] && echo "test-ledger: all passed"
exit "$bad"
