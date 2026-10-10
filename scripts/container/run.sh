#!/usr/bin/env bash
# Runs a command in the CI image (scripts/container/Dockerfile: the CI
# test jobs' Ubuntu 24.04 packages and Rust 1.97.0) as the host user,
# with the checkout mounted at its own absolute path.
#
# Usage: scripts/container/run.sh ci            every step of CI's lint, test,
#                                               budgets, acceptance and timing
#                                               jobs (CI_JOB=NAME: one job)
#        scripts/container/run.sh CMD [ARGS...] e.g. cargo test -p strand-watch
#        scripts/container/run.sh shell         an interactive bash
# Env:   CARGO_BUILD_JOBS (default 6); every STRAND_* variable set on the
#        host is passed in (STRAND_REQUIRE_SWAY/DBUS/PIPEWIRE/GPU default
#        to 1, as in CI; the GPU tier is lavapipe, the image's only
#        Vulkan driver, accepted with STRAND_GPU_SOFTWARE=1).
#
# Builds land in <checkout>/target/container (apart from any native
# target/); the cargo registry and git checkouts are the named volumes
# strand-cargo-registry and strand-cargo-git, shared by every worktree.
# The image is tagged by the Dockerfile's hash and built when missing.
# Works from a git worktree: the main repository's .git is mounted too.

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
[ "$#" -gt 0 ] || { sed -n '2,21p' "$0"; exit 2; }

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
  -e STRAND_REQUIRE_GPU=1
  -e STRAND_GPU_SOFTWARE=1
)
# The host's STRAND_* (overriding the defaults above).
while IFS='=' read -r name _; do
  envs+=(-e "$name")
done < <(env | grep -E '^STRAND_[A-Z0-9_]*=' || true)
[ -n "${CI_FROM:-}" ] && envs+=(-e CI_FROM)
[ -n "${CI_JOB:-}" ] && envs+=(-e CI_JOB)

# The advisory hardware GPU leg (scripts/container/gpu.sh sets
# RUN_GPU_HARDWARE to the hardware ICD's JSON): the render node and
# nothing else from /dev/dri (never a card*, CLAUDE.md), and that ICD in
# place of lavapipe.
devices=()
if [ -n "${RUN_GPU_HARDWARE:-}" ]; then
  devices=(--device /dev/dri/renderD128)
  envs+=(
    -e VK_DRIVER_FILES="$RUN_GPU_HARDWARE"
    -e VK_ICD_FILENAMES="$RUN_GPU_HARDWARE"
    -e STRAND_GPU_HARDWARE=1
  )
fi
for arg in "${devices[@]}"; do
  case "$arg" in *card*) echo "run.sh: refusing to pass $arg (render node only)" >&2; exit 2 ;; esac
done

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
  "${devices[@]}" "${mounts[@]}" "${envs[@]}" \
  -w "$ROOT" \
  "$IMAGE" bash -c 'mkdir -p "$HOME" && exec "$@"' bash "$@"
