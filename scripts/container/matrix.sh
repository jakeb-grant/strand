#!/usr/bin/env bash
# The CI `compositors` job (.github/workflows/ci.yml) on this machine:
# builds the compositor_matrix test in the CI image (run.sh), then runs
# scripts/compositor-matrix-ci.sh in archlinux:latest (a derived image,
# Dockerfile.matrix, with its packages cached) against sway and niri.
#
# Hyprland: CI loads vkms (a virtual KMS card); that needs modprobe, not
# done here. Only the render node /dev/dri/renderD128 is passed in (never
# a /dev/dri/card*): Hyprland is tried on it, and skipped with a message
# when it cannot start without a KMS card.
#
# Usage: scripts/container/matrix.sh [sway] [niri] [hyprland]
#        (default: all three). Logs and shots: target/matrix/<compositor>.
# Env:   MATRIX_REBUILD=1 rebuilds the Arch image (pulls archlinux:latest).

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
KINDS=("$@")
[ "${#KINDS[@]}" -gt 0 ] || KINDS=(sway niri hyprland)
RENDER=/dev/dri/renderD128

hash=$(sha256sum "$HERE/Dockerfile.matrix" | cut -c1-12)
ARCH_IMAGE=strand-matrix:$hash
if [ "${MATRIX_REBUILD:-}" = 1 ] || ! docker image inspect "$ARCH_IMAGE" >/dev/null 2>&1; then
  echo "building $ARCH_IMAGE" >&2
  docker build --pull -t "$ARCH_IMAGE" -f "$HERE/Dockerfile.matrix" "$HERE" >&2 || exit 1
fi

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
    -e MATRIX="$matrix" -e STRAND_DRM_CARD="$card" \
    "$ARCH_IMAGE" bash scripts/compositor-matrix-ci.sh "$TEST"
}

results=()
status=0
for kind in "${KINDS[@]}"; do
  case "$kind" in
    sway | niri)
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
        results+=("hyprland: SKIPPED (aquamarine needs a KMS card; vkms needs modprobe, and the host's card0 is never passed in; CI covers it)")
      else
        results+=("hyprland: FAILED (render node only)")
        status=1
      fi
      ;;
    *) echo "unknown compositor: $kind (sway, niri or hyprland)"; exit 2 ;;
  esac
done

echo
echo "=== compositor matrix"
printf '  %s\n' "${results[@]}"
exit "$status"
