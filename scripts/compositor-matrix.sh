#!/usr/bin/env bash
# The compositor matrix (M3 exit: "runs on Hyprland, niri and sway"):
# starts one compositor without a display, then runs
# crates/strand/tests/compositor_matrix.rs against it (the `workspaces`,
# `windows` and `wm` stores, and design.md's bar in `strand run`,
# compared with what the compositor's own CLI reports; on labwc, which
# has no IPC, the stores against the test windows' own view).
#
#   sway      headless (WLR_BACKENDS=headless, the pixman renderer), one
#             1280x720 output.
#   niri      nested: its winit backend in a window of a headless sway
#             (Mesa's software EGL on the parent's wl_shm), 1280x720.
#   labwc     headless (the pixman renderer), one output: the compositor
#             without an IPC adapter (the standard protocols alone:
#             ext-foreign-toplevel-list, wlr-foreign-toplevel-management,
#             ext-workspace), two desktops.
#   hyprland  on a virtual KMS device (vkms) through seatd: aquamarine
#             allocates every buffer, headless outputs included, on a
#             DRM node, so it needs one; Mesa renders in software
#             (llvmpipe). $STRAND_DRM_CARD names the card
#             (/dev/dri/cardN); seatd must be running (LIBSEAT_BACKEND
#             and SEATD_SOCK are passed through).
#
# Usage: scripts/compositor-matrix.sh sway|niri|hyprland|labwc [TEST_BINARY]
#   TEST_BINARY  the built compositor_matrix test (default: built here
#                with `cargo test -p strand --test compositor_matrix
#                --no-run`).
# Env:   OUT (default target/matrix/<compositor>): logs and screenshots.
# Exit status is the test's (non-zero when the compositor never starts).

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
KIND=${1:-}
TEST=${2:-}
case "$KIND" in
  sway|niri|hyprland|labwc) ;;
  *) sed -n '2,29p' "$0"; exit 2 ;;
esac
OUT=${OUT:-$ROOT/target/matrix/$KIND}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)

if [ -z "$TEST" ]; then
  # No colour: CARGO_TERM_COLOR=always puts codes inside the line read.
  TEST=$(cd "$ROOT" && CARGO_TERM_COLOR=never cargo test -p strand --test compositor_matrix --no-run 2>&1 |
    sed -n 's/.*Executable tests\/compositor_matrix.rs (\(.*\))/\1/p' | tail -1)
  [ -n "$TEST" ] || { echo "could not build the compositor_matrix test" >&2; exit 2; }
  case "$TEST" in /*) ;; *) TEST=$ROOT/$TEST ;; esac
fi
[ -x "$TEST" ] || { echo "no test binary at $TEST" >&2; exit 2; }

RT=$(mktemp -d /tmp/strand-matrix.XXXXXX)
chmod 700 "$RT"
PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  # Hyprland logs into its instance directory and writes a crash report
  # under ~/.cache: kept with the other logs.
  if [ "$KIND" = hyprland ]; then
    for f in "$RT"/hypr/*/hyprland.log; do [ -f "$f" ] && cp "$f" "$OUT/hyprland-instance.log"; done
    for f in "${XDG_CACHE_HOME:-$HOME/.cache}"/hyprland/hyprlandCrashReport*.txt; do
      [ -f "$f" ] && cp "$f" "$OUT/hyprland-crash-$(basename "$f" .txt).log"
    done
  fi
  rm -rf "$RT"
}
trap cleanup EXIT

# Waits (at most 30 s) for the command `$2...` to succeed, failing with
# the tail of the log `$1`.
wait_for() {
  local log=$1 i p
  shift
  for i in $(seq 300); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    for p in "${PIDS[@]}"; do
      kill -0 "$p" 2>/dev/null || { echo "exited while waiting for: $*" >&2; tail -80 "$log" >&2; exit 1; }
    done
    sleep 0.1
  done
  echo "timed out waiting for: $*" >&2
  tail -80 "$log" >&2
  exit 1
}

# The first entry matching `$2` in directory `$1` (empty when none).
first() { (cd "$1" && ls -1 | grep -E "$2" | grep -v '\.lock$' | head -1) || true; }

sway_ready() {
  local ipc display
  ipc=$(first "$1" '^sway-ipc\.'); display=$(first "$1" '^wayland-')
  [ -n "$ipc" ] && [ -n "$display" ] && SWAYSOCK=$1/$ipc swaymsg -t get_version
}

niri_ready() {
  local ipc display
  ipc=$(first "$1" '^niri\..*\.sock$'); display=$(first "$1" '^wayland-')
  [ -n "$ipc" ] && [ -n "$display" ] && NIRI_SOCKET=$1/$ipc niri msg version
}

