#!/usr/bin/env bash
# The advisory real-hardware GPU leg (decisions.md m4-owner): runs a
# command in the CI image (run.sh) with the laptop's GPU instead of
# lavapipe. CI and run.sh run the GPU tier on lavapipe; this leg is the
# owner's opt-in check on real hardware, advisory like the laptop
# timing gates.
#
# Usage: scripts/container/gpu.sh [CMD [ARGS...]]
#        e.g. scripts/container/gpu.sh cargo test -p strand-gpu
#        Without a command it proves the device works: vulkaninfo
#        --summary must list a non-CPU device, and vkcube presents 120
#        frames to the leg's own headless sway.
# Env:   STRAND_STRICT_GPU=1 makes a failure fail (exit status of CMD);
#        otherwise a failure prints WARN and the script exits 0.
#        GPU_ICD=/usr/share/vulkan/icd.d/<name>.json picks the hardware
#        ICD (default: from the render node's PCI vendor: intel_icd.json,
#        radeon_icd.json or nouveau_icd.json).
#        Everything run.sh takes (CARGO_BUILD_JOBS, STRAND_*).
#
# Only /dev/dri/renderD128 is passed in, never a /dev/dri/card* and
# never the host's Wayland socket. Inside, gpu-session.sh starts a
# private headless sway (its own XDG_RUNTIME_DIR, rendering with GLES2
# on the render node, pixman if that fails) and runs CMD with
# WAYLAND_DISPLAY set to it; the tests that start their own sway still
# do. STRAND_GPU_HARDWARE=1 and STRAND_REQUIRE_GPU=1 are set in there.

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
RENDER=/dev/dri/renderD128
STRICT=${STRAND_STRICT_GPU:-0}

warn_or_fail() { # STATUS MESSAGE
  local status=$1 msg=$2
  if [ "$STRICT" = 1 ]; then
    echo "gpu.sh FAILED: $msg (exit $status)" >&2
    exit "$status"
  fi
  echo "########################################################################"
  echo "### WARN gpu.sh: $msg (exit $status)."
  echo "### Advisory: the lavapipe tier in run.sh and CI is the enforcing gate."
  echo "### STRAND_STRICT_GPU=1 makes this fail."
  echo "########################################################################"
  exit 0
}

[ -c "$RENDER" ] || warn_or_fail 1 "no $RENDER on this machine"

icd=${GPU_ICD:-}
if [ -z "$icd" ]; then
  vendor=$(cat /sys/class/drm/renderD128/device/vendor 2>/dev/null || true)
  case "$vendor" in
    0x8086) icd=/usr/share/vulkan/icd.d/intel_icd.json ;;
    0x1002) icd=/usr/share/vulkan/icd.d/radeon_icd.json ;;
    0x10de) icd=/usr/share/vulkan/icd.d/nouveau_icd.json ;;
    *) warn_or_fail 1 "unknown GPU vendor '$vendor' on $RENDER (set GPU_ICD)" ;;
  esac
fi
case "$icd" in
  *lvp_icd*) echo "gpu.sh: GPU_ICD=$icd is lavapipe; this leg is for hardware" >&2; exit 2 ;;
esac

[ "$#" -gt 0 ] || set -- smoke
echo "=== gpu.sh: $RENDER ($(cat /sys/class/drm/renderD128/device/vendor 2>/dev/null):$(cat /sys/class/drm/renderD128/device/device 2>/dev/null)), ICD $icd"
echo "=== gpu.sh: $*"
RUN_GPU_HARDWARE=$icd "$HERE/run.sh" bash "$HERE/gpu-session.sh" "$@"
status=$?
[ "$status" = 0 ] || warn_or_fail "$status" "'$*' failed on the hardware GPU"
echo "gpu.sh PASSED: $*"
