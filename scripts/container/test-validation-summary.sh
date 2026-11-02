#!/usr/bin/env bash
# Tests validation-summary.py on made-up junit files: a clean run with its proofs
# passes; the allowed hazard, exactly, passes and is counted; the same ID on other
# accesses fails; any other message fails; a crash after the test passed is counted,
# a crash before it fails; a run whose proofs say it can't fail fails.
# Run: scripts/container/run.sh scripts/container/test-validation-summary.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; sed 's/^/     /' "$DIR/out"; bad=1; }

suite() { printf '<testsuites><testsuite name="s">%s</testsuite></testsuites>\n' "$1"; }
case_ok() { printf '<testcase name="%s"><system-out>%s</system-out></testcase>' "$1" "${2:-}"; }
case_bad() { printf '<testcase name="%s"><failure type="test abort" message="%s"/><system-out>%s</system-out></testcase>' "$1" "$2" "${3:-}"; }

BADCALL='VUID-VkBufferCreateInfo-size-00912(ERROR / SPEC): msgNum: 1 - Validation Error'
trapped=$(case_bad a_bad_vulkan_call_traps 'process aborted with signal 5 (SIGTRAP)' "$BADCALL")
logged=$(case_ok a_bad_vulkan_call_traps "$BADCALL")
layer=$(case_ok strands_gpu_thread_runs_under_the_layer)
clean() {
  suite "$trapped$layer" >"$DIR/proof-core.xml"
  suite "$logged$layer" >"$DIR/proof-sync.xml"
  for name in core-1 core-2 sync-1 sync-2; do suite "$(case_ok one)" >"$DIR/$name.xml"; done
}
run() { python3 -I "$HERE/validation-summary.py" "$DIR" >"$DIR/out" 2>&1; }
hazard() { # SUBMITTED PRIOR
  echo "SYNC-HAZARD-READ-AFTER-WRITE(ERROR / SPEC): msgNum: 2 - Validation Error: Submitted access info (submitted_usage: $1, command: vkCmdDraw). Access info (prior_usage: $2, write_barriers: 0)"
}

clean
run && ok "a clean run passes" || no "a clean run failed"
grep -q 'The layer reported nothing' "$DIR/out" && ok "it says the layer was silent" || no "silence not said"

clean
suite "$(case_ok one "$(hazard SYNC_VERTEX_ATTRIBUTE_INPUT_VERTEX_ATTRIBUTE_READ SYNC_IMAGE_LAYOUT_TRANSITION)")" >"$DIR/sync-1.xml"
run && ok "the allowed hazard passes" || no "the allowed hazard failed"
grep -q 'SYNC_IMAGE_LAYOUT_TRANSITION | 1 | allowed' "$DIR/out" && ok "and is counted" || no "not counted"

clean
suite "$(case_ok one "$(hazard SYNC_FRAGMENT_SHADER_SHADER_SAMPLED_READ SYNC_IMAGE_LAYOUT_TRANSITION)")" >"$DIR/sync-1.xml"
run && no "the same ID on other accesses passed" || ok "the same ID on other accesses fails"

clean
suite "$(case_ok one 'VUID-vkCmdDraw-None-02859(ERROR / SPEC): msgNum: 3 - Validation Error')" >"$DIR/sync-2.xml"
run && no "another message passed" || ok "another message fails"

clean
suite "$(case_bad one 'process aborted with signal 11 (SIGSEGV)' 'test result: ok. 1 passed; 0 failed')" >"$DIR/core-1.xml"
run && ok "a crash after the test passed is no failure" || no "a crash at exit failed"
grep -q '| core | 2 | 1 |' "$DIR/out" && ok "and is counted" || no "exit crash not counted"

clean
suite "$(case_bad one 'process aborted with signal 11 (SIGSEGV)' 'running 1 test')" >"$DIR/core-1.xml"
run && no "a crash mid-test passed" || ok "a crash before it passed fails"

clean
suite "$(case_bad one 'process aborted with signal 5 (SIGTRAP)' "$BADCALL")" >"$DIR/core-1.xml"
run && no "a trap passed" || ok "a trap fails"

clean
suite "$(case_ok a_bad_vulkan_call_traps "$BADCALL")$layer" >"$DIR/proof-core.xml"
run && no "a bad call that didn't trap passed" || ok "a bad call that didn't trap fails"
clean
suite "$(case_ok a_bad_vulkan_call_traps)$layer" >"$DIR/proof-sync.xml"
run && no "a bad call nothing read passed" || ok "a bad call nothing read fails"
clean
suite "$trapped$(case_bad strands_gpu_thread_runs_under_the_layer 'exit code 101')" >"$DIR/proof-core.xml"
run && no "no layer passed" || ok "the layer not loaded fails"

clean
find "$DIR" -name 'sync-*.xml' -delete
run && no "a pass with no results passed" || ok "a pass with no results fails"
exit "$bad"
