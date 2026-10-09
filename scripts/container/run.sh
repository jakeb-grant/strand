#!/usr/bin/env bash
# Runs a command in the CI image (scripts/container/Dockerfile: the CI
# `check` job's Ubuntu 24.04 packages and Rust 1.97.0) as the host user,
# with the checkout mounted at its own absolute path.
#
# Usage: scripts/container/run.sh ci            every step of CI's check job
#        scripts/container/run.sh CMD [ARGS...] e.g. cargo test -p strand-watch
#        scripts/container/run.sh shell         an interactive bash
# Env:   CARGO_BUILD_JOBS (default 6); every STRAND_* variable set on the
#        host is passed in (STRAND_REQUIRE_SWAY/DBUS/PIPEWIRE default to 1,
#        as in CI).
#
# Builds land in <checkout>/target/container (apart from any native
# target/); the cargo registry and git checkouts are the named volumes
# strand-cargo-registry and strand-cargo-git, shared by every worktree.
# The image is tagged by the Dockerfile's hash and built when missing.
# Works from a git worktree: the main repository's .git is mounted too.

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
[ "$#" -gt 0 ] || { sed -n '2,17p' "$0"; exit 2; }

hash=$(sha256sum "$HERE/Dockerfile" | cut -c1-12)
IMAGE=strand-ci:$hash
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "building $IMAGE" >&2
  docker build -t "$IMAGE" -f "$HERE/Dockerfile" "$HERE" >&2
fi

mounts=(-v "$ROOT:$ROOT")
# A worktree's .git is a file pointing into the main repository's
# common dir: mount that too, at its own path.
common=$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
case "$common" in
  "" | "$ROOT"/*) ;;
  *) mounts+=(-v "$common:$common") ;;
esac
mounts+=(-v strand-cargo-registry:/opt/cargo/registry -v strand-cargo-git:/opt/cargo/git)

envs=(
  -e HOME=/tmp/home
  -e CARGO_TARGET_DIR="$ROOT/target/container"
  -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}"
  -e CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
  -e RUSTFLAGS="-D warnings"
  -e CARGO_PROFILE_DEV_DEBUG=0
  -e CARGO_PROFILE_TEST_DEBUG=0
  -e STRAND_REQUIRE_SWAY=1
  -e STRAND_REQUIRE_DBUS=1
  -e STRAND_REQUIRE_PIPEWIRE=1
)
# The host's STRAND_* (overriding the defaults above).
while IFS='=' read -r name _; do
  envs+=(-e "$name")
done < <(env | grep -E '^STRAND_[A-Z0-9_]*=' || true)
[ -n "${CI_FROM:-}" ] && envs+=(-e CI_FROM)

tty=()
[ -t 0 ] && [ -t 1 ] && tty=(-it)

case "$1" in
  ci) set -- bash "$HERE/ci.sh" ;;
  shell) set -- bash ;;
esac

mkdir -p "$ROOT/target/container"
exec docker run --rm --init "${tty[@]}" \
  --user "$(id -u):$(id -g)" \
  --shm-size=2g \
  "${mounts[@]}" "${envs[@]}" \
  -w "$ROOT" \
  "$IMAGE" bash -c 'mkdir -p "$HOME" && exec "$@"' bash "$@"
