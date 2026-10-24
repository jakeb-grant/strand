#!/usr/bin/env bash
# The CI jobs `lint`, `test`, `budgets`, `acceptance`, `timing` and
# `eval` (.github/workflows/ci.yml), step by step and one job after the
# other,
# inside the image of scripts/container/run.sh (run it as
# `scripts/container/run.sh ci`). Stops at the first failing step and
# names it. Keep the steps in step with ci.yml.
#
# The wall-clock latency gates (the `timing` job: theme_swap_bench,
# reload_latency, list_scroll_bench and anim_bench's CPU frames) are
# advisory here: they run and
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
  "" | lint | test | budgets | acceptance | timing | eval) ;;
  *) echo "CI_JOB=$ONLY: not one of lint, test, budgets, acceptance, timing, eval" >&2; exit 2 ;;
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
  grep -a -E 'timing gate missed|theme swap .* median|crossfading swap|scopes, spring\(|reload latency over|save → presented|token p95|markup p95|portal SettingChanged|monitor plugged|scale change heard|panicked at|, over [0-9.]+|^token edits|^markup edits|^a (scale change|plugged monitor|portal change):|^list scroll|^anim gate|^test .* FAILED$|^test result:' "$1" |
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
step "cargo clippy --workspace --all-targets --all-features" 60 \
  cargo clippy --workspace --all-targets --all-features
step "scripts/check-tree-sitter.sh" 10 scripts/check-tree-sitter.sh
step "scripts/container/test-gate-misses.sh" 5 scripts/container/test-gate-misses.sh
step "scripts/container/test-check-tree-sitter.sh" 5 scripts/container/test-check-tree-sitter.sh
step "scripts/container/test-ledger.sh" 5 scripts/container/test-ledger.sh
step "scripts/container/test-gpu-measure.sh" 5 scripts/container/test-gpu-measure.sh
step "scripts/test-size-report.sh" 5 scripts/test-size-report.sh
step "scripts/container/test-tests-ran.sh" 5 scripts/container/test-tests-ran.sh
step "scripts/container/test-affected.sh" 5 scripts/container/test-affected.sh

job=test
step "cargo test -p strand-services --no-default-features --lib" 60 \
  cargo test -p strand-services --no-default-features --lib
step "cargo test --workspace (skipping the fuzzer and the 100 reloads)" 60 \
  cargo test --workspace -- --skip random_edits_through_five_save_styles --skip a_hundred_reloads_reconnect_and_restart_nothing
step "cargo test -p strand-auth --features faults" 30 \
  cargo test -p strand-auth --features faults
step "cargo test -p strand --test reloads" 10 \
  cargo test -p strand --test reloads -- --nocapture
step "random_edits_through_five_save_styles (STRAND_FUZZ_MAX_GAP_MS=20)" 60 \
  env STRAND_FUZZ_MAX_GAP_MS=20 scripts/container/tests-ran.sh cargo test -p strand --lib random_edits_through_five_save_styles -- --nocapture --test-threads=1

job=budgets
# M5's size ledger: the .text gate test adds each build's half here, and
# ledger.sh prints the line after the job (docs/m5-plan.md).
export STRAND_LEDGER_FILE=$LOGS/ledger
step "cargo test --release -p strand --test demo" 60 \
  cargo test --release -p strand --test demo
step "cargo test --release -p strand --test services" 60 \
  cargo test --release -p strand --test services -- --test-threads=1
step "cargo test --release -p strand --test budgets" 60 \
  cargo test --release -p strand --test budgets -- --nocapture --test-threads=1
step "cargo test --release -p strand --no-default-features --test budgets (.text gate)" 60 \
  cargo test --release -p strand --no-default-features --test budgets the_release_binary_code_stays_within_its_gate -- --nocapture
step "size ledger" 1 scripts/container/ledger.sh "$STRAND_LEDGER_FILE"

