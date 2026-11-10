#!/usr/bin/env bash
# Tests scripts/container/check-action-pins.sh on toy workflows, then holds
# this checkout's .github to it.
# Run: scripts/container/run.sh scripts/container/test-check-action-pins.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
CHECK=$HERE/check-action-pins.sh
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0

ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }
# expect WANT NAME [GREP]: the check on $DIR/gh exits WANT and, with GREP,
# prints it.
expect() {
  local want=$1 name=$2 pat=${3-} got=0
  "$CHECK" "$DIR/gh" >"$DIR/out" 2>&1 || got=$?
  if [ "$got" != "$want" ]; then
    no "$name: exit $got, wanted $want"
    sed 's/^/     /' "$DIR/out"
  elif [ -n "$pat" ] && ! grep -q -- "$pat" "$DIR/out"; then
    no "$name: no '$pat' in the output"
    sed 's/^/     /' "$DIR/out"
  else
    ok "$name"
  fi
}

SHA=0123456789abcdef0123456789abcdef01234567
# workflow STEP...: a workflow whose steps are these `uses:` refs, plus a
# local action and a first-party one.
workflow() {
  rm -rf "$DIR/gh"
  mkdir -p "$DIR/gh/workflows"
  {
    printf 'jobs:\n  j:\n    steps:\n      - uses: ./.github/actions/setup\n      - uses: actions/checkout@v7\n'
    printf '%s\n' "$@"
  } >"$DIR/gh/workflows/w.yml"
}

mkdir -p "$DIR/gh"
expect 2 "no files is an error"

workflow "      - uses: owner/act@$SHA # v1.2.3"
expect 0 "a SHA with its version comment passes" "all 3 uses"
workflow "      - name: x" "        uses: 'owner/act@$SHA'"
expect 0 "a quoted SHA under a named step passes"
workflow "      - uses: owner/act@v2"
expect 1 "a tag fails" "w.yml:6: 'owner/act@v2'"
workflow "      - uses: owner/act@master"
expect 1 "a branch fails" "is not pinned"
workflow "        uses: owner/act@${SHA:0:12}"
expect 1 "a short SHA fails"
workflow "      - uses: owner/act@${SHA^^}"
expect 1 "an upper-case SHA is not one"
workflow "      - uses: owner/act"
expect 1 "no ref fails"
workflow "      - uses: owner/act@$SHA" "      # - uses: owner/act@v1"
expect 0 "a comment is not a use"
workflow
mkdir -p "$DIR/gh/actions/a"
printf 'runs:\n  steps:\n    - uses: owner/act@v1\n' >"$DIR/gh/actions/a/action.yaml"
expect 1 "a composite action's step is held too" "action.yaml:3"

if "$CHECK" >"$DIR/out" 2>&1; then
  ok "this checkout's .github is pinned"
else
  no "this checkout's .github"
  sed 's/^/     /' "$DIR/out"
fi

[ "$bad" = 0 ] && echo "test-check-action-pins: all passed"
exit "$bad"
