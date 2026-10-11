#!/usr/bin/env bash
# M5 wave-0 spike 1 (decisions.md m5-infra): can headless sway render on
# the laptop's GPU through the render node alone? Runs inside.sh in
# scripts/container/run.sh's image with only /dev/dri/renderD128 and the
# hardware Vulkan ICD (gpu.sh's arrangement; never a /dev/dri/card*).
# The logs land in OUTDIR (default target/spike-gpu-compositor).
#
# Usage: spikes/gpu-compositor/run.sh [OUTDIR]
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=${1:-$ROOT/target/spike-gpu-compositor}
[ -c /dev/dri/renderD128 ] || { echo "no /dev/dri/renderD128" >&2; exit 1; }
case "$(cat /sys/class/drm/renderD128/device/vendor)" in
  0x8086) icd=/usr/share/vulkan/icd.d/intel_icd.json ;;
  0x1002) icd=/usr/share/vulkan/icd.d/radeon_icd.json ;;
  *) icd=${GPU_ICD:?set GPU_ICD} ;;
esac
mkdir -p "$OUT"
RUN_GPU_HARDWARE=$icd "$ROOT/scripts/container/run.sh" bash "$HERE/inside.sh" "$OUT"
