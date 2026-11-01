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
#
# Under GITHUB_ACTIONS only the registry's own "no such tag" builds (a fork's
# new Dockerfile), with a warning. Any other failed pull is retried and then
# fails the job, so a registry blip can't run the gates or build release
# archives on an image nobody published; and a build from an earlier step
# stays, so one job's steps all run on one image.

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
hash=$(sha256sum "$HERE/Dockerfile" | cut -c1-12)
LOCAL=strand-ci:$hash
PUBLISHED=ghcr.io/jakeb-grant/strand-ci:$hash

if [ "${1:-}" = --ref ]; then
  echo "$PUBLISHED"
  exit 0
fi

# Sets `why` to what the pull said; its progress goes to stderr.
pull() {
  { why=$(docker pull "$PUBLISHED" 2>&1 >&3); } 3>&2
}
# The registry's own "no such tag": the classic image store's wording, and
# the containerd store's, `failed to resolve reference "<ref>": <ref>: not found`.
absent() {
  grep -qiE 'manifest unknown|no such manifest|resolve reference.*: not found$' <<<"$why"
}

why=
if docker image inspect "$PUBLISHED" >/dev/null 2>&1; then
  # Tagged here by an earlier pull, unless someone removed the local name.
  docker image inspect "$LOCAL" >/dev/null 2>&1 || docker tag "$PUBLISHED" "$LOCAL" >&2
elif [ -n "${GITHUB_ACTIONS:-}" ] && docker image inspect "$LOCAL" >/dev/null 2>&1; then
  echo "image.sh: $LOCAL, built by an earlier step of this job" >&2
else
  pulled=true
  pull || pulled=false
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    for wait in 5 20; do
      if [ "$pulled" = true ] || absent; then break; fi
      echo "image.sh: pulling $PUBLISHED failed, again in ${wait}s: $why" >&2
      sleep "$wait"
      pulled=true
      pull || pulled=false
    done
    if [ "$pulled" = false ] && ! absent; then
      echo "::error::image.sh: can't pull $PUBLISHED and the registry doesn't say it is absent: $why" >&2
      exit 1
    fi
    [ "$pulled" = true ] || echo "::warning::image.sh: $PUBLISHED is not published: this job runs on an image built here" >&2
  fi
  if [ "$pulled" = true ]; then
    docker tag "$PUBLISHED" "$LOCAL" >&2
  elif docker image inspect "$LOCAL" >/dev/null 2>&1; then
    echo "image.sh: $PUBLISHED is not to be had: using $LOCAL, built on this machine" >&2
  else
    echo "image.sh: $PUBLISHED is not to be had: building $LOCAL on this machine" >&2
    docker build -t "$LOCAL" -f "$HERE/Dockerfile" "$HERE" >&2
  fi
fi
echo "$LOCAL"