job=acceptance
step "cargo test --release -p strand --test acceptance" 60 \
  cargo test --release -p strand --test acceptance -- --nocapture --test-threads=1

job=timing
timing_step "cargo test --profile timing -p strand-render --test theme_swap_bench" 60 \
  cargo test --profile timing -p strand-render --test theme_swap_bench -- --nocapture
# Advisory here, so fewer rounds than GitHub's 50 unless strict
# (decisions.md, ci-speed): a p95 over 15 rounds is noisier, and the
# timing job enforces the gate on 50.
ROUNDS=15
[ "$STRICT_TIMING" = 1 ] && ROUNDS=50
timing_step "reload_latency (--profile timing, STRAND_LATENCY_ROUNDS=$ROUNDS)" 60 \
  env STRAND_LATENCY_ROUNDS=$ROUNDS scripts/container/tests-ran.sh cargo test --profile timing -p strand --lib reload_latency -- --nocapture --test-threads=1
timing_step "cargo test --profile timing -p strand-render --test list_scroll_bench" 30 \
  cargo test --profile timing -p strand-render --test list_scroll_bench -- --nocapture
timing_step "cargo test --profile timing -p strand-render --test motion (frame time)" 30 \
  cargo test --profile timing -p strand-render --test motion animated_frames_fit_the_refresh_budget -- --nocapture
timing_step "cargo test --profile timing -p strand-render --test morph (island card frame time)" 30 \
  cargo test --profile timing -p strand-render --test morph the_island_card_swells_within_the_frame_budget -- --nocapture
timing_step "cargo test --profile timing -p strand-render --test anim_bench (animated CPU frames)" 30 \
  cargo test --profile timing -p strand-render --test anim_bench animated_cpu_frames_hold_their_gates -- --nocapture --test-threads=1
timing_step "cargo test --profile timing -p strand-compiler --test theme (theme swap logic)" 30 \
  cargo test --profile timing -p strand-compiler --test theme a_theme_swap_is_under_five_milliseconds_of_logic -- --nocapture

# The animation evaluation sandbox (eval/, docs/eval.md): its own
# workspace, so its steps are scripts/eval/ci.sh's 1 to 7 (fmt, clippy,
# lock and isolation, tests, the scenario gate, the matrix, a glitch
# report replayed end to end), as GitHub's eval job runs them.
job=eval
step "cargo fmt --check (strand-eval)" 10 bash scripts/eval/ci.sh 1
step "cargo clippy (eval/, -D warnings)" 60 bash scripts/eval/ci.sh 2
step "check-lock.sh, check-isolation.sh, their self-tests, test-worktree.sh, test-local-mounts.sh and test-repeat.sh" 10 bash scripts/eval/ci.sh 3
step "cargo test (eval/) and test-report-view.sh" 60 bash scripts/eval/ci.sh 4
step "strand-eval refs-check and check --gate (curated and regressions)" 30 bash scripts/eval/ci.sh 5
step "the effect matrix at 60 and 144 Hz against its references" 30 bash scripts/eval/ci.sh 6
step "a glitch report replayed end to end (test-report-view.sh --e2e)" 30 bash scripts/eval/ci.sh 7

echo
# The compile cache's hits and misses (run.sh's sccache), when it ran.
if [ -n "${RUSTC_WRAPPER:-}" ] && command -v sccache >/dev/null; then
  sccache --show-stats 2>/dev/null | grep -E '^(Compile requests|Cache hits|Cache misses|Non-cacheable calls) ' | sed 's/^/sccache: /' || true
fi
if [ "${#warned[@]}" -gt 0 ]; then
  echo "WARN: ${#warned[@]} timing step(s) missed their gate (advisory; GitHub's timing job enforces them):"
  printf '  %s\n' "${warned[@]}"
  echo "CI PASSED with timing warnings: all $n steps${ONLY:+ (job $ONLY)}"
else
  echo "CI PASSED: all $n steps${ONLY:+ (job $ONLY)}"
fi
