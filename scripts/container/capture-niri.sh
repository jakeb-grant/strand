#!/usr/bin/env bash
# Captures a real niri's IPC traffic for the fixture
# crates/strand-services/tests/fixtures/niri-26.04-captured: niri nested
# in a headless sway (as scripts/compositor-matrix.sh starts it) inside
# the matrix image (Dockerfile.matrix), three foot windows, a scripted
# scenario (capture-niri-session.sh), and raw replies and the event
# stream read with socat, requests written exactly as the adapter
# writes them.
#
# Usage: scripts/container/capture-niri.sh [OUT]   (default target/niri-capture)
# OUT gets events-stamped.txt and marks-stamped.txt (one timestamped line
# each), the replies per checkpoint (boot/ opened/ moved/ closed/ end/)
# and three raw action replies. The fixture's events.txt groups the
# events by the action whose timestamp precedes them; see its SOURCE.txt.

set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=${1:-$ROOT/target/niri-capture}
rm -rf "$OUT"; mkdir -p "$OUT"; OUT=$(cd "$OUT" && pwd)
hash=$(sha256sum "$HERE/Dockerfile.matrix" | cut -c1-12)
IMAGE=strand-matrix:$hash
docker image inspect "$IMAGE" >/dev/null 2>&1 ||
  docker build --pull -t "$IMAGE" -f "$HERE/Dockerfile.matrix" "$HERE"
docker run --rm --init -v "$HERE:/work:ro" -v "$OUT:/out" -e HOST_UID="$(id -u)" "$IMAGE" bash -c '
  set -uo pipefail
  sed -i "s/^CheckSpace/#CheckSpace/" /etc/pacman.conf
  pacman -Sy --noconfirm --needed foot socat jq >/tmp/pacman.log 2>&1 || { tail -20 /tmp/pacman.log; exit 1; }
  for bin in /usr/bin/sway /usr/bin/niri; do setcap -r "$bin" 2>/dev/null || true; done
  useradd -m -u "$HOST_UID" cap 2>/dev/null || true
  chown -R "$HOST_UID" /out
  pacman -Q niri sway foot
  runuser -u "$(id -nu "$HOST_UID")" -- env HOME="/home/$(id -nu "$HOST_UID")" bash /work/capture-niri-session.sh'
echo "captured into $OUT"
