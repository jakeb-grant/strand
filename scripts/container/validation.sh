#!/usr/bin/env bash
# CI's nightly `validation` job, and the same run here: the GPU tests with
# Khronos's validation layer on lavapipe, where a validation error fails the run
# (decisions.md robustness). The layer's manifest is off the loader's path
# (Dockerfile), so this is the only place it loads.
#
# How an error fails: wgpu registers a messenger of its own in a debug build, whose
# errors only reach `log`, so nothing there can fail. The layer's own action, set
# by a settings file (Ubuntu's 1.3.275 doesn't read the action from the
# environment), raises SIGTRAP on the error and nextest names the test. There are
# two passes:
#   core  every check but synchronization, trapping on any error. Holds children
#         too: a `strand run` that traps ends its test.
#   sync  the synchronization checks wgpu always asks for, logging only, and
#         validation-summary.py judges each message in each test's output. The
#         one hazard the layer reports in vello_gpu's buffers has an ID shared by
#         every read-after-write hazard, so it can't be filtered by ID without
#         hiding the others; this holds it to its exact accesses instead.
# First, proofs that each can fail: a test making a bad call must trap in core and
# be read in sync, and strand's GPU thread must have the layer loaded.
#
# Which tests: those opening a device, but not memory or wall-clock gates, whose
# numbers the layer changes (`strand::budgets`, `strand::demo`, `strand::services`,
# `strand::gpu_idle`, `strand-gpu::start_cost`, the timing benches). Pixel
# comparisons don't hold in a process with the layer loaded
# (`pixel_gates_held`), so their draws still get checked.
#
# Run: scripts/container/run.sh scripts/container/validation.sh
# Out: target/validation/: summary.md, the junit files and logs.

set -uo pipefail
cd "$(dirname "$0")/../.."

OUT=target/validation
GPU_TESTS='binary_id(/^strand-gpu(::|$)/) and not binary_id(=strand-gpu::start_cost) and not binary_id(=strand-gpu::validation) or binary_id(/^strand-render::(gpu|gpu_effects|inspect)$/) or (binary_id(=strand::report) and test(=a_gpu_surface_is_repainted_on_the_cpu))'
PROOFS='test(=a_bad_vulkan_call_traps) or test(=strands_gpu_thread_runs_under_the_layer)'
# As the test job's -j on a runner, more on a larger machine.
JOBS=$(($(nproc) * 3 / 2))

mkdir -p "$OUT"
find "$OUT" -maxdepth 1 -type f -delete
ROOT=$PWD
cat >"$OUT/core.settings" <<SETTINGS
khronos_validation.debug_action = VK_DBG_LAYER_ACTION_LOG_MSG,VK_DBG_LAYER_ACTION_BREAK
khronos_validation.report_flags = error
khronos_validation.log_filename = stdout
khronos_validation.validate_sync = false
SETTINGS
cat >"$OUT/sync.settings" <<SETTINGS
khronos_validation.debug_action = VK_DBG_LAYER_ACTION_LOG_MSG
khronos_validation.report_flags = error
khronos_validation.log_filename = stdout
SETTINGS

# Only in this script's environment: VK_ADD_LAYER_PATH is what finds the layer.
export VK_ADD_LAYER_PATH=/opt/vulkan-validation
export VK_LOADER_LAYERS_ENABLE=VK_LAYER_KHRONOS_validation

# run NAME MODE NEXTEST-ARGS...: one nextest run, its junit file kept as NAME.xml
# and its status in NAME.status. A failing test is the summary's to judge.
run() {
  local name=$1 mode=$2 status
  shift 2
  echo
  echo "=== validation: $name ($mode)"
  rm -f target/nextest/validation/junit.xml
  VK_LAYER_SETTINGS_PATH=$ROOT/$OUT/$mode.settings \
    cargo nextest run --profile validation -j "$JOBS" "$@" 2>&1 |
    tee "$OUT/$name.log" | grep -avE '^\s*\S*(PASS|SLOW)' | cut -c1-600
  status=${PIPESTATUS[0]}
  echo "$status" >"$OUT/$name.status"
  [ ! -f target/nextest/validation/junit.xml ] ||
    mv target/nextest/validation/junit.xml "$OUT/$name.xml"
}

# Built without the layer, whose cost the compiler needn't pay.
VK_ADD_LAYER_PATH= VK_LOADER_LAYERS_ENABLE= cargo nextest list --workspace --profile validation --message-format json --cargo-quiet >/dev/null || exit 1

dpkg-query -W -f='${Version}\n' vulkan-validationlayers >"$OUT/layer.version"
run proof-core core --workspace --run-ignored only -E "$PROOFS"
run proof-sync sync --workspace --run-ignored only -E "$PROOFS"
for mode in core sync; do
  run "$mode-1" "$mode" --workspace -E "$GPU_TESTS"
  # Alone, for the path a presented surface takes under frames that send nothing:
  # `--workspace` turns on `eval-hooks`, which draws every frame (ci.yml's test job).
  run "$mode-2" "$mode" -p strand --test presented_quiet
done

exec python3 -I scripts/container/validation-summary.py "$OUT"
