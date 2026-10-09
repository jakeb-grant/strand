#!/usr/bin/env bash
# Tests gate-misses.sh on hand-written logs: only runs whose every
# failure is a wall-clock gate miss count as advisory.
# Run: scripts/container/run.sh scripts/container/test-gate-misses.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0

# expect WANT NAME: gate-misses.sh on the log on stdin exits WANT.
expect() {
  local want=$1 name=$2 got=0
  cat >"$DIR/log"
  "$HERE/gate-misses.sh" "$DIR/log" || got=$?
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: exit $got, wanted $want"
    bad=1
  fi
}

expect 0 "a gate miss" <<'EOF'
running 1 test
save → presented over 50 edits each at 16.67 ms refresh:
thread 'bench::reload_latency_to_the_presented_frame' panicked at crates/strand/src/bench.rs:873:5:
timing gate missed: token edits: p95 on a monitor 35.3 ms (headless 20.9, the gate breaks above 20.6)
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test bench::reload_latency_to_the_presented_frame ... FAILED

failures:
    bench::reload_latency_to_the_presented_frame

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 41.20s
EOF

expect 0 "a gate miss, with the thread id Rust 1.97 prints" <<'EOF'
thread 'a_crossfading_swap_is_under_five_milliseconds_of_work' (53) panicked at crates/strand-render/tests/theme_swap_bench.rs:553:13:
timing gate missed: 1.772566ms blending a frame, over 1µs
test result: FAILED. 4 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.11s
EOF

expect 0 "two gate misses, coloured" <<EOF
thread 'a' panicked at t.rs:1:1:
timing gate missed: age 1: 5.1ms of work, over 5ms
thread 'b' panicked at t.rs:2:1:
timing gate missed: 4.04ms blending a frame, over 4ms
test result: $(printf '\033[31m')FAILED$(printf '\033[0m'). 3 passed; 2 failed; 0 ignored
EOF

expect 1 "a functional assertion" <<'EOF'
thread 'a_theme_swap_is_under_five_milliseconds_of_work' panicked at crates/strand-render/tests/theme_swap_bench.rs:320:17:
light → dark: nothing springs
test result: FAILED. 4 passed; 1 failed; 0 ignored
EOF

expect 1 "a gate miss and a functional assertion" <<'EOF'
thread 'a' panicked at t.rs:1:1:
timing gate missed: age 1: 5.1ms of work, over 5ms
thread 'quantiles' panicked at t.rs:121:5:
assertion `left == right` failed
  left: 2ms
 right: 3ms
test result: FAILED. 3 passed; 2 failed; 0 ignored
EOF

expect 1 "a thread of the test panicking under a gate miss" <<'EOF'
thread '<unnamed>' panicked at crates/strand/src/run.rs:2990:9:
reload: no diff
thread 'run::tests::reload_latency_meets_its_budget' panicked at crates/strand/src/run.rs:3051:9:
timing gate missed: token edits: p95 36.0 ms
test result: FAILED. 0 passed; 1 failed; 0 ignored
EOF

expect 1 "more failures than gate misses" <<'EOF'
thread 'a' panicked at t.rs:1:1:
timing gate missed: age 1: 5.1ms of work, over 5ms
note: test did not panic as expected
test result: FAILED. 3 passed; 2 failed; 0 ignored
EOF

expect 1 "the marker later in the message" <<'EOF'
thread 'a' panicked at t.rs:1:1:
light → dark: timing gate missed
test result: FAILED. 4 passed; 1 failed; 0 ignored
EOF

expect 1 "no test result (a crash)" <<'EOF'
thread 'a' panicked at t.rs:1:1:
timing gate missed: age 1: 5.1ms of work, over 5ms
error: test failed, to rerun pass `-p strand-render --test theme_swap_bench`
Caused by:
  process didn't exit successfully (signal: 11, SIGSEGV: invalid memory reference)
EOF

expect 1 "a build error" <<'EOF'
error[E0425]: cannot find value `x` in this scope
error: could not compile `strand` (bin "strand" test) due to 1 previous error
EOF

expect 1 "a passing run" <<'EOF'
test result: ok. 5 passed; 0 failed; 0 ignored
EOF

exit "$bad"
