#!/usr/bin/env bash
# The CI job `compositors` (.github/workflows/ci.yml), inside an
# archlinux:latest container (Hyprland and niri are packaged there, not
# on the Ubuntu runner): installs sway, niri, Hyprland, labwc, grim, seatd and
# Mesa, then runs scripts/compositor-matrix.sh for each compositor as an
# unprivileged user (Hyprland refuses root) with the test binary built on
# the runner (the same path: the repository is mounted where the runner
# checked it out).
#
# Usage (as root): scripts/compositor-matrix-ci.sh TEST_BINARY
# Env:   STRAND_DRM_CARD  the vkms card passed in with --device (Hyprland)
#        MATRIX_OUTPUTS   outputs for sway and Hyprland (default 2; niri 1)
#        MATRIX           the compositors (default "sway niri hyprland labwc")
# The bar's click is required (STRAND_MATRIX_REQUIRE_CLICK=1): a
# compositor without zwlr_virtual_pointer_manager_v1 fails, not skips it.
# Exit status is non-zero when any compositor failed; logs and shots are
# in target/matrix/<compositor>. A failure is reported with the
# compositor's package version (a GitHub annotation), and
# target/matrix/summary.md lists each compositor's version and result
# (the job summary): the nightly run says which release broke what.

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TEST=${1:?the compositor_matrix test binary}
MATRIX=${MATRIX:-sway niri hyprland labwc}

# pacman's free-space check cannot read an overlayfs root's mount points
# in a container ("could not determine filesystem mount points"): off.
sed -i 's/^CheckSpace/#CheckSpace/' /etc/pacman.conf
# The image's keyring, should it ship uninitialised.
pacman-key --init >/dev/null 2>&1 || true
pacman-key --populate archlinux >/dev/null 2>&1 || true
pacman -Syu --noconfirm --needed \
  sway niri hyprland labwc grim seatd mesa dbus ttf-dejavu adwaita-icon-theme \
  fontconfig libxkbcommon wayland pipewire libcap >/tmp/pacman.log 2>&1 ||
  { tail -40 /tmp/pacman.log; exit 1; }
pacman -Q sway niri hyprland labwc grim seatd mesa
# The package version of a compositor of the matrix, for its result line.
version_of() { pacman -Q "$1" 2>/dev/null | cut -d' ' -f2; }
# sway is installed with file capabilities (cap_sys_nice, for its
# realtime scheduling); Docker's bounding set lacks them, so exec(2) of
# it fails with EPERM ("env: 'sway': Operation not permitted", run 206).
# Off for every compositor: none needs them headless.
for bin in /usr/bin/sway /usr/bin/niri /usr/bin/Hyprland /usr/bin/Hyprland-bin /usr/bin/labwc; do
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
  # Hyprland's renderer opens the card by path for GBM (vkms has no render
  # node), besides seatd's handle: the user needs it read-write (inside
  # the container it belongs to the host's video gid, run 37652552567).
  chmod a+rw /dev/dri/card*
  ls -l /dev/dri /sys/class/drm/
  for c in /sys/class/drm/card[0-9]; do echo "$c: $(cat "$c/device/uevent" 2>/dev/null | tr '\n' ' ')"; done
  SEATD_VTBOUND=0 seatd -u "$user" -g "$(id -gn "$user")" -l info >"$ROOT/target/matrix/seatd.log" 2>&1 &
  for _ in $(seq 100); do [ -S /run/seatd.sock ] && break; sleep 0.1; done
  [ -S /run/seatd.sock ] || { echo "seatd did not start"; cat "$ROOT/target/matrix/seatd.log"; }
fi

failed=()
summary="$ROOT/target/matrix/summary.md"
{
  echo "### Compositor matrix (archlinux:latest)"
  echo
  echo "| compositor | package version | result |"
  echo "|---|---|---|"
} >"$summary"
for kind in $MATRIX; do
  version=$(version_of "$kind")
  version=${version:-unknown}
  echo "::group::$kind"
  runuser -u "$user" -- env \
    HOME="/home/$user" USER="$user" \
    STRAND_DRM_CARD="${STRAND_DRM_CARD:-}" LIBSEAT_BACKEND=seatd SEATD_SOCK=/run/seatd.sock \
    STRAND_MATRIX_REQUIRE_CLICK=1 MATRIX_OUTPUTS="${MATRIX_OUTPUTS:-2}" \
    OUT="$ROOT/target/matrix/$kind" \
    "$ROOT/scripts/compositor-matrix.sh" "$kind" "$TEST"
  status=$?
  echo "::endgroup::"
  if [ "$status" != 0 ]; then
    echo "::error title=compositors: $kind $version::the compositor matrix failed on $kind $version (archlinux:latest, exit $status)"
    for log in "$ROOT/target/matrix/$kind"/*.log; do
      [ -f "$log" ] || continue
      case "$log" in
        *-crash-*) echo "--- head of $log"; sed -n 1,70p "$log" ;;
        *) echo "--- tail of $log"; tail -80 "$log" ;;
      esac
    done
    failed+=("$kind $version")
    echo "| $kind | $version | FAILED (exit $status) |" >>"$summary"
  else
    echo "| $kind | $version | passed |" >>"$summary"
  fi
done
chown "$uid" "$summary" 2>/dev/null || true

if [ "${#failed[@]}" -gt 0 ]; then
  echo "failed: $(IFS=,; echo "${failed[*]}")"
  exit 1
fi
echo "the compositor matrix passed: $MATRIX"
