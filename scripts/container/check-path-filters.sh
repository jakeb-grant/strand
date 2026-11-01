#!/usr/bin/env bash
# check-path-filters.sh [WORKFLOW [TREE]]: every entry of WORKFLOW's
# dorny/paths-filter `filters:` blocks (default ci.yml) matches a file
# tracked in TREE (default this checkout). A filter entry naming a moved
# file matches nothing and says nothing: the job behind it just stops
# running for that file (decisions.md ci-speed-2). A literal path and a
# glob are both held to it; a glob as git's `:(glob)` pathspec reads it,
# which for `*` and `**` is what the filter's picomatch does.
#
# Exit 0: all match. 1: an entry matches nothing. 2: nothing to check or
# an entry this script cannot judge, so a change of format can't pass it.

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
WORKFLOW=${1:-$ROOT/.github/workflows/ci.yml}
TREE=${2:-$ROOT}

# LINE<TAB>ENTRY for each `- entry` under a `filters: |` block, the block
# ending at the first line indented no deeper than `filters:`.
entries=$(awk '
  {
    match($0, /^ */)
    indent = RLENGTH
    if (inside && $0 !~ /^ *$/ && indent <= base) inside = 0
    if (inside && $0 ~ /^ *- /) {
      entry = $0
      sub(/^ *- */, "", entry)
      sub(/ *$/, "", entry)
      gsub(/^["\047]|["\047]$/, "", entry)
      print NR "\t" entry
    }
    if ($0 ~ /^ *filters: *\|/) { inside = 1; base = indent }
  }' "$WORKFLOW") || exit 2

if [ -z "$entries" ]; then
  echo "check-path-filters: no filter entries read from $WORKFLOW" >&2
  exit 2
fi

status=0
count=0
while IFS=$'\t' read -r line entry; do
  count=$((count + 1))
  case $entry in
    *'{'* | *'('* | '!'*)
      echo "check-path-filters: $WORKFLOW:$line: '$entry': braces, groups and negation are not judged here" >&2
      exit 2
      ;;
    *'*'* | *'?'* | *'['*) spec=":(glob)$entry" ;;
    *) spec=":(literal)$entry" ;;
  esac
  found=$(git -C "$TREE" ls-files -- "$spec") || exit 2
  if [ -z "$found" ]; then
    echo "check-path-filters: $WORKFLOW:$line: '$entry' matches no tracked file" >&2
    status=1
  fi
done <<<"$entries"
[ "$status" = 0 ] && echo "check-path-filters: all $count entries match"
exit "$status"
