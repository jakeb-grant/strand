#!/usr/bin/env bash
# The CI jobs `lint`, `test`, `budgets`, `acceptance` and `timing`
# (.github/workflows/ci.yml), step by step and one job after the other,
# inside the image of scripts/container/run.sh (run it as
# `scripts/container/run.sh ci`). Stops at the first failing step and
# names it. Keep the steps in step with ci.yml.
#
# The wall-clock latency gates (the `timing` job: theme_swap_bench,
# reload_latency and list_scroll_bench) are advisory here: they run and
# print their numbers, and a step whose only failures are wall-clock
# gate misses (panics marked "timing gate missed", judged by
# gate-misses.sh) prints WARN instead of failing the run (decisions.md
# laptop-open, "laptop timing gates" and "only gate misses are
# advisory"). GitHub's `timing` job
# stays the enforcing gate. Any other failure in those steps (a
# functional assertion, a crash, a build error, a timeout) still fails.
# The memory budgets, pixel tests and every other step stay strict.
#
# Env: CI_FROM=N starts at step N (the steps are numbered as printed).
#      CI_JOB=NAME runs that job's steps only (the setup steps 1 to 3
#      always run).
#      STRAND_STRICT_TIMING=1 makes the timing steps fail as in CI.

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
FROM=${CI_FROM:-1}
ONLY=${CI_JOB:-}
STRICT_TIMING=${STRAND_STRICT_TIMING:-0}
case "$ONLY" in
  "" | lint | test | budgets | acceptance | timing) ;;
  *) echo "CI_JOB=$ONLY: not one of lint, test, budgets, acceptance, timing" >&2; exit 2 ;;
esac
n=0
job=setup
warned=()
LOGS=$(mktemp -d)
trap 'rm -rf "$LOGS"' EXIT

# skipped: whether step $n is left out by CI_FROM or CI_JOB (printed).
skipped() {
  local name=$1
  if [ "$n" -lt "$FROM" ]; then
    echo "=== [$n] $job: $name (skipped: CI_FROM=$FROM)"
    return 0
  fi
  if [ -n "$ONLY" ] && [ "$job" != setup ] && [ "$job" != "$ONLY" ]; then
    echo "=== [$n] $job: $name (skipped: CI_JOB=$ONLY)"
    return 0
  fi
  return 1
}

fail() {
  local name=$1 status=$2 minutes=$3
  [ "$status" = 124 ] && echo "=== timed out after $minutes min"
  echo "CI FAILED at step $n: $job: $name"
  exit "$status"
}

# step NAME MINUTES CMD...: runs CMD under a timeout of MINUTES (ci.yml's
# timeout-minutes where it sets one; else the job's 60).
step() {
  local name=$1 minutes=$2
  shift 2
  n=$((n + 1))
  skipped "$name" && return 0
  echo
  echo "=== [$n] $job: $name"
  local start=$SECONDS status=0
  timeout --foreground "${minutes}m" "$@" || status=$?
  echo "=== [$n] $job: $name: exit $status after $((SECONDS - start)) s"
  [ "$status" = 0 ] || fail "$name" "$status" "$minutes"
}

# The lines of a timing test's output that carry its numbers and, on a
# miss, its verdicts.
key_numbers() {
  grep -a -E 'timing gate missed|theme swap .* median|crossfading swap|scopes, spring\(|reload latency over|save → presented|token p95|markup p95|portal SettingChanged|monitor plugged|scale change heard|panicked at|, over [0-9.]+|^token edits|^markup edits|^a (scale change|plugged monitor|portal change):|^list scroll|^test .* FAILED$|^test result:' "$1" |
    sed 's/\x1b\[[0-9;]*m//g' | cut -c1-400 | head -60
}

# timing_step NAME MINUTES CMD...: a wall-clock latency gate. As `step`
# under STRAND_STRICT_TIMING=1; otherwise a run whose every failure is
# a gate miss (gate-misses.sh) warns and the run goes on. Either way
# its key numbers are printed again after it.
timing_step() {
  local name=$1 minutes=$2
  shift 2
  n=$((n + 1))
  skipped "$name" && return 0
  echo
  if [ "$STRICT_TIMING" = 1 ]; then
    echo "=== [$n] $job: $name (strict: STRAND_STRICT_TIMING=1)"
  else
    echo "=== [$n] $job: $name (advisory: STRAND_STRICT_TIMING=1 enforces it)"
  fi
  local start=$SECONDS status=0 log="$LOGS/step-$n.log"
  timeout --foreground "${minutes}m" "$@" 2>&1 | tee "$log"
  status=${PIPESTATUS[0]}
  echo "=== [$n] $job: $name: exit $status after $((SECONDS - start)) s"
  echo "--- [$n] key numbers:"
  key_numbers "$log" | sed 's/^/    /'
  [ "$status" = 0 ] && return 0
  if [ "$STRICT_TIMING" = 1 ] || ! scripts/container/gate-misses.sh "$log"; then
    fail "$name" "$status" "$minutes"
  fi
  echo "########################################################################"
  echo "### WARN [$n] $job: $name missed its gate on this machine (exit $status)."
  echo "### Advisory here: GitHub's timing job is the enforcing gate."
  echo "### STRAND_STRICT_TIMING=1 makes this step fail."
  echo "########################################################################"
  warned+=("[$n] $job: $name")
}

# .github/actions/setup (every job).
step "Services tier tools" 1 bash -c '
  dbus-daemon --version | head -1 &&
  python3 -c "import dbusmock" && dpkg -s python3-dbusmock | grep "^Version" &&
  pipewire --version'
# The GPU tier (M4): lavapipe is the image's only Vulkan driver
# (VK_DRIVER_FILES, Dockerfile) and the GPU tests require it.
export STRAND_REQUIRE_GPU=1
# Lavapipe is a software adapter, which strand counts as no device
# unless this is set (docs/architecture.md, "`strand-gpu`").
export STRAND_GPU_SOFTWARE=1
step "GPU tier device (lavapipe)" 1 bash -c '
  dpkg -s mesa-vulkan-drivers | grep "^Version"
  out=$(vulkaninfo --summary 2>&1)
  printf "%s\n" "$out" | sed -n "/^Devices/,\$p"
  printf "%s\n" "$out" | grep -Eq "driverName += llvmpipe" || { echo "no lavapipe (llvmpipe) Vulkan device"; exit 1; }'
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
step "cargo clippy -p strand --no-default-features --all-targets" 60 \
  cargo clippy -p strand --no-default-features --all-targets
step "cargo build -p strand --no-default-features" 60 \
  cargo build -p strand --no-default-features

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
timing_step "cargo test --profile timing -p strand-render --test theme_swap_bench" 60 \
  cargo test --profile timing -p strand-render --test theme_swap_bench -- --nocapture
timing_step "reload_latency (--profile timing, STRAND_LATENCY_ROUNDS=50)" 60 \
  env STRAND_LATENCY_ROUNDS=50 cargo test --profile timing -p strand --bin strand reload_latency -- --nocapture --test-threads=1
timing_step "cargo test --profile timing -p strand-render --test list_scroll_bench" 30 \
  cargo test --profile timing -p strand-render --test list_scroll_bench -- --nocapture

echo
if [ "${#warned[@]}" -gt 0 ]; then
  echo "WARN: ${#warned[@]} timing step(s) missed their gate (advisory; GitHub's timing job enforces them):"
  printf '  %s\n' "${warned[@]}"
  echo "CI PASSED with timing warnings: all $n steps${ONLY:+ (job $ONLY)}"
else
  echo "CI PASSED: all $n steps${ONLY:+ (job $ONLY)}"
fi
