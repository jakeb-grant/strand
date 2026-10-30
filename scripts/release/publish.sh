#!/usr/bin/env bash
# publish.sh DIST: CI's release job (.github/workflows/ci.yml) publishes
# the package job's archive in DIST (scripts/release/package.sh's OUT)
# on GitHub with `gh`, for the installer (scripts/install.sh):
#
#   a push to main   replaces the `edge` pre-release's build, made the
#                    first time; skipped when main has moved on since
#                    (the newer push's run publishes its own)
#   a tag vX         the release vX (X the archive's version), made or
#                    its files replaced
#
# Edge's files are named by their commit and uploaded checksum first, so
# an installer listing the release mid-update finds either the old build
# or the new one whole; the build before the newest is kept a while for
# an installer that listed it. The `edge` tag stays at the commit the
# release was first made at: moving a tag breaks `git fetch` for anyone
# who fetched it ("would clobber existing tag").
#
# Env: GITHUB_REF, GITHUB_SHA, GITHUB_REPOSITORY, GH_TOKEN (contents:
# write). test-publish.sh runs it against a stand-in `gh`.
set -euo pipefail
DIST=${1:?usage: publish.sh DIST}
ARCH=x86_64
SITE=https://strand.jacobgrant.dev
archive=$(ls "$DIST"/strand-*-$ARCH-linux.tar.gz)
name=${archive##*/}
id=${name#strand-}
id=${id%-"$ARCH"-linux.tar.gz}
short=${GITHUB_SHA::12}
when=$(date -u +%Y-%m-%d)

case $GITHUB_REF in
refs/heads/main)
  [ "$id" = "edge-$short" ] || { echo "publish: $name is not the edge build of $short" >&2; exit 1; }
  head=$(gh api "repos/$GITHUB_REPOSITORY/git/ref/heads/main" --jq .object.sha)
  if [ "$head" != "$GITHUB_SHA" ]; then
    echo "::notice::main is at ${head::12} now, not $short: that push's run publishes edge"
    exit 0
  fi
  notes="The newest build of main that passed CI: commit $GITHUB_SHA, built $when.

Install it, or update to it, with:

    curl -fsSL $SITE/install.sh | sh -s -- --edge

The \`edge\` tag stays at the commit this release was first made from; the archive's name and this text say which commit the build is.
See $SITE/start/install.html."
  if ! gh release view edge >/dev/null 2>&1; then
    gh release create edge --prerelease --target "$GITHUB_SHA" --title "edge" --notes "$notes"
  fi
  gh release upload edge "$archive.sha256" --clobber
  gh release upload edge "$archive" --clobber
  gh release upload edge "$DIST/install.sh" --clobber
  gh release edit edge --prerelease --title "edge: $short ($when)" --notes "$notes"
  # The two newest builds stay; older ones go.
  keep=$(gh release view edge --json assets \
    --jq '.assets | sort_by(.createdAt) | reverse | .[].name' |
    sed -n "s/^strand-\(edge-[0-9a-f]*\)-$ARCH-linux\.tar\.gz\$/\1/p" | awk '!seen[$0]++' | head -2)
  gh release view edge --json assets --jq '.assets[].name' | while read -r asset; do
    case $asset in
    strand-edge-*)
      aid=${asset#strand-}
      aid=${aid%%-"$ARCH"-linux.tar.gz*}
      grep -qx "$aid" <<<"$keep" || gh release delete-asset edge "$asset" --yes
      ;;
    esac
  done
  echo "publish: edge is $id"
  ;;
refs/tags/v*)
  tag=${GITHUB_REF#refs/tags/}
  [ "$id" = "${tag#v}" ] || { echo "publish: $name is not the build of $tag" >&2; exit 1; }
  notes="Strand ${tag#v}, built from $GITHUB_SHA.

Install it, or update to it, with:

    curl -fsSL $SITE/install.sh | sh

See $SITE/start/install.html."
  if gh release view "$tag" >/dev/null 2>&1; then
    gh release upload "$tag" "$archive.sha256" "$archive" "$DIST/install.sh" --clobber
  else
    gh release create "$tag" --verify-tag --title "Strand ${tag#v}" --notes "$notes" \
      "$archive.sha256" "$archive" "$DIST/install.sh"
  fi
  echo "publish: $tag is $id"
  ;;
*)
  echo "publish: nothing to publish for $GITHUB_REF" >&2
  exit 1
  ;;
esac
