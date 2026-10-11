#!/usr/bin/env bash
# M5 wave-0 spike 3 (decisions.md m5-infra), inside run.sh's image: each
# scenario in a fresh private headless sway (pixman, as the tests), once
# with sway's default focus_on_window_activation (urgent) and once with
# `focus_on_window_activation focus`. Never the host session.
# Usage: inside.sh BIN OUTDIR
set -uo pipefail
BIN=$1 OUT=$2
mkdir -p "$OUT"
sway --version
unset WAYLAND_DISPLAY DISPLAY SWAYSOCK
for mode in default focus; do
  for sc in layer bogus none launch; do
    dir=$(mktemp -d /tmp/spike-xdg.XXXXXX); chmod 0700 "$dir"
    export XDG_RUNTIME_DIR=$dir
    { echo "xwayland disable"; echo "output HEADLESS-1 resolution 1280x720"
      [ "$mode" = focus ] && echo "focus_on_window_activation focus"; } >"$dir/sway.conf"
    unset WAYLAND_DISPLAY SWAYSOCK
    WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
      sway -c "$dir/sway.conf" -d >"$OUT/sway-$mode-$sc.log" 2>&1 &
    pid=$!
    for _ in $(seq 100); do
      for s in "$dir"/wayland-*; do case $s in *.lock|*'*'*) ;; *) [ -S "$s" ] && export WAYLAND_DISPLAY=${s##*/};; esac; done
      for s in "$dir"/sway-ipc.*.sock; do [ -S "$s" ] && export SWAYSOCK=$s; done
      [ -n "${WAYLAND_DISPLAY:-}" ] && [ -n "${SWAYSOCK:-}" ] && break
      kill -0 $pid 2>/dev/null || { echo "sway exited:"; tail -5 "$OUT/sway-$mode-$sc.log"; break; }
      sleep 0.1
    done
    echo
    echo "############ focus_on_window_activation=$mode scenario=$sc"
    timeout 30 "$BIN" "$sc" 2>&1; echo "exit $?"
    grep -a -iE 'activation|token' "$OUT/sway-$mode-$sc.log" | grep -av 'Registered' | head -8
    kill $pid 2>/dev/null; wait $pid 2>/dev/null
    while kill -0 $pid 2>/dev/null; do sleep 0.1; done
    rm -rf "$dir"
  done
done
