#!/usr/bin/env bash
# Tests nightly-red.sh against a stand-in `gh`: the first red night opens the
# issue, the next comments on it, only the jobs that went red are named, a
# summary is quoted, and a failed call fails the script.
# Run: scripts/container/run.sh scripts/container/test-nightly-red.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; sed 's/^/     /' "$DIR/calls"; bad=1; }

mkdir "$DIR/bin" "$DIR/summaries"
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
echo "| \`strand::demo x\` | 2/40 (5.0%) |" >"$DIR/summaries/stress.md"
SUMMARIES=$DIR/summaries run "compositors: failure"
grep -q 'issue comment 7' "$DIR/calls" && ok "the next comments on the open one" || no "no comment"
grep -q 'issue create' "$DIR/calls" && no "a second issue" || ok "and opens no second"
grep -q '2/40 (5.0%)' "$DIR/body" && ok "a summary is quoted" || no "no summary"

GH_FAIL=1 run "stress: failure" && no "a failed call passed" || ok "a failed call fails it"
run && no "no argument passed" || ok "no job named is a usage error"
exit "$bad"
