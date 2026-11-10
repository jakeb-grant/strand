#!/usr/bin/env bash
# Tests scripts/eval/check-mutants.sh on a toy repository.
# Run: scripts/container/run.sh bash scripts/eval/test-check-mutants.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
CHECK=$HERE/check-mutants.sh
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0

ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }
# expect WANT NAME CMD...: CMD exits WANT.
expect() {
  local want=$1 name=$2 got=0
  shift 2
  "$@" >"$DIR/out" 2>&1 || got=$?
  if [ "$got" = "$want" ]; then ok "$name"; else no "$name: exit $got, wanted $want"; sed 's/^/     /' "$DIR/out"; fi
}

R=$DIR/repo
mkdir -p "$R/eval/mutants" "$R/src"
git -C "$R" init -q
printf 'fn a() {\n    one();\n}\n' >"$R/src/a.rs"
printf 'two.patch  regressions/x.scn  redraw\n' >"$R/eval/mutants/mutants.txt"
git -C "$R" add -A
git -C "$R" -c user.name=t -c user.email=t@t commit -qm base
# The mutant: src/a.rs's `one` made `two`, cut against the tree as it is.
sed -i 's/one();/two();/' "$R/src/a.rs"
git -C "$R" diff >"$DIR/two.patch"
git -C "$R" checkout -q -- src/a.rs
mv "$DIR/two.patch" "$R/eval/mutants/two.patch"

expect 0 "a patch that applies passes" "$CHECK" "$R"
grep -q 'check-mutants: 1 patches apply' "$DIR/out" &&
  ok "and is counted" || no "and is counted: $(cat "$DIR/out")"

printf 'fn a() {\n    zero();\n}\n' >"$R/src/a.rs"
expect 1 "a patch whose lines moved fails" "$CHECK" "$R"
grep -q 'STALE eval/mutants/two.patch' "$DIR/out" &&
  ok "the failure names the patch" || no "the failure names the patch: $(cat "$DIR/out")"
git -C "$R" checkout -q -- src/a.rs
expect 0 "and passes once the tree is back" "$CHECK" "$R"

exit "$bad"
