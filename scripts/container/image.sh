#!/usr/bin/env bash
# The CI image, one for the laptop and GitHub: scripts/container/Dockerfile
# as published at ghcr.io/jakeb-grant/strand-ci:<first 12 hex digits of the
# Dockerfile's sha256> by ci.yml's image-publish job (publish-image.sh),
# never from a laptop.
#
# Usage: image.sh        makes it local and prints its local name,
#                        strand-ci:<hash>, on stdout alone
#        image.sh --ref  prints the published name, touching nothing
#
# The published image already here is used as it is. Otherwise it is pulled.
# A Dockerfile not published yet (an edit in progress, a fork's pull
# request, no network, or a package the host may not read) is built here as
# strand-ci:<hash> with a note saying so; every later call tries the pull
# again, so a build made here gives way to the published image once there
# is one. Its packages come from the day it was built, the reason pixel
# references are only ever judged on the published image.

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
hash=$(sha256sum "$HERE/Dockerfile" | cut -c1-12)
LOCAL=strand-ci:$hash
PUBLISHED=ghcr.io/jakeb-grant/strand-ci:$hash

if [ "${1:-}" = --ref ]; then
  echo "$PUBLISHED"
  exit 0
fi

if docker image inspect "$PUBLISHED" >/dev/null 2>&1; then
  # Tagged here by an earlier pull, unless someone removed the local name.
  docker image inspect "$LOCAL" >/dev/null 2>&1 || docker tag "$PUBLISHED" "$LOCAL" >&2
elif docker pull "$PUBLISHED" 2>/dev/null >&2; then
  docker tag "$PUBLISHED" "$LOCAL" >&2
else
  if docker image inspect "$LOCAL" >/dev/null 2>&1; then
    echo "image.sh: $PUBLISHED is not to be had: using $LOCAL, built on this machine" >&2
  else
    echo "image.sh: $PUBLISHED is not to be had: building $LOCAL on this machine" >&2
    docker build -t "$LOCAL" -f "$HERE/Dockerfile" "$HERE" >&2
  fi
fi
echo "$LOCAL"
