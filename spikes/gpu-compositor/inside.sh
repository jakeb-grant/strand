#!/usr/bin/env bash
# M5 wave-0 spike 1 (decisions.md m5-infra): inside gpu.sh's container
# (only /dev/dri/renderD128 passed in), start a private headless sway
# with WLR_RENDERER=gles2, then vulkan, on the render node, and record
# for each whether it starts, which renderer it ends up with, whether it
# offers zwp_linux_dmabuf_v1 (what a hardware Vulkan WSI needs to
# present), and whether vkcube on the hardware ICD presents to it.
# Never the host session: a fresh XDG_RUNTIME_DIR, no host socket.
#
# Usage (from run.sh in this directory): inside.sh OUTDIR
set -uo pipefail

OUT=$1
mkdir -p "$OUT"
[ "${STRAND_GPU_HARDWARE:-}" = 1 ] || { echo "inside.sh runs inside gpu.sh's container" >&2; exit 2; }
for dev in /dev/dri/*; do
  case "$dev" in */card*) echo "inside.sh: $dev is in the container; only the render node may be" >&2; exit 2 ;; esac
done
ls -l /dev/dri | tee "$OUT/dev-dri.txt"

{
  echo "== packages"
  dpkg -s sway libwlroots-0.17-dev libwlroots12 2>/dev/null | grep -E '^(Package|Version)' || true
  dpkg -l | awk '/^ii/ && /(mesa|libegl|libgles|libgbm|libdrm|wlroots|libvulkan)/ {print $2, $3}'
  echo "== vulkan"
  vulkaninfo --summary 2>&1 | sed -n '/^Devices/,$p'
  echo "== EGL vendors"
  ls /usr/share/glvnd/egl_vendor.d/ 2>/dev/null
  echo "== DRI drivers"
  ls /usr/lib/x86_64-linux-gnu/dri/ 2>/dev/null | head -50
} >"$OUT/env.txt" 2>&1
cat "$OUT/env.txt"

unset WAYLAND_DISPLAY DISPLAY
SWAY_PID=
try() { # RENDERER
  local r=$1 i sock
  local dir; dir=$(mktemp -d /tmp/spike-xdg.XXXXXX); chmod 0700 "$dir"
  export XDG_RUNTIME_DIR=$dir
  printf 'output HEADLESS-1 resolution 1280x720\n' >"$dir/sway.conf"
  echo
  echo "=================== WLR_RENDERER=$r"
  WLR_BACKENDS=headless WLR_RENDERER=$r WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128 \
    WLR_LIBINPUT_NO_DEVICES=1 sway -c "$dir/sway.conf" -d >"$OUT/sway-$r.log" 2>&1 &
  SWAY_PID=$!
  unset WAYLAND_DISPLAY
  for i in $(seq 100); do
    for sock in "$dir"/wayland-*; do
      case "$sock" in *.lock | *'*'*) continue ;; esac
      [ -S "$sock" ] && export WAYLAND_DISPLAY=${sock##*/}
    done
    [ -n "${WAYLAND_DISPLAY:-}" ] && break
    kill -0 "$SWAY_PID" 2>/dev/null || break
    sleep 0.1
  done
  if [ -z "${WAYLAND_DISPLAY:-}" ]; then
    echo "RESULT $r: sway did not start"
  else
    echo "RESULT $r: sway started at $WAYLAND_DISPLAY"
    # The globals sway advertises: WAYLAND_DEBUG on a client that only
    # binds the registry (vkcube's own run lists them too).
    WAYLAND_DEBUG=1 timeout 20 vkcube-wayland --c 60 >"$OUT/vkcube-$r.out" 2>"$OUT/vkcube-$r.err"
    local st=$?
    grep -o 'wl_registry@[0-9]*\.global([^)]*)' "$OUT/vkcube-$r.err" | sed 's/.*global(//; s/)$//' | sort -u >"$OUT/globals-$r.txt"
    if grep -q zwp_linux_dmabuf_v1 "$OUT/globals-$r.txt"; then
      echo "RESULT $r: zwp_linux_dmabuf_v1 advertised"
    else
      echo "RESULT $r: zwp_linux_dmabuf_v1 NOT advertised"
    fi
    if [ "$st" = 0 ]; then echo "RESULT $r: vkcube presented 60 frames on the hardware ICD"; else echo "RESULT $r: vkcube failed (exit $st): $(grep -v '^\[' "$OUT/vkcube-$r.err" | grep -iv 'wl_\|zwp_\|xdg_\|wp_' | tail -3 | tr '\n' ' ')"; fi
  fi
  echo "--- renderer lines in sway-$r.log:"
  grep -a -iE 'renderer|render|egl|gles|vulkan|pixman|gbm|drm|dmabuf|failed|error' "$OUT/sway-$r.log" | grep -av 'DEBUG.*keyboard' | head -40
  kill "$SWAY_PID" 2>/dev/null; wait "$SWAY_PID" 2>/dev/null; SWAY_PID=
  rm -rf "$dir"
}
try gles2
try vulkan
try pixman
