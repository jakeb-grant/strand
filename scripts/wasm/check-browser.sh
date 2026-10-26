#!/usr/bin/env bash
# check-browser.sh: the playground (target/site/playground.html, after
# scripts/wasm/build.sh and `strand-site build`) in headless Chromium,
# in eval's pinned Playwright image (scripts/eval/Dockerfile.viewer)
# with no network; scripts/wasm/check.py serves the site on the
# container's loopback. Prints its JSON; screenshots land in
# target/site-shots/.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
DOCKERFILE=$ROOT/scripts/eval/Dockerfile.viewer
IMAGE=strand-viewer-check:$(sha256sum "$DOCKERFILE" | cut -c1-12)
SHOTS=${SITE_SHOTS:-$ROOT/target/site-shots}
LABEL=strand.wasm-check=$$
[ -f target/site/playground.html ] || { echo "no target/site/playground.html: run strand-site build" >&2; exit 2; }
[ -f target/site/assets/wasm/strand_web_bg.wasm ] || { echo "no wasm: run scripts/wasm/build.sh" >&2; exit 2; }
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
