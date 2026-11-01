#!/usr/bin/env bash
# What the image gives the jobs, printed into a job's log so a pixel mismatch
# can be traced to a version (.github/actions/setup; ci.sh steps 1 to 3).
#
# Usage, inside the image: machine.sh services|gpu|references
set -euo pipefail

case "${1:-}" in
  services)
    dbus-daemon --version | head -1
    python3 -c 'import dbusmock'
    dpkg -s python3-dbusmock | grep '^Version'
    pipewire --version
    ;;
  # Lavapipe is the only driver the loader is given (VK_DRIVER_FILES, the
  # Dockerfile); STRAND_REQUIRE_GPU=1 makes the GPU tests fail without it.
  gpu)
    dpkg -s mesa-vulkan-drivers | grep '^Version'
    out=$(vulkaninfo --summary 2>&1)
    printf '%s\n' "$out" | sed -n '/^Devices/,$p'
    printf '%s\n' "$out" | grep -Eq 'driverName += llvmpipe' || {
      echo "::error::no lavapipe (llvmpipe) Vulkan device"
      exit 1
    }
    ;;
  references)
    sway --version
    dpkg -s fonts-dejavu-core adwaita-icon-theme | grep -E '^(Package|Version)'
    # Every font fontconfig sees (text references depend on which family
    # a style or a missing glyph falls back to).
    dpkg -l 'fonts-*' | grep '^ii' || true
    fc-list : family style file | sort
    fc-match -s 'sans-serif:italic' | head -5
    # The acceptance tests pin XCURSOR_THEME=Adwaita (size 24); the
    # default theme only matters to processes outside them.
    readlink -f /usr/share/icons/default/index.theme || true
    ls /usr/share/icons/Adwaita/cursors/default
    rustc --version
    ;;
  *)
    echo "usage: machine.sh services|gpu|references" >&2
    exit 2
    ;;
esac
