#!/usr/bin/env bash
# Inside the container of scripts/container/gpu.sh: a private headless
# sway on the render node, then CMD (or the `smoke` check) with
# WAYLAND_DISPLAY set to that sway. Never the host's session: the
# runtime dir is a fresh one in the container and no host socket is
# mounted.
#
# Usage (from gpu.sh): gpu-session.sh smoke | CMD [ARGS...]
#
# The sway renders with GLES2 on the render node, else Vulkan, else
# pixman (decisions.md m5-infra, spike 1: GLES2 and Vulkan both start
# on the laptop's Intel GPU and offer linux-dmabuf, which ANV's WSI
# needs to present). The renderer that started, as sway's log reports
# it (GLES2 or Vulkan on renderD128), is exported as
# STRAND_GPU_SWAY_RENDERER, so tests that start their own sway can
# start it the same way (gpu_idle.rs does with STRAND_GPU_PRESENT=1). The sway log is kept in
# GPU_SWAY_LOG (default target/container/gpu-session-sway.log under
# the working directory).

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
KEEP_LOG=${GPU_SWAY_LOG:-$PWD/target/container/gpu-session-sway.log}
cleanup() {
  [ -n "$SWAY_PID" ] && kill "$SWAY_PID" 2>/dev/null
  wait 2>/dev/null
  mkdir -p "$(dirname "$KEEP_LOG")" && cp "$LOG" "$KEEP_LOG" 2>/dev/null && echo "=== sway log kept in $KEEP_LOG"
  rm -rf "$XDG_RUNTIME_DIR"
}
trap cleanup EXIT

# Whether sway's log shows RENDERER created (wlroots 0.17's lines, as
# seen on the laptop's Intel GPU: decisions.md m5-infra).
renderer_in_log() { # RENDERER
  local node="Opening DRM render node '/dev/dri/renderD128'"
  case "$1" in
    gles2) grep -aqF "$node" "$LOG" && grep -aqF 'Creating GLES2 renderer' "$LOG" ;;
    vulkan) grep -aqF "$node" "$LOG" && grep -aqF 'Found matching Vulkan physical device' "$LOG" ;;
    *) return 0 ;;
  esac
}

start_sway() { # RENDERER
  local renderer=$1 i seen=
  : >"$LOG"
  printf 'output HEADLESS-1 resolution 1280x720\n' >"$XDG_RUNTIME_DIR/sway.conf"
  WLR_BACKENDS=headless WLR_RENDERER=$renderer WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128 \
    WLR_LIBINPUT_NO_DEVICES=1 sway -c "$XDG_RUNTIME_DIR/sway.conf" -d >"$LOG" 2>&1 &
  SWAY_PID=$!
  for i in $(seq 100); do
    for sock in "$XDG_RUNTIME_DIR"/wayland-*; do
      case "$sock" in *.lock | *'*'*) continue ;; esac
      [ -S "$sock" ] || continue
      # The socket alone does not say which renderer came up: require
      # wlroots' own lines for it (and the render node) in the log, so
      # STRAND_GPU_SWAY_RENDERER is what sway reported.
      if renderer_in_log "$renderer"; then
        export WAYLAND_DISPLAY=${sock##*/}
        return 0
      fi
      seen=1
    done
    kill -0 "$SWAY_PID" 2>/dev/null || break
    sleep 0.1
  done
  [ -n "$seen" ] && echo "=== sway took WLR_RENDERER=$renderer but its log does not show that renderer on /dev/dri/renderD128" >&2
  kill "$SWAY_PID" 2>/dev/null; wait "$SWAY_PID" 2>/dev/null; SWAY_PID=
  # sway 1.9 can crash on shutdown and leave its socket behind; the next
  # attempt must not take that one for its own.
  rm -f "$XDG_RUNTIME_DIR"/wayland-*
  return 1
}

export STRAND_GPU_SWAY_RENDERER
if start_sway gles2; then
  STRAND_GPU_SWAY_RENDERER=gles2
  echo "=== headless sway (GLES2 on the render node) at $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY (container-private)"
elif cp "$LOG" "$LOG.gles2" && start_sway vulkan; then
  STRAND_GPU_SWAY_RENDERER=vulkan
  echo "=== headless sway (Vulkan on the render node; GLES2 did not start, log in $LOG.gles2) at $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY (container-private)"
elif start_sway pixman; then
  STRAND_GPU_SWAY_RENDERER=pixman
  echo "=== headless sway (pixman; GLES2 and Vulkan did not start) at $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY (container-private)"
  echo "=== the GLES2 attempt's log:"; tail -20 "$LOG.gles2"
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