labwc_ready() { [ -n "$(first "$1" '^wayland-')" ]; }

hypr_instance() { hyprctl -j instances | sed -n "s/.*\"$1\": *\"\([^\"]*\)\".*/\\1/p" | head -1; }

hypr_ready() {
  local sig
  sig=$(hypr_instance instance)
  [ -n "$sig" ] && [ -n "$(hypr_instance wl_socket)" ] &&
    HYPRLAND_INSTANCE_SIGNATURE=$sig hyprctl -j monitors | grep -q '"name"'
}

# A headless sway in runtime dir `$1` with one output `$2`x`$3`.
start_sway() {
  local dir=$1
  mkdir -p "$dir"; chmod 700 "$dir"
  printf 'xwayland disable\ndefault_border none\noutput HEADLESS-1 resolution %sx%s position 0 0 scale 1\n' "$2" "$3" >"$dir/sway.cfg"
  env -u WAYLAND_DISPLAY -u SWAYSOCK -u DISPLAY XDG_RUNTIME_DIR="$dir" \
    WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    sway -c "$dir/sway.cfg" >"$OUT/sway.log" 2>&1 &
  PIDS+=($!)
  wait_for "$OUT/sway.log" sway_ready "$dir"
}

case "$KIND" in
  sway)
    start_sway "$RT" 1280 720
    export XDG_RUNTIME_DIR=$RT
    WAYLAND_DISPLAY=$(first "$RT" '^wayland-')
    SWAYSOCK=$RT/$(first "$RT" '^sway-ipc\.')
    export WAYLAND_DISPLAY SWAYSOCK
    unset HYPRLAND_INSTANCE_SIGNATURE NIRI_SOCKET
    export XDG_CURRENT_DESKTOP=sway
    swaymsg -t get_version
    ;;

  niri)
    start_sway "$RT/host" 1280 720
    host=$RT/host/$(first "$RT/host" '^wayland-')
    mkdir -p "$RT/niri"; chmod 700 "$RT/niri"
    cat >"$RT/niri.kdl" <<'EOF'
hotkey-overlay {
    skip-at-startup
}
animations {
    off
}
prefer-no-csd
layout {
    gaps 8
}
EOF
    env -u SWAYSOCK -u DISPLAY XDG_RUNTIME_DIR="$RT/niri" WAYLAND_DISPLAY="$host" \
      LIBGL_ALWAYS_SOFTWARE=1 RUST_LOG=niri=debug \
      niri -c "$RT/niri.kdl" >"$OUT/niri.log" 2>&1 &
    PIDS+=($!)
    wait_for "$OUT/niri.log" niri_ready "$RT/niri"
    export XDG_RUNTIME_DIR=$RT/niri
    WAYLAND_DISPLAY=$(first "$RT/niri" '^wayland-')
    NIRI_SOCKET=$RT/niri/$(first "$RT/niri" '^niri\..*\.sock$')
    export WAYLAND_DISPLAY NIRI_SOCKET
    # The test's reload: an edit of the config niri watches.
    export STRAND_MATRIX_NIRI_CONFIG=$RT/niri.kdl
    unset SWAYSOCK HYPRLAND_INSTANCE_SIGNATURE
    export XDG_CURRENT_DESKTOP=niri
    niri msg version
    ;;

  hyprland)
    [ -n "${STRAND_DRM_CARD:-}" ] || { echo "STRAND_DRM_CARD is not set (a vkms card for aquamarine)" >&2; exit 2; }
    export XDG_RUNTIME_DIR=$RT
    # Its status apart (it aborted in run 206): the version is read from
    # whatever it printed.
    Hyprland --version >"$OUT/hyprland-version.log" 2>&1 || echo "Hyprland --version: exit $?" >&2
    sed -n 1,5p "$OUT/hyprland-version.log"
    version=$(sed -n 's/^Hyprland \([0-9]*\)\.\([0-9]*\).*/\1 \2/p' "$OUT/hyprland-version.log" | sed -n 1p)
    read -r major minor <<<"${version:-0 0}"
    echo "Hyprland $major.$minor"
    # 0.55 moved the configuration to Lua (hyprland.lua); before it,
    # hyprlang (hyprland.conf).
    if [ "$major" -gt 0 ] || [ "$minor" -ge 55 ]; then
      cfg=$RT/hyprland.lua
      cat >"$cfg" <<'EOF'
hl.monitor({ output = "", mode = "1280x720@60", position = "0x0", scale = 1 })
hl.config({
    animations = { enabled = false },
    misc = { disable_hyprland_logo = true, disable_splash_rendering = true, force_default_wallpaper = 0 },
    ecosystem = { no_update_news = true, no_donation_nag = true },
    debug = { disable_logs = false, enable_stdout_logs = true, suppress_errors = true },
})
EOF
    else
      cfg=$RT/hyprland.conf
      cat >"$cfg" <<'EOF'
