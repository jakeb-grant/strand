#!/usr/bin/env bash
# shots.sh [page ...]: the built site (target/site, `strand-site build`)
# in headless Chromium, in eval's pinned Playwright image
# (scripts/eval/Dockerfile.viewer) with no network: each page at 1440,
# 768 and 390 px, light and dark, saved to target/site-shots/, failing on
# console errors, failed requests and sideways overflow (shots.py).
# Pages are paths in the site; index.html when none is given, every page
# with --all (CI's site job). The search is driven on the first page.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
DOCKERFILE=$ROOT/scripts/eval/Dockerfile.viewer
IMAGE=strand-viewer-check:$(sha256sum "$DOCKERFILE" | cut -c1-12)
SHOTS=${SITE_SHOTS:-$ROOT/target/site-shots}
LABEL=strand.site-shots=$$
[ -f target/site/index.html ] || { echo "no target/site: run strand-site build" >&2; exit 2; }
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" -f "$DOCKERFILE" "$ROOT/scripts/eval" >&2
fi
mkdir -p "$SHOTS"
status=0
docker run --rm --init --network none --label "$LABEL" \
  --user "$(id -u):$(id -g)" -e HOME=/tmp --shm-size=1g \
  -v "$ROOT/target/site:/site:ro" -v "$SHOTS:/shots" -v "$ROOT/scripts/site:/check:ro" \
  "$IMAGE" python3 -I /check/shots.py --site /site --shots /shots "$@" || status=$?
left=$(docker ps -aq --filter "label=$LABEL")
if [ -n "$left" ]; then
  echo "shots: a container outlived its run: $left" >&2
  docker rm -f $left >/dev/null 2>&1 || true
  status=1
fi
exit $status
