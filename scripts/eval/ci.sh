#!/usr/bin/env bash
# The evaluation sandbox's checks (docs/eval.md), shared by GitHub's
# `eval` job, the local ci.sh and `scripts/container/run.sh eval
# selftest`. Run from anywhere; it works from the repository root, with
# CARGO_TARGET_DIR (default target/) as the root workspace's.
#
#   1  cargo fmt --check of strand-eval (-p: its path dependencies are the
#      root workspace's to format)
#   2  cargo clippy of eval/, every target, -D warnings
#   3  check-lock.sh, check-isolation.sh and their self-tests,
#      test-worktree.sh (ab.sh's and mutants.sh's worktree helpers) and
#      test-local-mounts.sh (what `run.sh eval local` mounts) and
#      test-repeat.sh (the repeated gate and matrix, on a stand-in build)
#   4  cargo test of eval/
#   5  strand-eval refs-check (the reference files: seals, notes, and a
#      recording changed since the merge base with main under its old
#      note), then check --gate over the curated and regression
#      scenarios (each must match its approved reference recording)
#   6  the effect matrix, every cell at 60 and 144 Hz, judged against
#      eval/matrix/known.toml and held to eval/refs/matrix-<hz>.ref (a
#      cell whose recording changed fails until it is approved)
#
# Usage: scripts/eval/ci.sh [STEP...]   (default: all, as affected.sh runs it)
# Stops at the first failing step and names it.

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
MANIFEST=eval/Cargo.toml
steps=("$@")
[ "${#steps[@]}" -gt 0 ] || steps=(1 2 3 4 5 6)

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
    bash scripts/eval/test-check-isolation.sh &&
    bash scripts/eval/test-worktree.sh &&
    bash scripts/eval/test-local-mounts.sh &&
    bash scripts/eval/test-repeat.sh
}

# The merge base with main, which refs-check holds changed references
# to: main, else origin/main (GitHub's eval job fetches its history);
# none (a checkout without it) skips that rule and says so.
merge_base() {
  git merge-base HEAD main 2>/dev/null || git merge-base HEAD origin/main 2>/dev/null || true
}

# The gate's summary goes to GitHub's step summary from here, not from
# strand-eval (whose tests and mutant runs would write to it too).
gate() {
  local status=0 base
  base=$(merge_base)
  cargo run -q --locked --manifest-path "$MANIFEST" --bin strand-eval -- \
    refs-check ${base:+--base "$base"} || return 1
  rm -f target/eval/ci/summary.md
  cargo run -q --locked --manifest-path "$MANIFEST" --bin strand-eval -- \
    check --gate eval/scenarios/curated eval/scenarios/regressions --out target/eval/ci ||
    status=$?
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ] && [ -f target/eval/ci/summary.md ]; then
    cat target/eval/ci/summary.md >>"$GITHUB_STEP_SUMMARY"
  fi
  return "$status"
}

for n in "${steps[@]}"; do
  case "$n" in
    1) run 1 "fmt" cargo fmt --manifest-path "$MANIFEST" -p strand-eval --check ;;
    2) run 2 "clippy" cargo clippy --locked --manifest-path "$MANIFEST" --all-targets -- -D warnings ;;
    3) run 3 "lock and isolation" lock_and_isolation ;;
    4) run 4 "tests" cargo test --locked --manifest-path "$MANIFEST" ;;
    5) run 5 "references and scenario gate" gate ;;
    6) run 6 "the effect matrix against its references" env OUT=target/eval/matrix-ci HZ=60,144 bash scripts/eval/matrix.sh ;;
    *) echo "scripts/eval/ci.sh: no step $n (1 to 6)" >&2; exit 2 ;;
  esac
done
echo "=== eval: all steps passed (${steps[*]})"
