#!/usr/bin/env bash
# Tests tests-ran.sh on stand-in commands: a passing run that ran no test
# fails, one that ran a test passes, and a failing run keeps its status.
# Run: scripts/container/run.sh scripts/container/test-tests-ran.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
bad=0

# expect WANT NAME CMD...: tests-ran.sh CMD exits WANT.
expect() {
  local want=$1 name=$2 got=0
  shift 2
  "$HERE/tests-ran.sh" "$@" >/dev/null 2>&1 || got=$?
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: exit $got, wanted $want"
    bad=1
  fi
}

expect 1 "every test filtered out (the hollow --bin strand step)" bash -c '
  echo "     Running unittests src/main.rs (target/debug/deps/strand-0)"
  echo "running 0 tests"
  echo "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 7 filtered out; finished in 0.00s"'
expect 1 "only ignored tests" bash -c '
  echo "running 1 test"
  echo "test bench::x ... ignored"
  echo "test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 9 filtered out; finished in 0.00s"'
expect 1 "no libtest summary at all" true
expect 0 "a test ran" bash -c '
  echo "running 2 tests"
  echo "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 900 filtered out; finished in 41.20s"'
expect 0 "a test ran in the second of two harnesses, coloured" bash -c '
  echo "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 7 filtered out; finished in 0.00s"
  printf "test result: \033[32mok\033[0m. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out\n"'
expect 3 "a failing run keeps its status" bash -c '
  echo "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out"
  exit 3'
expect 101 "a failing run with no summary keeps its status" bash -c 'exit 101'
expect 2 "no command"

# The live docs cite the strand lib's tests with --lib: since the lib/bin
# split, `-p strand --bin strand <filter>` runs 0 tests and passes.
ROOT=$(cd "$HERE/../.." && pwd)
for doc in README.md docs/development.md docs/features.md; do
  if tr '\n' ' ' <"$ROOT/$doc" | grep -Eq -- '-p +strand +--bin +strand +[a-z_]'; then
    echo "FAIL $doc cites a strand test with --bin strand (runs 0 tests); use --lib"
    bad=1
  else
    echo "ok   $doc cites no hollow --bin strand test command"
  fi
done

exit "$bad"
