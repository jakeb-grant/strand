#!/usr/bin/env bash
# The evaluation sandbox's checks (docs/eval.md), shared by GitHub's
# `eval` job, the local ci.sh and `scripts/container/run.sh eval
# selftest`. Run from anywhere; it works from the repository root, with
# CARGO_TARGET_DIR (default target/) as the root workspace's.
#
#   1  cargo fmt --check of strand-eval (-p: its path dependencies are the
#      root workspace's to format)
#   2  cargo clippy of eval/, every target, -D warnings
#   3  check-lock.sh, check-isolation.sh and their self-tests
#   4  cargo test of eval/
#   5  strand-eval check --gate over the curated and regression scenarios
#
# Usage: scripts/eval/ci.sh [STEP...]   (default: all; affected.sh runs 3 4 5)
# Stops at the first failing step and names it.

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
MANIFEST=eval/Cargo.toml
steps=("$@")
[ "${#steps[@]}" -gt 0 ] || steps=(1 2 3 4 5)

run() {
  local n=$1 name=$2
  shift 2
  echo "=== eval [$n] $name"
  if ! "$@"; then
    echo "EVAL FAILED at step $n: $name"
    exit 1
  fi
}

lock_and_isolation() {
  bash scripts/eval/check-lock.sh &&
    bash scripts/eval/test-check-lock.sh &&
    bash scripts/eval/check-isolation.sh &&
    bash scripts/eval/test-check-isolation.sh
}

gate() {
  # The scenarios land with the driver and the checks; until then there
  # is nothing to gate (said, not silently passed).
  if [ ! -d eval/scenarios ]; then
    echo "SKIP: eval/scenarios does not exist yet"
    return 0
  fi
  cargo run -q --locked --manifest-path "$MANIFEST" --bin strand-eval -- \
    check --gate eval/scenarios/curated eval/scenarios/regressions --out target/eval/ci
}

for n in "${steps[@]}"; do
  case "$n" in
    1) run 1 "fmt" cargo fmt --manifest-path "$MANIFEST" -p strand-eval --check ;;
    2) run 2 "clippy" cargo clippy --locked --manifest-path "$MANIFEST" --all-targets -- -D warnings ;;
    3) run 3 "lock and isolation" lock_and_isolation ;;
    4) run 4 "tests" cargo test --locked --manifest-path "$MANIFEST" ;;
    5) run 5 "scenario gate" gate ;;
    *) echo "scripts/eval/ci.sh: no step $n (1 to 5)" >&2; exit 2 ;;
  esac
done
echo "=== eval: all steps passed (${steps[*]})"
