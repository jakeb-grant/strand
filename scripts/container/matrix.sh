#!/usr/bin/env bash
# The CI `compositors` job (.github/workflows/ci.yml) on this machine:
# builds the compositor_matrix test in the CI image (run.sh), then runs
# scripts/compositor-matrix-ci.sh in Dockerfile.matrix's pinned Arch image
# (published as the CI image is; image.sh) against sway, niri and labwc (the
# compositor without an IPC adapter).
#
# Hyprland: CI loads vkms (a virtual KMS card); that needs modprobe, not
# done here. Only the render node /dev/dri/renderD128 is passed in (never
# a /dev/dri/card*): Hyprland is tried on it, and skipped with a message
# when it cannot start without a KMS card.
#
# Usage: scripts/container/matrix.sh [sway] [niri] [hyprland] [labwc]
#        (default: all four). Logs and shots: target/matrix/<compositor>.
# Env:   MATRIX_OUTPUTS (default 2): outputs for sway and Hyprland (niri: 1).
#        MATRIX_FILTER, MATRIX_LOOP, MATRIX_WAYLAND_DEBUG: one test, many
#        runs, Wayland protocol logs (scripts/compositor-matrix.sh), e.g.
#        MATRIX_FILTER=window_state_actions_follow_the_compositor MATRIX_LOOP=100 \
#          scripts/container/matrix.sh labwc

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
KINDS=("$@")
[ "${#KINDS[@]}" -gt 0 ] || KINDS=(sway niri hyprland labwc)
RENDER=/dev/dri/renderD128

ARCH_IMAGE=$(STRAND_IMAGE_FILE=Dockerfile.matrix "$HERE/image.sh") || exit 1

# The test, as CI builds it (its path from cargo's JSON messages).
echo "=== building the compositor_matrix test"
json=$(CARGO_TERM_COLOR=never "$HERE/run.sh" cargo test -p strand --test compositor_matrix --no-run --message-format=json) ||
  { echo "the compositor_matrix test did not build"; exit 1; }
TEST=$(printf '%s\n' "$json" | jq -r 'select(.reason == "compiler-artifact" and .target.name == "compositor_matrix" and .executable != null) | .executable' | tail -1)
[ -x "$TEST" ] || { echo "no compositor_matrix test binary ($TEST)"; exit 1; }
echo "matrix test: $TEST"

mounts=(-v "$ROOT:$ROOT")
common=$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
case "$common" in "" | "$ROOT"/*) ;; *) mounts+=(-v "$common:$common:ro") ;; esac

run_arch() { # MATRIX DRM_CARD [docker args...]
  local matrix=$1 card=$2
  shift 2
  docker run --rm --init "$@" "${mounts[@]}" -w "$ROOT" \
    -e MATRIX="$matrix" -e STRAND_DRM_CARD="$card" -e MATRIX_OUTPUTS="${MATRIX_OUTPUTS:-2}" \
    -e MATRIX_FILTER="${MATRIX_FILTER:-}" -e MATRIX_LOOP="${MATRIX_LOOP:-1}" \
    -e MATRIX_WAYLAND_DEBUG="${MATRIX_WAYLAND_DEBUG:-}" \
    "$ARCH_IMAGE" bash scripts/compositor-matrix-ci.sh "$TEST"
}

results=()
status=0
for kind in "${KINDS[@]}"; do
  case "$kind" in
    sway | niri | labwc)
      echo "=== $kind"
      if run_arch "$kind" ""; then results+=("$kind: passed"); else results+=("$kind: FAILED"); status=1; fi
      ;;
    hyprland)
      echo "=== hyprland (render node $RENDER only; no KMS card)"
      if [ ! -c "$RENDER" ]; then
        results+=("hyprland: SKIPPED (no $RENDER)")
        continue
      fi
      rm -rf "$ROOT/target/matrix/hyprland"
      if run_arch hyprland "$RENDER" --device "$RENDER"; then
        results+=("hyprland: passed (render node only)")
      elif [ ! -f "$ROOT/target/matrix/hyprland/test.log" ]; then
        # Hyprland never became ready: the test did not run.
        echo "Hyprland did not start on the render node alone; its log:"
        tail -25 "$ROOT/target/matrix/hyprland/hyprland.log" 2>/dev/null || true
        results+=("hyprland: SKIPPED (aquamarine needs a KMS card; vkms needs modprobe, and the host's card0 is never passed in; nested Hyprland 0.56.2 fails in sway and niri, decisions.md laptop-verify; CI covers it)")
      else
        results+=("hyprland: FAILED (render node only)")
        status=1
      fi
      ;;
    *) echo "unknown compositor: $kind (sway, niri, hyprland or labwc)"; exit 2 ;;
  esac
done

echo
echo "=== compositor matrix"
printf '  %s\n' "${results[@]}"
exit "$status"
