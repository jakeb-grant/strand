#!/usr/bin/env bash
# Inside run.sh's (or gpu.sh's) container: a private headless sway with
# the pixman renderer (CI's setup), then each spike mode, with a grim
# shot of the layer surface. Never the host session.
# Usage: run-spike.sh BIN OUTDIR [MODES...]
set -uo pipefail
BIN=$1 OUT=$2; shift 2
MODES=${*:-present readback mem}
mkdir -p "$OUT"
unset WAYLAND_DISPLAY DISPLAY
export XDG_RUNTIME_DIR; XDG_RUNTIME_DIR=$(mktemp -d /tmp/spike-xdg.XXXXXX); chmod 0700 "$XDG_RUNTIME_DIR"
printf 'output HEADLESS-1 resolution 640x360\n' >"$XDG_RUNTIME_DIR/sway.conf"
WLR_BACKENDS=headless WLR_RENDERER=${SPIKE_RENDERER:-pixman} WLR_LIBINPUT_NO_DEVICES=1 \
  sway -c "$XDG_RUNTIME_DIR/sway.conf" >"$OUT/sway.log" 2>&1 &
SWAY=$!
trap 'kill $SWAY 2>/dev/null; wait; rm -rf "$XDG_RUNTIME_DIR"' EXIT
for _ in $(seq 100); do
  for s in "$XDG_RUNTIME_DIR"/wayland-*; do case $s in *.lock|*'*'*) ;; *) [ -S "$s" ] && export WAYLAND_DISPLAY=${s##*/};; esac; done
  [ -n "${WAYLAND_DISPLAY:-}" ] && break; sleep 0.1
done
echo "sway (${SPIKE_RENDERER:-pixman}) at $WAYLAND_DISPLAY"
vulkaninfo --summary 2>/dev/null | sed -n '/^Devices/,$p' | grep -E 'deviceName|deviceType|driverName|driverInfo'
for m in $MODES; do
  echo "=== mode $m"
  SPIKE_HOLD_MS=1500 timeout 120 "$BIN" "$m" 60 &
  pid=$!
  if [ "$m" != mem ]; then sleep 1.2; grim -g "0,0 256x128" "$OUT/$m.png" && echo "grim: $OUT/$m.png"; fi
  wait $pid; echo "exit $?"
done
