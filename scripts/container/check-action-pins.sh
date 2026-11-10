#!/usr/bin/env bash
# check-action-pins.sh [DIR]: every `uses:` in DIR's workflow and action files
# (default this checkout's .github) that names a third-party action pins it to
# a full 40-hex commit SHA, the version in a trailing comment. A tag or a
# branch can be moved by whoever holds that repository, and the job that builds
# the released archives runs these actions with write access to the checkout
# and the dependency caches (decisions.md audit-action-pins). Local actions
# (`./...`) and GitHub's own `actions/*` are exempt; .github/dependabot.yml
# moves the pins.
#
# Exit 0: all pinned. 1: a ref that is not a SHA. 2: no file to read.

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
DIR=${1:-$ROOT/.github}

files=$(find "$DIR" -type f \( -name '*.yml' -o -name '*.yaml' \) | sort)
if [ -z "$files" ]; then
  echo "check-action-pins: no workflow or action files under $DIR" >&2
  exit 2
fi

status=0
count=0
while IFS= read -r file; do
  # LINE<TAB>REF for each `uses:` key, a list item's or a step's.
  while IFS=$'\t' read -r line ref; do
    [ -n "$ref" ] || continue
    count=$((count + 1))
    case $ref in
      ./* | actions/*) continue ;;
    esac
    if ! [[ $ref =~ ^[^@[:space:]]+@[0-9a-f]{40}$ ]]; then
      echo "check-action-pins: $file:$line: '$ref' is not pinned to a commit SHA" >&2
      status=1
    fi
  done < <(awk '
    /^[ \t]*(-[ \t]+)?uses:/ {
      ref = $0
      sub(/^[ \t]*(-[ \t]+)?uses:[ \t]*/, "", ref)
      sub(/[ \t]+#.*$/, "", ref)
      sub(/[ \t]*$/, "", ref)
      gsub(/^["\047]|["\047]$/, "", ref)
      print NR "\t" ref
    }' "$file")
done <<<"$files"

[ "$status" = 0 ] && echo "check-action-pins: all $count uses pinned or exempt"
exit "$status"
