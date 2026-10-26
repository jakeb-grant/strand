#!/usr/bin/env bash
# Runs a command in the live examples' image (scripts/wasm/Dockerfile: the
# CI image plus wasm32-unknown-unknown, wasm-bindgen-cli and wasm-opt),
# mounted as scripts/container/run.sh mounts the checkout: same paths,
# same cargo volumes and compile cache, the host's uid.
#
# Usage: scripts/wasm/run.sh CMD [ARGS...]   e.g. bash scripts/wasm/build.sh
#        scripts/wasm/run.sh shell
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
[ "$#" -gt 0 ] || { sed -n '2,8p' "$0"; exit 2; }

base_hash=$(sha256sum "$ROOT/scripts/container/Dockerfile" | cut -c1-12)
BASE=strand-ci:$base_hash
if ! docker image inspect "$BASE" >/dev/null 2>&1; then
  echo "building $BASE" >&2
  docker build -t "$BASE" -f "$ROOT/scripts/container/Dockerfile" "$ROOT/scripts/container" >&2
fi
hash=$( (cat "$ROOT/scripts/container/Dockerfile" "$HERE/Dockerfile") | sha256sum | cut -c1-12)
IMAGE=strand-wasm:$hash
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "building $IMAGE" >&2
  docker build --build-arg BASE="$BASE" -t "$IMAGE" -f "$HERE/Dockerfile" "$HERE" >&2
fi

mounts=(-v "$ROOT:$ROOT")
common=$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
case "$common" in
  "" | "$ROOT"/*) ;;
  *) mounts+=(-v "$common:$common") ;;
esac
mounts+=(-v strand-cargo-registry:/opt/cargo/registry -v strand-cargo-git:/opt/cargo/git)

if [ -z "${CARGO_BUILD_JOBS:-}" ]; then
  others=$(docker ps -q --filter label=strand.build=1 | wc -l)
  CARGO_BUILD_JOBS=$(($(nproc) / (others + 1)))
  [ "$CARGO_BUILD_JOBS" -ge 4 ] || CARGO_BUILD_JOBS=4
fi

envs=(
  -e HOME=/tmp/home
  -e CARGO_TARGET_DIR=target/wasm
  -e CARGO_BUILD_JOBS="$CARGO_BUILD_JOBS"
  -e CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
  -e CARGO_PROFILE_DEV_DEBUG=0
  -e CARGO_PROFILE_TEST_DEBUG=0
  -e 'CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS=--cfg getrandom_backend="wasm_js"'
)
if [ "${RUN_SCCACHE:-1}" != 0 ]; then
  mounts+=(-v strand-sccache:/opt/sccache)
  envs+=(
    -e RUSTC_WRAPPER=/usr/local/bin/sccache
    -e SCCACHE_DIR=/opt/sccache
    -e SCCACHE_CACHE_SIZE=40G
    -e SCCACHE_BASEDIRS="$ROOT"
  )
fi
while IFS='=' read -r name _; do
  envs+=(-e "$name")
done < <(env | grep -E '^(STRAND|WASM)_[A-Z0-9_]*=' || true)

tty=()
[ -t 0 ] && [ -t 1 ] && tty=(-it)
[ "$1" = shell ] && set -- bash

mkdir -p "$ROOT/target/wasm"
exec docker run --rm --init "${tty[@]}" \
  --label strand.build=1 \
  --user "$(id -u):$(id -g)" \
  "${mounts[@]}" "${envs[@]}" \
  -w "$ROOT" \
  "$IMAGE" bash -c 'mkdir -p "$HOME" && exec "$@"' bash "$@"
