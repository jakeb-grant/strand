#!/usr/bin/env bash
# prune-rust-caches.sh [--dry-run]: CI's prune-caches job. Deletes every
# Swatinem/rust-cache entry but the newest of its name in its ref, with
# `gh` (GH_TOKEN with actions: write). A key is
#
#   v0-rust-<name>-<env and toolchain hash>-<lock and manifest hash>
#
# and a run restores by exact key, then by the name and first hash, newest
# first, so an older entry is read only by a branch still on its lock
# and otherwise only fills the 10 GB (decisions.md ci-speed-2). Entries of
# one ref are compared only with each other: a cache saved on a branch
# must not take main's, which every branch reads. Other caches
# (cargo-about, wasm-bindgen) are left alone.
#
# --dry-run prints what it would delete and deletes nothing.
set -euo pipefail

dry=0
case ${1-} in
  "") ;;
  --dry-run) dry=1 ;;
  *) echo "usage: prune-rust-caches.sh [--dry-run]" >&2; exit 2 ;;
esac

list=$(gh cache list --limit 1000 --json id,key,ref,sizeInBytes,createdAt)
doomed=$(jq -r '
  [ .[]
    | select(.key | test("^v0-rust-.+-[0-9a-f]{8}-[0-9a-f]{8}$"))
    | . + { name: (.key | sub("-[0-9a-f]{8}-[0-9a-f]{8}$"; "")) } ]
  | group_by([.name, .ref])[]
  | sort_by(.createdAt)[:-1][]
  | [.id, .sizeInBytes, .ref, .key] | @tsv' <<<"$list")

freed=0
count=0
failed=0
while IFS=$'\t' read -r id size ref key; do
  [ -n "$id" ] || continue
  if [ "$dry" = 1 ]; then
    echo "would delete $key ($ref, $((size / 1048576)) MiB)"
  elif gh cache delete "$id"; then
    echo "deleted $key ($ref, $((size / 1048576)) MiB)"
  else
    echo "::warning::could not delete $key ($ref)"
    failed=1
    continue
  fi
  freed=$((freed + size))
  count=$((count + 1))
done <<<"$doomed"
echo "prune-rust-caches: $count caches, $((freed / 1048576)) MiB$([ "$dry" = 1 ] && echo " (dry run)")"
exit "$failed"
