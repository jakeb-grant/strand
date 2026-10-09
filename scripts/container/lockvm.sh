#!/usr/bin/env bash
# The lock screen's VM tier: runs a command as root in a QEMU guest
# (Ubuntu 24.04 with a real PAM stack, headless sway and a test user;
# scripts/container/Dockerfile.lockvm) inside a Docker container, and
# exits with its status.
#
# Usage: scripts/container/lockvm.sh [CMD [ARGS...]]
#        e.g. scripts/container/lockvm.sh bash scripts/lockvm/scenarios/all.sh
#        Without a command it runs the smoke (lockvm/smoke.sh): log in as
#        the test user, sway --version, a headless sway answering
#        swaymsg, pam_unix accepting the right password and refusing a
#        wrong one through /etc/pam.d/strand.
# Guest: the checkout is shared (9p) at its own path, read-write, and is
#        CMD's working directory; build what it runs first with
#        scripts/container/run.sh (target/container: the same Ubuntu
#        release, so the binaries run in the guest). The test user is
#        `tester` (uid 1000, password "strand-test", /run/user/1000);
#        /etc/pam.d/strand includes common-auth and common-account. The
#        guest's disk is a snapshot: writes outside the checkout vanish
#        with it, so a scenario may replace /etc/pam.d files freely.
#        No network. Write results under target/lockvm/.
# Env:   LOCKVM_TCG=1 software emulation when there is no /dev/kvm (slow;
#        without it a missing /dev/kvm fails); LOCKVM_TIMEOUT (seconds,
#        default 600); LOCKVM_MEM (default 2G); LOCKVM_CPUS (default 4);
#        LOCKVM_REBUILD=1 rebuilds the image.
# Exit:  CMD's status; 125 when the guest stopped without one (a crash,
#        a kernel panic or the timeout).
#
# The container gets /dev/kvm (decisions.md m4-owner: owner-approved for
# this container only) and nothing else from the host: no /dev/dri, no
# Wayland or D-Bus socket, no network in the guest. QEMU runs as the
# host user.

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)

devices=()
if [ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
  devices=(--device /dev/kvm)
elif [ "${LOCKVM_TCG:-}" = 1 ]; then
  echo "lockvm.sh: no usable /dev/kvm; LOCKVM_TCG=1, emulating in software (slow)" >&2
else
  echo "lockvm.sh: FAILED: /dev/kvm is missing or not read-writable by $(id -un)." >&2
  ls -l /dev/kvm >&2 2>/dev/null || true
  echo "lockvm.sh: LOCKVM_TCG=1 emulates in software instead (much slower)." >&2
  exit 1
fi

hash=$(cat "$HERE/Dockerfile.lockvm" "$HERE"/lockvm/* | sha256sum | cut -c1-12)
IMAGE=strand-lockvm:$hash
if [ "${LOCKVM_REBUILD:-}" = 1 ] || ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "building $IMAGE" >&2
  docker build -t "$IMAGE" -f "$HERE/Dockerfile.lockvm" "$HERE" >&2 || exit 1
fi

[ "$#" -gt 0 ] || set -- bash scripts/container/lockvm/smoke.sh

mkdir -p "$ROOT/target/lockvm"
tty=()
[ -t 0 ] && [ -t 1 ] && tty=(-t)
exec docker run --rm --init "${tty[@]}" \
  --user "$(id -u):$(id -g)" \
  "${devices[@]}" \
  -v "$ROOT:$ROOT" \
  -e LOCKVM_TCG="${LOCKVM_TCG:-}" -e LOCKVM_TIMEOUT="${LOCKVM_TIMEOUT:-600}" \
  -e LOCKVM_MEM="${LOCKVM_MEM:-2G}" -e LOCKVM_CPUS="${LOCKVM_CPUS:-4}" \
  "$IMAGE" lockvm-run-guest "$ROOT" "$@"
