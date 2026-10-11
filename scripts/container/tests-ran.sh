#!/usr/bin/env bash
# tests-ran.sh CMD...: runs a `cargo test` command, passing its output and
# exit status through, and fails a run that passed without running a
# test: one whose libtest summaries ("test result: ...") report no test
# passed or failed in all. A filtered step that names a test in the wrong
# target (`--bin strand` after the strand lib/bin split ran 0 of 7 and
# passed; decisions.md m5-w0-harden) then fails instead of gating
# nothing. Used by CI's and ci.sh's single-test steps.

set -uo pipefail

[ "$#" -ge 1 ] || { echo "usage: $0 CMD..." >&2; exit 2; }

log=$(mktemp)
trap 'rm -f "$log"' EXIT
"$@" 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
[ "$status" = 0 ] || exit "$status"

ran=$(sed 's/\x1b\[[0-9;]*m//g' "$log" | awk '
  /^test result: / {
    if (match($0, /[0-9]+ passed/)) n += substr($0, RSTART, RLENGTH) + 0
    if (match($0, /[0-9]+ failed/)) n += substr($0, RSTART, RLENGTH) + 0
  }
  END { print n + 0 }')
if [ "$ran" = 0 ]; then
  echo "tests-ran.sh: no test ran (every test filtered out or ignored): $*" >&2
  exit 1
fi