monitor = , 1280x720@60, 0x0, 1
animations {
    enabled = false
}
misc {
    disable_hyprland_logo = true
    disable_splash_rendering = true
    force_default_wallpaper = 0
}
ecosystem {
    no_update_news = true
    no_donation_nag = true
}
debug {
    disable_logs = false
    enable_stdout_logs = true
    suppress_errors = true
}
EOF
    fi
    env -u WAYLAND_DISPLAY -u SWAYSOCK -u DISPLAY -u HYPRLAND_INSTANCE_SIGNATURE \
      AQ_DRM_DEVICES="$STRAND_DRM_CARD" LIBGL_ALWAYS_SOFTWARE=1 GBM_ALWAYS_SOFTWARE=1 \
      AQ_TRACE=1 HYPRLAND_TRACE=1 \
      Hyprland --config "$cfg" >"$OUT/hyprland.log" 2>&1 &
    PIDS+=($!)
    wait_for "$OUT/hyprland.log" hypr_ready
    HYPRLAND_INSTANCE_SIGNATURE=$(hypr_instance instance)
    WAYLAND_DISPLAY=$(hypr_instance wl_socket)
    export HYPRLAND_INSTANCE_SIGNATURE WAYLAND_DISPLAY
    unset SWAYSOCK NIRI_SOCKET
    export XDG_CURRENT_DESKTOP=Hyprland
    hyprctl version | sed -n 1,3p
    hyprctl -j monitors | sed -n 1,20p
    # A config mistake draws Hyprland's error bar where the bar is read:
    # fail on it here, with the errors, not later on a pixel.
    errors=$(hyprctl configerrors 2>&1 || true)
    case "$errors" in
      *"unknown request"*) echo "hyprctl configerrors: $errors" ;;
      *[![:space:]]*) echo "Hyprland reports config errors:" >&2; echo "$errors" >&2; exit 1 ;;
    esac
    # The raw reply to a dispatch in the dialect the config selects, for
    # the record (the adapter's success test, hyprland.rs dispatch_reply).
    active=$(hyprctl -j activeworkspace | sed -n 's/^ *"name": *"\([^"]*\)".*/\1/p' | sed -n 1p)
    if [ "${cfg##*.}" = lua ]; then
      reply=$(hyprctl dispatch "hl.dsp.focus({ workspace = \"$active\" })" 2>&1 || true)
    else
      reply=$(hyprctl dispatch workspace "name:$active" 2>&1 || true)
    fi
    echo "Hyprland's reply to a dispatch (${cfg##*.} config): '$reply'"
    ;;

  labwc)
    mkdir -p "$RT/labwc"
    cat >"$RT/labwc/rc.xml" <<'EOF'
<?xml version="1.0"?>
<labwc_config>
  <desktops number="2">
    <popupTime>0</popupTime>
    <names>
      <name>1</name>
      <name>2</name>
    </names>
  </desktops>
</labwc_config>
EOF
    : >"$RT/labwc/autostart"
    env -u WAYLAND_DISPLAY -u SWAYSOCK -u DISPLAY -u HYPRLAND_INSTANCE_SIGNATURE -u NIRI_SOCKET \
      XDG_RUNTIME_DIR="$RT" WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_RENDERER=pixman \
      WLR_LIBINPUT_NO_DEVICES=1 \
      labwc -C "$RT/labwc" >"$OUT/labwc.log" 2>&1 &
    PIDS+=($!)
    wait_for "$OUT/labwc.log" labwc_ready "$RT"
    export XDG_RUNTIME_DIR=$RT
    WAYLAND_DISPLAY=$(first "$RT" '^wayland-')
    export WAYLAND_DISPLAY
    # No IPC: nothing for the adapters' detection to find.
    unset SWAYSOCK NIRI_SOCKET HYPRLAND_INSTANCE_SIGNATURE
    export XDG_CURRENT_DESKTOP=labwc
    labwc --version || true
    ;;
esac

# The bare desktop, for the record.
sleep 1
grim "$OUT/$KIND-desktop.png" || echo "grim failed on the bare $KIND desktop" >&2

status=0
STRAND_MATRIX=$KIND STRAND_SHOTS=$OUT "$TEST" --test-threads=1 --nocapture 2>&1 |
  tee "$OUT/test.log" || status=${PIPESTATUS[0]}
echo "compositor matrix on $KIND: exit $status (logs and shots in $OUT)"
exit "$status"
