#!/usr/bin/env bash
# CI's image and image-publish jobs (.github/workflows/ci.yml): publishes
# scripts/container/Dockerfile under its hash tag (image.sh --ref), once.
# A published tag is never written again, so nothing here pushes unless the
# registry has said the tag is absent. The laptop never runs this.
#
# Usage: publish-image.sh missing  prints true unless the tag is there, for
#                                  the image job, which holds no credentials
#                                  (a private package or a registry error
#                                  also reads true; publish decides)
#        publish-image.sh publish  checks again, with the credentials of
#                                  `docker login`, and builds and pushes when
#                                  absent; any answer other than present or
#                                  absent fails, as it can't prove absence

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REF=$("$HERE/image.sh" --ref)

case "${1:-}" in
  missing)
    if docker manifest inspect "$REF" >/dev/null 2>&1; then echo false; else echo true; fi
    ;;
  publish)
    status=0
    out=$(docker manifest inspect "$REF" 2>&1 >/dev/null) || status=$?
    if [ "$status" = 0 ]; then
      echo "publish-image: $REF is already published: left as it is"
      exit 0
    fi
    # For a package that doesn't exist yet GHCR answers `denied`, even with
    # a token that may create it; a token that truly may not write fails
    # at the push, overwriting nothing.
    if ! grep -qiE 'manifest unknown|no such manifest|not found|denied' <<<"$out"; then
      echo "publish-image: can't tell whether $REF exists: $out" >&2
      exit 1
    fi
    echo "publish-image: building and pushing $REF"
    docker build -t "$REF" \
      --label "org.opencontainers.image.source=https://github.com/jakeb-grant/strand" \
      -f "$HERE/Dockerfile" "$HERE"
    docker push "$REF"
    ;;
  *)
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
    ;;
esac
