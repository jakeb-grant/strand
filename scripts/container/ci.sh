#!/usr/bin/env bash
# The CI jobs `lint`, `test`, `budgets`, `acceptance` and `timing`
# (.github/workflows/ci.yml), step by step and one job after the other,
# inside the image of scripts/container/run.sh (run it as
# `scripts/container/run.sh ci`). Stops at the first failing step and
# names it. Keep the steps in step with ci.yml.
#
# Env: CI_FROM=N starts at step N (the steps are numbered as printed).
#      CI_JOB=NAME runs that job's steps only (the setup steps 1 and 2
#      always run).

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
FROM=${CI_FROM:-1}
ONLY=${CI_JOB:-}
case "$ONLY" in
  "" | lint | test | budgets | acceptance | timing) ;;
  *) echo "CI_JOB=$ONLY: not one of lint, test, budgets, acceptance, timing" >&2; exit 2 ;;
esac
n=0
job=setup

# step NAME MINUTES CMD...: runs CMD under a timeout of MINUTES (ci.yml's
# timeout-minutes where it sets one; else the job's 60).
step() {
  local name=$1 minutes=$2
  shift 2
  n=$((n + 1))
  if [ "$n" -lt "$FROM" ]; then
    echo "=== [$n] $job: $name (skipped: CI_FROM=$FROM)"
    return 0
  fi
  if [ -n "$ONLY" ] && [ "$job" != setup ] && [ "$job" != "$ONLY" ]; then
    echo "=== [$n] $job: $name (skipped: CI_JOB=$ONLY)"
    return 0
  fi
  echo
  echo "=== [$n] $job: $name"
  local start=$SECONDS status=0
  timeout --foreground "${minutes}m" "$@" || status=$?
  echo "=== [$n] $job: $name: exit $status after $((SECONDS - start)) s"
  if [ "$status" != 0 ]; then
    [ "$status" = 124 ] && echo "=== timed out after $minutes min"
    echo "CI FAILED at step $n: $job: $name"
    exit "$status"
  fi
}

# .github/actions/setup (every job).
step "Services tier tools" 1 bash -c '
  dbus-daemon --version | head -1 &&
  python3 -c "import dbusmock" && dpkg -s python3-dbusmock | grep "^Version" &&
  pipewire --version'
step "Reference package versions" 1 bash -c '
  sway --version
  dpkg -s fonts-dejavu-core adwaita-icon-theme | grep -E "^(Package|Version)"
  dpkg -l "fonts-*" | grep "^ii" || true
  fc-list : family style file | sort
  fc-match -s "sans-serif:italic" | head -5
  readlink -f /usr/share/icons/default/index.theme || true
  ls /usr/share/icons/Adwaita/cursors/default
  rustc --version'

job=lint
step "cargo fmt --all --check" 60 cargo fmt --all --check
step "cargo clippy --workspace --all-targets" 60 cargo clippy --workspace --all-targets
step "cargo clippy -p strand-services --no-default-features --all-targets" 60 \
  cargo clippy -p strand-services --no-default-features --all-targets

job=test
step "cargo test -p strand-services --no-default-features --lib" 60 \
  cargo test -p strand-services --no-default-features --lib
step "cargo test --workspace (skipping the fuzzer and the 100 reloads)" 60 \
  cargo test --workspace -- --skip random_edits_through_five_save_styles --skip a_hundred_reloads_reconnect_and_restart_nothing
step "cargo test -p strand --test reloads" 10 \
  cargo test -p strand --test reloads -- --nocapture
step "random_edits_through_five_save_styles (STRAND_FUZZ_MAX_GAP_MS=20)" 60 \
  env STRAND_FUZZ_MAX_GAP_MS=20 cargo test -p strand --bin strand random_edits_through_five_save_styles -- --nocapture --test-threads=1

job=budgets
step "cargo test --release -p strand --test demo" 60 \
  cargo test --release -p strand --test demo
step "cargo test --release -p strand --test services" 60 \
  cargo test --release -p strand --test services -- --test-threads=1
step "cargo test --release -p strand --test budgets" 60 \
  cargo test --release -p strand --test budgets -- --nocapture --test-threads=1

job=acceptance
step "cargo test --release -p strand --test acceptance" 60 \
  cargo test --release -p strand --test acceptance -- --nocapture --test-threads=1

job=timing
step "cargo test --profile timing -p strand-render --test theme_swap_bench" 60 \
  cargo test --profile timing -p strand-render --test theme_swap_bench -- --nocapture
step "reload_latency (--profile timing, STRAND_LATENCY_ROUNDS=50)" 60 \
  env STRAND_LATENCY_ROUNDS=50 cargo test --profile timing -p strand --bin strand reload_latency -- --nocapture --test-threads=1

echo
echo "CI PASSED: all $n steps${ONLY:+ (job $ONLY)}"
