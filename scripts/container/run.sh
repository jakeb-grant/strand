#!/usr/bin/env bash
# Runs a command in the CI image (scripts/container/Dockerfile: the CI
# test jobs' Ubuntu 24.04 packages and Rust 1.97.0) as the host user,
# with the checkout mounted at its own absolute path.
#
# Usage: scripts/container/run.sh ci            every step of CI's lint, test,
#                                               budgets, acceptance and timing
#                                               jobs (CI_JOB=NAME: one job)
#        scripts/container/run.sh affected      the tests of the crates this
#                                               branch changed (affected.sh:
#                                               --deep, --list, BASE)
#        scripts/container/run.sh eval CMD ...  the animation evaluation
#                                               sandbox (scripts/eval/eval.sh:
#                                               strand-eval's commands,
#                                               `selftest`, scripts/eval/ci.sh,
#                                               and `mutants`, mutants.sh)
#        scripts/container/run.sh CMD [ARGS...] e.g. cargo test -p strand-watch
#        scripts/container/run.sh shell         an interactive bash
# Env:   CARGO_BUILD_JOBS (default: the cores shared among the strand
#        builds running, at least 4); RUN_SCCACHE=0 builds without the
#        compile cache; every STRAND_* variable set on the
#        host is passed in (STRAND_REQUIRE_SWAY/DBUS/PIPEWIRE/GPU default
#        to 1, as in CI; the GPU tier is lavapipe, the image's only
#        Vulkan driver, accepted with STRAND_GPU_SOFTWARE=1), and so
#        is every SIZE_REPORT_* (scripts/size-report.sh's options).
#
# Builds land in <checkout>/target/container (apart from any native
# target/); the cargo registry and git checkouts are the named volumes
# strand-cargo-registry and strand-cargo-git, and sccache's compile cache
# the volume strand-sccache, shared by every worktree (decisions.md,
# ci-speed).
# The image is tagged by the Dockerfile's hash and built when missing.
# Works from a git worktree: the main repository's .git is mounted too.

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
[ "$#" -gt 0 ] || { sed -n '2,33p' "$0"; exit 2; }

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

# Cores: a build alone takes them all; with others running (at most three
# worktrees build at once, CLAUDE.md) each takes its share.
if [ -z "${CARGO_BUILD_JOBS:-}" ]; then
  others=$(docker ps -q --filter label=strand.build=1 | wc -l)
  CARGO_BUILD_JOBS=$(($(nproc) / (others + 1)))
  [ "$CARGO_BUILD_JOBS" -ge 4 ] || CARGO_BUILD_JOBS=4
fi

# CARGO_TARGET_DIR is relative to the checkout (the container's working
# directory): sccache hashes CARGO_* variables, and an absolute path would
# keep each worktree's builds out of the others' cache.
envs=(
  -e HOME=/tmp/home
  -e CARGO_TARGET_DIR=target/container
  -e CARGO_BUILD_JOBS="$CARGO_BUILD_JOBS"
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
# The compile cache: paths under the checkout are hashed relative to it
# (SCCACHE_BASEDIRS), so every worktree shares one cache.
if [ "${RUN_SCCACHE:-1}" != 0 ]; then
  mounts+=(-v strand-sccache:/opt/sccache)
  envs+=(
    -e RUSTC_WRAPPER=/usr/local/bin/sccache
    -e SCCACHE_DIR=/opt/sccache
    -e SCCACHE_CACHE_SIZE=40G
    -e SCCACHE_BASEDIRS="$ROOT"
  )
fi
# The host's STRAND_* (overriding the defaults above) and
# scripts/size-report.sh's SIZE_REPORT_*.
while IFS='=' read -r name _; do
  envs+=(-e "$name")
done < <(env | grep -E '^(STRAND|SIZE_REPORT)_[A-Z0-9_]*=' || true)
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
  affected) shift; set -- bash "$HERE/affected.sh" "$@" ;;
  eval) shift; set -- bash "$HERE/../eval/eval.sh" "$@" ;;
  shell) set -- bash ;;
esac

mkdir -p "$ROOT/target/container"
exec docker run --rm --init "${tty[@]}" \
  --label strand.build=1 \
  --user "$(id -u):$(id -g)" \
  --shm-size=2g \
  "${devices[@]}" "${mounts[@]}" "${envs[@]}" \
  -w "$ROOT" \
  "$IMAGE" bash -c 'mkdir -p "$HOME" && exec "$@"' bash "$@"
