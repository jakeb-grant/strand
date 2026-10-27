#!/usr/bin/env bash
# package.sh [--check] SITE OUT: the docs site ready for Railway
# (deploy/railway/README.md). OUT gets deploy/railway's Dockerfile,
# Caddyfile and railway.toml, and SITE (a built site with its live
# examples, CI's `site-live` artifact) as OUT/site. --check then builds
# the image and asks the server, inside a container with no network,
# for a page, llms.txt, a markdown copy, the wasm and a missing page,
# and fails unless each answers with the status, type and encoding the
# site needs.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CHECK=0
if [ "${1-}" = --check ]; then CHECK=1; shift; fi
if [ $# -ne 2 ]; then
  sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
fi
SITE=$1
OUT=$2
[ -f "$SITE/index.html" ] || { echo "package: $SITE has no index.html" >&2; exit 1; }
[ -f "$SITE/wasm/manifest.js" ] \
  || { echo "package: $SITE has no live examples (wasm/): build it with scripts/wasm/site.sh" >&2; exit 1; }
rm -rf "$OUT"
mkdir -p "$OUT"
cp "$ROOT"/deploy/railway/{Dockerfile,Caddyfile,railway.toml} "$OUT"/
cp -r "$SITE" "$OUT/site"
echo "package: $OUT ($(du -sh "$OUT/site" | cut -f1) of site)"
[ $CHECK = 1 ] || exit 0

IMAGE=strand-site-railway-check
CURL=curlimages/curl:8.16.0@sha256:463eaf6072688fe96ac64fa623fe73e1dbe25d8ad6c34404a669ad3ce1f104b6
NAME=strand-site-railway-check-$$
docker build -q -t "$IMAGE" "$OUT" >/dev/null
docker run -d --rm --network none --name "$NAME" -e PORT=8080 "$IMAGE" >/dev/null
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT
# curl in the server's own network namespace, which has no other.
curl() { docker run --rm --network "container:$NAME" "$CURL" "$@"; }
curl -sS --retry 20 --retry-connrefused --retry-delay 1 -o /dev/null http://127.0.0.1:8080/
FAILED=0
# [CACHE=…] [ACCEPT=…] expect PATH STATUS TYPE [ENCODING]: the
# response's status, Content-Type, Content-Encoding (asked for ACCEPT,
# by default brotli or gzip; none when not given) and, when CACHE is
# set, Cache-Control.
expect() {
  local got want
  got=$(curl -s -o /dev/null -H "Accept-Encoding: ${ACCEPT:-br, gzip}" \
    -w '%{http_code} | %{content_type} | %header{content-encoding} | %header{cache-control}' \
    "http://127.0.0.1:8080$1")
  want="$2 | $3 | ${4-} | ${CACHE-}"
  [ -n "${CACHE-}" ] || got=${got% | *}" | "
  if [ "$got" = "$want" ]; then
    echo "package: $1: $got"
  else
    echo "package: $1: got $got; want $want" >&2
    FAILED=1
  fi
}
WASM=$(cd "$OUT/site" && ls wasm/strand_web_bg.*.wasm | head -1)
CACHE=no-cache expect / 200 "text/html; charset=utf-8" br
CACHE=no-cache expect /guide/level-1.html 200 "text/html; charset=utf-8" br
CACHE=no-cache ACCEPT=gzip expect /guide/level-1.html 200 "text/html; charset=utf-8" gzip
CACHE=no-cache expect /llms.txt 200 "text/plain; charset=utf-8" br
CACHE=no-cache expect /guide/level-1.html.md 200 "text/markdown; charset=utf-8" br
# Under 512 bytes, Caddy's least worth compressing.
CACHE=no-cache expect /wasm/manifest.js 200 "text/javascript; charset=utf-8"
CACHE=no-cache expect /assets/site.css 200 "text/css; charset=utf-8" br
CACHE="public, max-age=31536000, immutable" expect "/$WASM" 200 "application/wasm" br
CACHE=no-cache expect /media/hello-light.png 200 "image/png"
expect /no-such-page.html 404 "text/html; charset=utf-8"
[ $FAILED = 0 ] || exit 1
echo "package: the server answers as the site needs"
