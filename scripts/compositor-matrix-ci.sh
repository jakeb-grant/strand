#!/usr/bin/env bash
# The CI job `compositors` (.github/workflows/ci.yml), inside an
# archlinux:latest container (Hyprland and niri are packaged there, not
# on the Ubuntu runner): installs sway, niri, Hyprland, grim, seatd and
# Mesa, then runs scripts/compositor-matrix.sh for each compositor as an
# unprivileged user (Hyprland refuses root) with the test binary built on
# the runner (the same path: the repository is mounted where the runner
# checked it out).
#
# Usage (as root): scripts/compositor-matrix-ci.sh TEST_BINARY
# Env:   STRAND_DRM_CARD  the vkms card passed in with --device (Hyprland)
#        MATRIX           the compositors (default "sway niri hyprland")
# Exit status is non-zero when any compositor failed; logs and shots are
# in target/matrix/<compositor>.

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TEST=${1:?the compositor_matrix test binary}
MATRIX=${MATRIX:-sway niri hyprland}

# pacman's free-space check cannot read an overlayfs root's mount points
# in a container ("could not determine filesystem mount points"): off.
sed -i 's/^CheckSpace/#CheckSpace/' /etc/pacman.conf
# The image's keyring, should it ship uninitialised.
pacman-key --init >/dev/null 2>&1 || true
pacman-key --populate archlinux >/dev/null 2>&1 || true
pacman -Syu --noconfirm --needed \
  sway niri hyprland grim seatd mesa dbus ttf-dejavu adwaita-icon-theme \
  fontconfig libxkbcommon wayland pipewire libcap >/tmp/pacman.log 2>&1 ||
  { tail -40 /tmp/pacman.log; exit 1; }
pacman -Q sway niri hyprland grim seatd mesa
# sway is installed with file capabilities (cap_sys_nice, for its
# realtime scheduling); Docker's bounding set lacks them, so exec(2) of
# it fails with EPERM ("env: 'sway': Operation not permitted", run 206).
# Off for every compositor: none needs them headless.
for bin in /usr/bin/sway /usr/bin/niri /usr/bin/Hyprland /usr/bin/Hyprland-bin; do
  [ -e "$bin" ] || continue
  if [ -n "$(getcap "$bin" 2>/dev/null)" ]; then
    echo "dropping $(getcap "$bin")"
    setcap -r "$bin"
  fi
done
fc-match sans-serif

# The runner's user, so the logs and shots under target/ are its own.
uid=$(stat -c %u "$ROOT")
[ "$uid" != 0 ] || uid=1000
useradd -m -u "$uid" strand 2>/dev/null || true
user=$(id -nu "$uid")
mkdir -p "$ROOT/target/matrix"
chown "$uid" "$ROOT/target/matrix"

# seatd hands the KMS card to Hyprland's libseat (no logind here).
if [ -n "${STRAND_DRM_CARD:-}" ]; then
  ls -l /dev/dri /sys/class/drm/
  for c in /sys/class/drm/card[0-9]; do echo "$c: $(cat "$c/device/uevent" 2>/dev/null | tr '\n' ' ')"; done
  SEATD_VTBOUND=0 seatd -u "$user" -g "$(id -gn "$user")" -l info >"$ROOT/target/matrix/seatd.log" 2>&1 &
  for _ in $(seq 100); do [ -S /run/seatd.sock ] && break; sleep 0.1; done
  [ -S /run/seatd.sock ] || { echo "seatd did not start"; cat "$ROOT/target/matrix/seatd.log"; }
fi

failed=()
for kind in $MATRIX; do
  echo "::group::$kind"
  runuser -u "$user" -- env \
    HOME="/home/$user" USER="$user" \
    STRAND_DRM_CARD="${STRAND_DRM_CARD:-}" LIBSEAT_BACKEND=seatd SEATD_SOCK=/run/seatd.sock \
    OUT="$ROOT/target/matrix/$kind" \
    "$ROOT/scripts/compositor-matrix.sh" "$kind" "$TEST"
  status=$?
  echo "::endgroup::"
  if [ "$status" != 0 ]; then
    echo "::error::the compositor matrix failed on $kind (exit $status)"
    for log in "$ROOT/target/matrix/$kind"/*.log; do
      [ -f "$log" ] || continue
      case "$log" in
        *-crash-*) echo "--- head of $log"; sed -n 1,70p "$log" ;;
        *) echo "--- tail of $log"; tail -80 "$log" ;;
      esac
    done
    failed+=("$kind")
  fi
done

# Where the bar's click went untested (no zwlr_virtual_pointer_manager_v1).
for kind in $MATRIX; do
  if grep -q 'the click is not tested here' "$ROOT/target/matrix/$kind/test.log" 2>/dev/null; then
    echo "::warning::the bar's click (ws.focus() from a pointer) was not tested on $kind: no virtual pointer"
  fi
done

if [ "${#failed[@]}" -gt 0 ]; then
  echo "failed: ${failed[*]}"
  exit 1
fi
echo "the compositor matrix passed: $MATRIX"
