#!/usr/bin/env bash
# check-browser.sh: the playground (docs/live-examples.md) of the
# built site (target/site, after `strand-site build`,
# scripts/wasm/build.sh and scripts/wasm/site.sh) in headless Chromium,
# in eval's pinned Playwright image (scripts/eval/Dockerfile.viewer)
# with no network; scripts/wasm/check.py serves the site on the
# container's loopback and holds every docs example's live frames to
# eval's. Prints its JSON; screenshots land in target/site-shots/live/.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
DOCKERFILE=$ROOT/scripts/eval/Dockerfile.viewer
IMAGE=strand-viewer-check:$(sha256sum "$DOCKERFILE" | cut -c1-12)
SHOTS=${SITE_SHOTS:-$ROOT/target/site-shots/live}
LABEL=strand.wasm-check=$$
[ -f target/site/index.html ] || { echo "no target/site: run strand-site build" >&2; exit 2; }
grep -q '"module"' target/site/wasm/manifest.js 2>/dev/null || { echo "target/site has no live examples: run scripts/wasm/build.sh, then scripts/wasm/site.sh" >&2; exit 2; }
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" -f "$DOCKERFILE" "$ROOT/scripts/eval" >&2
fi
mkdir -p "$SHOTS"
status=0
docker run --rm --init --network none --label "$LABEL" \
  --user "$(id -u):$(id -g)" -e HOME=/tmp --shm-size=1g \
  -v "$ROOT/target/site:/site:ro" -v "$SHOTS:/shots" -v "$ROOT/scripts/wasm:/check:ro" \
  "$IMAGE" python3 -I /check/check.py --site /site --shots /shots || status=$?
left=$(docker ps -aq --filter "label=$LABEL")
if [ -n "$left" ]; then
  docker rm -f $left >/dev/null 2>&1 || true
  status=1
fi
exit $status
