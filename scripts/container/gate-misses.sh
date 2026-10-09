#!/usr/bin/env bash
# gate-misses.sh LOG: exits 0 when every failure in LOG (the output of a
# `cargo test ... -- --nocapture` run) is a wall-clock gate miss, and 1
# otherwise. ci.sh's advisory timing steps warn instead of failing only
# then (decisions.md laptop-open, "only gate misses are advisory").
#
# A gate miss is a panic whose message starts with the marker
# "timing gate missed" (the `GATE_MISS` constants of
# strand-render/tests/theme_swap_bench.rs, strand/src/bench.rs and
# strand/src/run.rs; their tests check this file still names it). LOG
# passes when:
#   - libtest reported "test result: FAILED" with N failed tests,
#   - every panic in it ("thread '...' panicked at ...:" followed by its
#     message line) carries the marker, and
#   - at least N panics do (so a failure without a panic, such as a
#     `should_panic` test that did not panic, does not pass).
# Anything else (a functional assertion, a crash, a build error, no test
# result at all) is a real failure.

set -uo pipefail

[ "$#" = 1 ] || { echo "usage: $0 LOG" >&2; exit 2; }

sed 's/\x1b\[[0-9;]*m//g' "$1" | awk '
  /^test result: FAILED\./ {
    results++
    if (match($0, /[0-9]+ failed/)) failed += substr($0, RSTART, RLENGTH) + 0
  }
  /^thread .* panicked at / {
    panics++
    if ((getline msg) > 0 && index(msg, "timing gate missed") == 1) gates++
  }
  END { exit !(results > 0 && failed > 0 && panics == gates && gates >= failed) }
'
