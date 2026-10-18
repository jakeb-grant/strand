#!/usr/bin/env bash
# The viewer in a real browser (docs/eval.md, "The viewer in a browser";
# docs/decisions.md eval-viewer-browser): headless Chromium in a pinned
# Playwright image loads each report page from file://, with no network,
# drives every mode (scripts/eval/viewer_check.py) and fails on any
# console error, uncaught exception, failed request or broken mode.
# Screenshots of each mode, light and dark, at a desktop's and a phone's
# width land in target/eval/viewer-shots/ (VIEWER_SHOTS overrides).
#
# Usage: scripts/eval/viewer-check.sh [--expect KINDS] PAGE...
#        PAGE: a report directory (strand-eval check/run/ab, or a matrix
#        run) or its index.html. Each is mounted read-only.
#        --expect pass,fail,changed,matrix: each kind must have been
#        exercised (a passing scenario, a failing one, one that left its
#        approved reference, a matrix grid).
#        scripts/eval/viewer-check.sh --selftest builds its own pages
#        first: the curated and regression scenarios, then the same
#        under the bug25-hidden-text mutant (a failing and a CHANGED
#        scenario, the A/B against the reference) and a small matrix,
#        each through scripts/container/run.sh, and checks them all.
# Runs on the host's Docker like run.sh (never inside it); the container
# is removed at the end and the script fails if one is left.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
# The image: scripts/eval/Dockerfile.viewer (Playwright's, pinned by
# version and digest, with its Python package), tagged by its hash and
# built when missing, like run.sh's.
DOCKERFILE=$ROOT/scripts/eval/Dockerfile.viewer
IMAGE=strand-viewer-check:$(sha256sum "$DOCKERFILE" | cut -c1-12)
SHOTS=${VIEWER_SHOTS:-$ROOT/target/eval/viewer-shots}
LABEL=strand.viewer-check=$$

expect=
pages=()
selftest=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --expect) expect=$2; shift 2 ;;
    --selftest) selftest=1; shift ;;
    -h | --help) sed -n '2,22p' "$0"; exit 0 ;;
    *) pages+=("$1"); shift ;;
  esac
done

if [ "$selftest" = 1 ]; then
  src=target/eval/viewer-src
  patch=eval/mutants/bug25-hidden-text.patch
  if ! git diff --quiet -- crates/strand-render; then
    echo "viewer-check --selftest: crates/strand-render has changes; it applies a mutant there" >&2
    exit 2
  fi
  rm -rf "$src"
  scripts/container/run.sh eval check eval/scenarios/curated eval/scenarios/regressions \
    --frames all --out "$src/pass" >/dev/null
  git apply "$patch"
  trap 'git apply -R "$ROOT/$patch"' EXIT
  # Under the mutant the check and the matrix fail (exit 1): their pages are the point.
  scripts/container/run.sh eval check eval/scenarios/curated eval/scenarios/regressions \
    --frames all --out "$src/mutant" >/dev/null || [ "$?" = 1 ]
  scripts/container/run.sh eval matrix --effects enter-fade,late-text-1,size-spring --hz 60 \
    --out "$src/matrix" >/dev/null || [ "$?" = 1 ]
  git apply -R "$patch"
  trap - EXIT
  pages=("$src/pass" "$src/mutant" "$src/matrix")
  expect=${expect:-pass,fail,changed,matrix}
fi
[ "${#pages[@]}" -gt 0 ] || { sed -n '2,22p' "$0"; exit 2; }

mkdir -p "$SHOTS"
rm -f "$SHOTS"/*.png
mounts=(-v "$ROOT/scripts/eval/viewer_check.py:/check/viewer_check.py:ro" -v "$SHOTS:/shots")
args=()
envs=()
n=0
for p in "${pages[@]}"; do
  [ -d "$p" ] && p=$p/index.html
  [ -f "$p" ] || { echo "viewer-check: no page at $p" >&2; exit 2; }
  dir=$(cd "$(dirname "$p")" && pwd)
  mounts+=(-v "$dir:/pages/$n:ro")
  args+=("/pages/$n/$(basename "$p")")
  envs+=(-e "VIEWER_TAG_$n=$(basename "$dir")")
  n=$((n + 1))
done

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "building $IMAGE" >&2
  docker build -q -t "$IMAGE" -f "$DOCKERFILE" "$ROOT/scripts/eval" >&2
fi
status=0
docker run --rm --init --network none --label "$LABEL" \
  --user "$(id -u):$(id -g)" -e HOME=/tmp --shm-size=1g \
  "${mounts[@]}" "${envs[@]}" "$IMAGE" \
  python3 -I /check/viewer_check.py --shots /shots --expect "$expect" "${args[@]}" || status=$?

# The browser lived in that container: nothing of it may be left.
left=$(docker ps -aq --filter "label=$LABEL")
if [ -n "$left" ]; then
  echo "viewer-check: a container outlived its run: $left" >&2
  docker rm -f $left >/dev/null 2>&1 || true
  status=1
fi
echo "screenshots: $SHOTS"
exit "$status"
