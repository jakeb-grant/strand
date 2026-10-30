#!/usr/bin/env bash
# smoke.sh DIST: the installer against the release archive in DIST
# (scripts/release/package.sh's OUT) on the systems it is for and two
# it must refuse, each in a fresh container on the machine's Docker
# (CI's package job; locally from the host, never inside the dev
# container), as a user with an empty home (test-install.sh):
#
#   ubuntu:24.04 with Strand's libraries  full (and root is refused)
#   archlinux:latest with them            full
#   ubuntu:24.04 without them             nolibs
#   ubuntu:22.04 (glibc 2.35)             oldglibc
#
# The containers download their distribution's packages; the installer
# itself reads only DIST, over file://.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
DIST=$(cd "${1:?usage: smoke.sh DIST}" && pwd)
ls "$DIST"/strand-*-x86_64-linux.tar.gz >/dev/null
UBUNTU_LIBS="libpipewire-0.3-0t64 libxkbcommon0 libwayland-client0 libfontconfig1"
ARCH_LIBS="libpipewire libxkbcommon wayland fontconfig"

# case NAME IMAGE SETUP MODE: SETUP as root, then test-install.sh MODE as
# the user `u`.
failed=0
case_() {
  local name=$1 image=$2 setup=$3 mode=$4
  echo "=== $name ($image)"
  if docker run --rm -v "$DIST:/dist:ro" -v "$HERE/test-install.sh:/test-install.sh:ro" "$image" \
    bash -c "set -e; { $setup; } >/tmp/setup.log 2>&1 || { cat /tmp/setup.log; exit 1; }; grep '^ok' /tmp/setup.log || true; useradd -m u; su u -c 'cd && bash /test-install.sh $mode'"; then
    echo "=== $name: passed"
  else
    echo "=== $name: FAILED"
    failed=1
  fi
}

apt="export DEBIAN_FRONTEND=noninteractive; apt-get update -qq && apt-get install -y -qq --no-install-recommends curl ca-certificates"
# Root is refused before anything else happens.
root_refused="out=\$(sh /dist/install.sh 2>&1) && exit 1; echo \"\$out\" | grep -q 'not root' || { echo \"\$out\"; exit 1; }; echo 'ok   root is refused'"

case_ "Ubuntu 24.04" ubuntu:24.04 "$apt $UBUNTU_LIBS && $root_refused" full
case_ "Arch" archlinux:latest "pacman -Sy --noconfirm --needed curl $ARCH_LIBS" full
case_ "Ubuntu 24.04 without the libraries" ubuntu:24.04 "$apt" nolibs
case_ "Ubuntu 22.04" ubuntu:22.04 "$apt" oldglibc

[ $failed = 0 ] && echo "smoke: all passed"
exit $failed
