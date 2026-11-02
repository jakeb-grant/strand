#!/usr/bin/env bash
# Tests nightly-red.sh against a stand-in `gh`: the first red night opens the
# issue, the next comments on it, only the jobs that went red are named, every
# artifact's summary is quoted though they share a file name, and a failed call
# fails the script.
# Run: scripts/container/run.sh scripts/container/test-nightly-red.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; sed 's/^/     /' "$DIR/calls"; bad=1; }

mkdir -p "$DIR/bin" "$DIR/summaries/nightly-summary-stress" "$DIR/summaries/nightly-summary-validation"
cat >"$DIR/bin/gh" <<'STUB'
#!/usr/bin/env bash
echo "gh $*" >>"$DIR/calls"
[ -z "${GH_FAIL:-}" ] || exit 1
case "$1 $2" in
  "label create") ;;
  "issue list") cat "$DIR/open" ;;
  "issue comment" | "issue create")
    while [ "$#" -gt 0 ]; do [ "$1" = --body-file ] && cp "$2" "$DIR/body"; shift; done ;;
  *) echo "gh: unexpected: $*" >&2; exit 1 ;;
esac
STUB
chmod +x "$DIR/bin/gh"
export DIR PATH="$DIR/bin:$PATH" RUN_URL=https://example.invalid/run/1
run() { : >"$DIR/calls"; rm -f "$DIR/body"; "$HERE/nightly-red.sh" "$@" >/dev/null 2>&1; }

: >"$DIR/open"
run "stress: failure" "compositors: success"
grep -q 'issue create.*--label nightly-red' "$DIR/calls" && ok "the first red night opens the issue" || no "no issue opened"
grep -q 'stress: failure' "$DIR/body" && ok "it names the job that failed" || no "stress not named"
grep -q 'compositors' "$DIR/body" && no "a green job is named" || ok "it leaves out the green one"
grep -q 'example.invalid/run/1' "$DIR/body" && ok "it links the run" || no "no link"

echo 7 >"$DIR/open"
echo "| \`strand::demo x\` | 2/40 (5.0%) |" >"$DIR/summaries/nightly-summary-stress/summary.md"
echo "the layer said nothing new" >"$DIR/summaries/nightly-summary-validation/summary.md"
SUMMARIES=$DIR/summaries run "compositors: failure"
grep -q 'issue comment 7' "$DIR/calls" && ok "the next comments on the open one" || no "no comment"
grep -q 'issue create' "$DIR/calls" && no "a second issue" || ok "and opens no second"
grep -q '2/40 (5.0%)' "$DIR/body" && ok "a summary is quoted" || no "no summary"
grep -q 'the layer said' "$DIR/body" && ok "and the other job's, of the same name" || no "one summary lost"
run "image-publish: failure" "stress: skipped"
grep -q 'image-publish: failure' "$DIR/body" && ok "a publish that failed is named" || no "image-publish not named"
grep -q 'stress: skipped' "$DIR/body" && no "a skipped job is named" || ok "and what it skipped isn't"
run "lock-vm: cancelled"
grep -q 'lock-vm: cancelled' "$DIR/body" && ok "a job at its timeout is named" || no "a cancelled job not named"

GH_FAIL=1 run "stress: failure" && no "a failed call passed" || ok "a failed call fails it"
run && no "no argument passed" || ok "no job named is a usage error"
exit "$bad"
