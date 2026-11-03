#!/usr/bin/env bash
# Tests stress-summary.py on made-up junit files: a test failing once by its own
# panic is named with its rate and doesn't fail the run, twice does, and so does
# a crash or a timeout even once; a gate miss alone is listed apart and doesn't, a crash
# carrying no gate marker isn't a miss, a block that didn't run fails, and so does
# no test at all.
# Run: scripts/container/run.sh scripts/container/test-stress-summary.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; sed 's/^/     /' "$DIR/out"; bad=1; }

blocks() {
  rm -f "$DIR"/block-*.xml
  printf 'block\ttier\tjobs\tspinners\tpartitions\tseconds\tstatus\n1\trecord\t8\t4\t2\t60\t%s\n' "$1" >"$DIR/blocks.tsv"
}
# junit SUITE-BODY: the file of one partition of block 1.
junit() {
  printf '<testsuites>%s</testsuites>\n' "$1" >"$DIR/block-1-1.xml"
}
fail() { printf '<testcase name="%s"><failure message="m">%s</failure></testcase>' "$1" "$2"; }
pass() { printf '<testcase name="%s"/>' "$1"; }
run() { python3 -I "$HERE/stress-summary.py" "$DIR" >"$DIR/out" 2>&1; }

blocks 0
junit "<testsuite name=\"s::a@stress-0\">$(pass one)$(pass two)</testsuite><testsuite name=\"s::a@stress-1\">$(pass one)$(pass two)</testsuite>"
run && ok "a clean run passes" || no "a clean run failed"
grep -q '4 test runs of 2 tests in 1 blocks' "$DIR/out" && ok "it counts the runs" || no "counts"

panic="thread &apos;t&apos; (1) panicked at a.rs:1:1:"
junit "<testsuite name=\"s::a@stress-0\">$(fail one "$panic
never: the thing")$(pass two)</testsuite><testsuite name=\"s::a@stress-1\">$(pass one)$(pass two)</testsuite>"
run && ok "one panic in the night passes" || no "one panic failed the run"
grep -q '`s::a one` | 1/2 (50.0%) | -j 8 beside 4: 1' "$DIR/out" && ok "it names the test, its rate and the load" || no "no rate"
GITHUB_ACTIONS=true run
grep -q '^::warning title=stress: one::' "$DIR/out" && ok "and warns of it" || no "no warning"

junit "<testsuite name=\"s::a@stress-0\">$(fail one "$panic
never: the thing")$(fail two "$panic
never: the other")</testsuite><testsuite name=\"s::a@stress-1\">$(pass one)$(pass two)</testsuite>"
run && ok "two tests failing once each pass" || no "two single failures failed the run"

junit "<testsuite name=\"s::a@stress-0\">$(fail one "$panic
never: the thing")</testsuite><testsuite name=\"s::a@stress-1\">$(fail one "$panic
never: the thing")</testsuite>"
run && no "a test failing twice passed" || ok "a test failing twice fails the run"
GITHUB_ACTIONS=true run
grep -q '^::error title=stress: one::' "$DIR/out" && ok "as an error" || no "no error"

junit "<testsuite name=\"s::a@stress-0\">$(fail one "$panic
never: the thing
test timed out after 120s")</testsuite>"
run && no "a timeout passed" || ok "a timeout fails the run even once"

junit "<testsuite name=\"s::a@stress-0\">$(fail one "$panic
timing gate missed: 9 ms over 5 ms")</testsuite>"
run && ok "a gate miss alone passes" || no "a gate miss failed"
grep -q 'Gates missed' "$DIR/out" && grep -q '`s::a one` | 1/1' "$DIR/out" && ok "and is listed apart" || no "gate not listed"

junit "<testsuite name=\"s::a@stress-0\">$(fail one "$panic
timing gate missed: 9 ms
$panic
never: the thing")</testsuite>"
run
grep -q '1 tests failed' "$DIR/out" && ok "a panic beside a gate miss is a failure, not a miss" || no "a panic beside a gate miss was a miss"
junit "<testsuite name=\"s::a@stress-0\">$(fail one "(test aborted with signal 5: SIGTRAP)")</testsuite>"
run && no "a crash passed" || ok "a crash is no gate miss"

blocks 4
junit "<testsuite name=\"s::a@stress-0\">$(pass one)</testsuite>"
run && no "a block that didn't run passed" || ok "a block that didn't run fails"
grep -q 'nextest exited 4' "$DIR/out" && ok "and is named" || no "block not named"

blocks 0
run && no "no junit file passed" || ok "no test run at all fails"

: >"$DIR/step-summary"
junit "<testsuite name=\"s::a@stress-0\">$(pass one)</testsuite>"
GITHUB_STEP_SUMMARY=$DIR/step-summary run
grep -q '### Stress run' "$DIR/step-summary" && ok "the job summary gets it" || no "no job summary"
exit "$bad"
