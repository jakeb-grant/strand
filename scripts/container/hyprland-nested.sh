#!/usr/bin/env bash
# A command against a real Hyprland on this machine: Hyprland nested in a
# headless labwc, in the matrix image (Dockerfile.matrix) with Vulkan drivers
# added and only /dev/dri/renderD128 passed in. No KMS card, so it runs where
# matrix.sh skips Hyprland. labwc because Hyprland's Wayland backend needs
# xdg_wm_base 6, which sway and weston do not offer.
#
# The command runs with WAYLAND_DISPLAY and HYPRLAND_INSTANCE_SIGNATURE set and
# the checkout at its own path; build with run.sh first. Hyprland's monitors
# are headless outputs (`hyprctl output create headless NAME`, HYPR_OUTPUTS
# of them at the start). They are not real monitors: `hl.dsp.dpms` does not
# stop their frames, so a sleeping output cannot be shown this way.
#
# Usage: scripts/container/hyprland-nested.sh <command> [args...], e.g.
#   scripts/container/hyprland-nested.sh target/container/debug/strand run examples/bar
# Env:   HYPR_OUTPUTS (default 2). MATRIX_REBUILD=1 refreshes the image.
#        Logs: target/hyprland-nested/{labwc,hyprland}.log.

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
RENDER=/dev/dri/renderD128
[ "$#" -gt 0 ] || { sed -n '15,17p' "$0" >&2; exit 2; }

base=strand-matrix:$(sha256sum "$HERE/Dockerfile.matrix" | cut -c1-12)
image=strand-hyprland-nested:$(sha256sum "$HERE/Dockerfile.matrix" "$0" | sha256sum | cut -c1-12)
if [ "${MATRIX_REBUILD:-}" = 1 ] || ! docker image inspect "$image" >/dev/null 2>&1; then
  docker image inspect "$base" >/dev/null 2>&1 || MATRIX_REBUILD=1
  [ "${MATRIX_REBUILD:-}" != 1 ] || docker build --pull -t "$base" -f "$HERE/Dockerfile.matrix" "$HERE"
  # setcap -r: Docker's bounding set lacks the file capabilities they carry,
  # so exec(2) of them fails (compositor-matrix-ci.sh does the same).
  docker build -t "$image" - <<EOF
FROM $base
RUN pacman -Syu --noconfirm --needed jq vulkan-intel vulkan-radeon vulkan-swrast \\
 && pacman -Scc --noconfirm >/dev/null \\
 && for b in /usr/bin/Hyprland /usr/bin/Hyprland-bin /usr/bin/labwc; do \\
      [ ! -e "\$b" ] || setcap -r "\$b" 2>/dev/null || true; done
EOF
fi

logs=$ROOT/target/hyprland-nested
mkdir -p "$logs"
# shellcheck disable=SC2016 # Expanded in the container.
exec docker run --rm --init --user "$(id -u):$(id -g)" --group-add "$(stat -c %g "$RENDER")" \
  --device "$RENDER" -v "$ROOT:$ROOT" -w "$ROOT" -e LOGS="$logs" -e OUTPUTS="${HYPR_OUTPUTS:-2}" \
  "$image" bash -c '
set -euo pipefail
export HOME=/tmp/home XDG_RUNTIME_DIR=/tmp/run
mkdir -p "$HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

WLR_BACKENDS=headless WLR_RENDERER=gles2 WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128 \
  WLR_LIBINPUT_NO_DEVICES=1 labwc >"$LOGS/labwc.log" 2>&1 &
for _ in $(seq 100); do [ -S "$XDG_RUNTIME_DIR/wayland-0" ] && break; sleep 0.1; done

cat >"$XDG_RUNTIME_DIR/hyprland.lua" <<H
hl.monitor({ output = "", mode = "1280x720@60", position = "auto", scale = 1 })
hl.config({
    animations = { enabled = false },
    misc = { disable_hyprland_logo = true, disable_splash_rendering = true },
    ecosystem = { no_update_news = true, no_donation_nag = true },
})
H
WAYLAND_DISPLAY=wayland-0 Hyprland --config "$XDG_RUNTIME_DIR/hyprland.lua" >"$LOGS/hyprland.log" 2>&1 &
for _ in $(seq 150); do
  HYPRLAND_INSTANCE_SIGNATURE=$(hyprctl -j instances 2>/dev/null | jq -r ".[0].instance // empty" 2>/dev/null) && [ -n "$HYPRLAND_INSTANCE_SIGNATURE" ] && break
  sleep 0.2
done
[ -n "$HYPRLAND_INSTANCE_SIGNATURE" ] || { echo "hyprland-nested: Hyprland did not start; see $LOGS/hyprland.log" >&2; exit 1; }
export HYPRLAND_INSTANCE_SIGNATURE
export WAYLAND_DISPLAY=$(hyprctl -j instances | jq -r ".[0].wl_socket")
# It starts with no monitor at all, and `-j monitors` is not JSON until it has one.
for n in $(seq "$OUTPUTS"); do hyprctl output create headless "NESTED-$n" >/dev/null; done
for _ in $(seq 50); do
  [ "$(hyprctl -j monitors 2>/dev/null | jq length 2>/dev/null || echo 0)" = "$OUTPUTS" ] && break
  sleep 0.2
done
[ "$(hyprctl -j monitors | jq length)" = "$OUTPUTS" ] || { echo "hyprland-nested: no $OUTPUTS monitors" >&2; exit 1; }
exec "$@"
' hyprland-nested "$@"
