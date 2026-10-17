#!/usr/bin/env bash
# eval/ keeps its own Cargo.lock (it is a workspace of its own); every
# package it shares with the root Cargo.lock must be locked at a version
# the root locks, so the sandbox builds what the product builds
# (docs/decisions.md eval-sandbox). Packages only eval/ has are listed.
#
# Usage: scripts/eval/check-lock.sh [ROOT_LOCK EVAL_LOCK]
#        (default: Cargo.lock and eval/Cargo.lock of this checkout)
# Self-test: scripts/eval/test-check-lock.sh

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
root_lock=${1:-$ROOT/Cargo.lock}
eval_lock=${2:-$ROOT/eval/Cargo.lock}
for f in "$root_lock" "$eval_lock"; do
  [ -f "$f" ] || { echo "check-lock: no $f" >&2; exit 2; }
done

# "name version" per [[package]].
pairs() {
  awk '
    /^\[\[package\]\]/ { if (n != "") print n, v; n = ""; v = ""; next }
    /^name = "/ { n = $3; gsub(/"/, "", n) }
    /^version = "/ { v = $3; gsub(/"/, "", v) }
    END { if (n != "") print n, v }
  ' "$1" | sort -u
}

root_pairs=$(pairs "$root_lock")
bad=0
only=()
while read -r name version; do
  [ -n "$name" ] || continue
  have=$(awk -v n="$name" '$1 == n { print $2 }' <<<"$root_pairs")
  if [ -z "$have" ]; then
    only+=("$name $version")
  elif ! grep -qxF "$version" <<<"$have"; then
    want=$(tr '\n' ' ' <<<"$have")
    echo "check-lock: $name $version in eval/Cargo.lock; the root locks ${want% }"
    echo "  fix: cargo update --manifest-path eval/Cargo.toml -p $name@$version --precise ${have%%$'\n'*}"
    bad=1
  fi
done < <(pairs "$eval_lock")

for p in "${only[@]}"; do
  echo "check-lock: only in eval/Cargo.lock: $p"
done
if [ "$bad" = 0 ]; then
  echo "check-lock: every package eval/ shares with the root is locked at the root's version"
fi
exit "$bad"
