#!/usr/bin/env bash
# Tests nextest-summary.sh on stand-in junit files: counts, a flaky test named
# in the log and the job summary, a failure named in the summary, and no file
# or no test at all fails.
# Run: scripts/container/run.sh scripts/container/test-nextest-summary.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
bad=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }

cat >"$tmp/junit.xml" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="3" failures="1" errors="0" uuid="x" timestamp="2026-11-01T00:00:00Z" time="1.0">
  <testsuite name="strand-surface::sway" tests="2" disabled="0" errors="0" failures="0">
    <testcase name="fine" classname="strand-surface::sway" timestamp="2026-11-01T00:00:00Z" time="0.1"/>
    <testcase name="shaky" classname="strand-surface::sway" timestamp="2026-11-01T00:00:00Z" time="0.2">
      <flakyFailure timestamp="2026-11-01T00:00:00Z" time="0.1" type="test failure with exit code 101">out</flakyFailure>
    </testcase>
  </testsuite>
  <testsuite name="strand-core" tests="1" disabled="0" errors="0" failures="1">
    <testcase name="broken" classname="strand-core" timestamp="2026-11-01T00:00:00Z" time="0.1">
      <failure type="test failure with exit code 101">out</failure>
    </testcase>
  </testsuite>
</testsuites>
XML
out=$(GITHUB_STEP_SUMMARY=$tmp/summary "$HERE/nextest-summary.sh" "$tmp/junit.xml")
[ "$?" = 0 ] && ok "a run with tests exits 0" || no "a run with tests"
case "$out" in *"3 tests, 1 failed, 1 flaky"*) ok "the counts" ;; *) no "the counts: $out" ;; esac
case "$out" in *"FLAKY (passed on its retry): strand-surface::sway shaky"*) ok "the flaky test in the log" ;; *) no "flaky in the log: $out" ;; esac
grep -q 'strand-surface::sway shaky' "$tmp/summary" && ok "the flaky test in the job summary" || no "flaky in the summary"
grep -q 'strand-core broken' "$tmp/summary" && ok "the failure in the job summary" || no "failure in the summary"
if grep -q 'strand-surface::sway fine' "$tmp/summary"; then no "a plain pass in the summary"; else ok "a plain pass isn't listed"; fi

"$HERE/nextest-summary.sh" "$tmp/missing.xml" >/dev/null 2>&1
[ "$?" = 1 ] && ok "no file fails" || no "no file"
echo '<testsuites name="nextest-run" tests="0"></testsuites>' >"$tmp/empty.xml"
"$HERE/nextest-summary.sh" "$tmp/empty.xml" >/dev/null 2>&1
[ "$?" = 1 ] && ok "no test fails" || no "no test"
exit "$bad"
