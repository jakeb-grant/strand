#!/usr/bin/env bash
# serve.sh [--port N] [--no-watch]: the site on http://127.0.0.1:N/
# (8000 by default) for working on it: built (`strand-site build`, in
# the container suite), with the live examples added when
# scripts/wasm/build.sh has built them (target/wasm/pkg), and served by
# Python's http.server in eval's viewer image, published on the
# loopback only. It then builds again whenever a page, the generator or
# a docs scenario changes; reload the browser to see it. A build that
# fails prints why and the last good site stays up. Ctrl-C stops it.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
PORT=8000
WATCH=1
while [ $# -gt 0 ]; do
  case $1 in
    --port) PORT=$2; shift 2 ;;
    --no-watch) WATCH=0; shift ;;
    -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "serve.sh: unknown argument $1" >&2; exit 2 ;;
  esac
done
DOCKERFILE=$ROOT/scripts/eval/Dockerfile.viewer
IMAGE=strand-viewer-check:$(sha256sum "$DOCKERFILE" | cut -c1-12)
NAME=strand-site-serve-$PORT
# What a build reads: the pages, the guide and other docs, the
# generator, and the examples' scenarios.
WATCHED=(site docs crates/strand-site eval/scenarios/docs)

build() {
  if scripts/container/run.sh cargo run -q -p strand-site -- build; then
    if [ -f target/wasm/pkg/strand_web_bg.wasm ]; then
      scripts/wasm/site.sh >/dev/null
    fi
    echo "serve.sh: built $(date +%T); http://127.0.0.1:$PORT/"
  else
    echo "serve.sh: the build failed (above); still serving the last good site" >&2
  fi
}

build
[ -f target/site/index.html ] || { echo "serve.sh: no site to serve" >&2; exit 1; }
[ -f target/wasm/pkg/strand_web_bg.wasm ] ||
  echo "serve.sh: no live examples (run scripts/wasm/build.sh first to have them)"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" -f "$DOCKERFILE" "$ROOT/scripts/eval" >/dev/null
fi
docker rm -f "$NAME" >/dev/null 2>&1 || true
# target/, not target/site: the build writes the site in place, and the
# server follows it either way.
docker run -d --rm --init --name "$NAME" --user "$(id -u):$(id -g)" \
  -p "127.0.0.1:$PORT:8000" -v "$ROOT/target:/target:ro" \
  "$IMAGE" python3 -I -m http.server 8000 --bind 0.0.0.0 --directory /target/site >/dev/null
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT
echo "serve.sh: serving http://127.0.0.1:$PORT/ (Ctrl-C stops it)"
if [ "$WATCH" = 0 ]; then
  docker wait "$NAME" >/dev/null
  exit 0
fi
stamp=$(mktemp)
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -f "$stamp"' EXIT
while sleep 1; do
  if [ -n "$(find "${WATCHED[@]}" -newer "$stamp" -type f -print -quit 2>/dev/null)" ]; then
    touch "$stamp"
    build
  fi
done
