#!/usr/bin/env bash
# The M3 report's screenshots (docs/m3-report.md, docs/images/m3-*.png):
# design.md's full shell (theme, bar, launcher, toasts, OSD; the fixtures
# byte for byte) in `strand run` on headless sway with the real services,
# no STRAND_MOCK. The backends are those of crates/strand/tests/budgets.rs:
# python-dbusmock's UPower, NetworkManager and logind and a mock portal on
# a private bus, the shell's own notifications server, sway's IPC and a
# private PipeWire with WirePlumber. HEADLESS-1 (2560x1440 at scale 2) is
# saved at each step of `full_shell`, whose checks all still run:
#
#   m3-bar.png       the bar: sway's workspaces, the test window's title,
#                    the clock, volume, network and battery (design.md's
#                    bar has no network: the shots add a two-line
#                    component with its icon and name after the volume)
#   m3-launcher.png  the launcher listing the machine's desktop entries
#   m3-toasts.png    two toasts sent over D-Bus to the notifications server
#   m3-osd.png       the OSD raised by `wpctl set-volume` on PipeWire
#   m3-shell.png     the whole output with all four up
#
# Usage: scripts/m3-shots.sh [out dir]   (default: docs/images)
# Needs sway, grim, dbus-daemon, python3-dbusmock (STRAND_DBUSMOCK_PYTHON
# picks the interpreter), pipewire and wireplumber.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=$(realpath -m "${1:-$ROOT/docs/images}")
mkdir -p "$OUT"
cd "$ROOT"
STRAND_SHOTS_DIR="$OUT" STRAND_REQUIRE_SWAY=1 STRAND_REQUIRE_DBUS=1 STRAND_REQUIRE_PIPEWIRE=1 \
  cargo test --release -p strand --test budgets the_m3_screenshots -- --ignored --nocapture --test-threads=1
ls -l "$OUT"/m3-*.png
