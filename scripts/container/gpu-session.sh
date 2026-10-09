#!/usr/bin/env bash
# Inside the container of scripts/container/gpu.sh: a private headless
# sway on the render node, then CMD (or the `smoke` check) with
# WAYLAND_DISPLAY set to that sway. Never the host's session: the
# runtime dir is a fresh one in the container and no host socket is
# mounted.
#
# Usage (from gpu.sh): gpu-session.sh smoke | CMD [ARGS...]

set -uo pipefail

[ "${STRAND_GPU_HARDWARE:-}" = 1 ] || { echo "gpu-session.sh runs inside gpu.sh's container" >&2; exit 2; }
for dev in /dev/dri/*; do
  case "$dev" in */card*) echo "gpu-session.sh: $dev is in the container; only the render node may be" >&2; exit 2 ;; esac
done

unset WAYLAND_DISPLAY DISPLAY
export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR=$(mktemp -d /tmp/gpu-xdg.XXXXXX)
chmod 0700 "$XDG_RUNTIME_DIR"
LOG=$XDG_RUNTIME_DIR/sway.log
SWAY_PID=
cleanup() { [ -n "$SWAY_PID" ] && kill "$SWAY_PID" 2>/dev/null; wait 2>/dev/null; rm -rf "$XDG_RUNTIME_DIR"; }
trap cleanup EXIT

start_sway() { # RENDERER
  local renderer=$1 i
  : >"$LOG"
  printf 'output HEADLESS-1 resolution 1280x720\n' >"$XDG_RUNTIME_DIR/sway.conf"
  WLR_BACKENDS=headless WLR_RENDERER=$renderer WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128 \
    WLR_LIBINPUT_NO_DEVICES=1 sway -c "$XDG_RUNTIME_DIR/sway.conf" -d >"$LOG" 2>&1 &
  SWAY_PID=$!
  for i in $(seq 100); do
    for sock in "$XDG_RUNTIME_DIR"/wayland-*; do
      case "$sock" in *.lock | *'*'*) continue ;; esac
      [ -S "$sock" ] && { export WAYLAND_DISPLAY=${sock##*/}; return 0; }
    done
    kill -0 "$SWAY_PID" 2>/dev/null || break
    sleep 0.1
  done
  kill "$SWAY_PID" 2>/dev/null; wait "$SWAY_PID" 2>/dev/null; SWAY_PID=
  return 1
}

if start_sway gles2; then
  echo "=== headless sway (GLES2 on the render node) at $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY (container-private)"
elif start_sway pixman; then
  echo "=== headless sway (pixman; GLES2 did not start) at $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY (container-private)"
else
  echo "headless sway did not start; its log:"; tail -30 "$LOG"; exit 1
fi

if [ "$1" != smoke ]; then
  "$@"
  exit $?
fi

out=$(vulkaninfo --summary 2>&1) || { printf '%s\n' "$out"; exit 1; }
printf '%s\n' "$out" | sed -n '/^Devices/,$p'
printf '%s\n' "$out" | grep -Eq 'deviceType += PHYSICAL_DEVICE_TYPE_(INTEGRATED|DISCRETE)_GPU' ||
  { echo "no hardware Vulkan device (lavapipe only?)"; exit 1; }
echo "=== vkcube: 120 frames presented to the headless sway"
timeout 60 vkcube-wayland --c 120 || { echo "vkcube failed"; exit 1; }
echo "smoke: hardware Vulkan device present and presenting"
