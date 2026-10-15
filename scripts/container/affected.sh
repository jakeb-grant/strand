#!/usr/bin/env bash
# Tests what a branch changed: the workspace crates with a changed file
# since the merge base with BASE (default main), committed or not, and
# with --deep every crate that depends on them. The inner loop's check
# before a commit (CLAUDE.md); the whole suite stays `run.sh ci`, once
# per branch. Run through scripts/container/run.sh affected [--deep]
# [--list] [BASE] [-- TEST ARGS...].
#
# A change to the root Cargo.toml, Cargo.lock or .cargo/ tests every
# crate. A change under docs/ adds the crates whose tests read the docs
# (strand-dev's reference, strand-compiler's spec and design snippets,
# strand's guide and architecture checks). Other files outside crates/
# (scripts, .github) test nothing here; ci covers them.
#
# --list prints the crates and runs nothing. Test args after `--` go to
# the test binaries; by default the fuzzer and the 100 reloads are
# skipped, as in ci's workspace step.

set -euo pipefail
cd "$(dirname "$0")/../.."

deep=0 list=0 base=main
while [ "$#" -gt 0 ]; do
  case "$1" in
    --deep) deep=1 ;;
    --list) list=1 ;;
    --) shift; break ;;
    -*) echo "affected.sh: unknown option $1" >&2; exit 2 ;;
    *) base=$1 ;;
  esac
  shift
done
test_args=("$@")
[ "${#test_args[@]}" -gt 0 ] ||
  test_args=(--skip random_edits_through_five_save_styles --skip a_hundred_reloads_reconnect_and_restart_nothing)

# AFFECTED_FILES (one path a line) stands in for the diff: the tests'.
if [ -n "${AFFECTED_FILES+set}" ]; then
  mb=$base
  changed=$AFFECTED_FILES
else
  mb=$(git merge-base HEAD "$base")
  changed=$( { git diff --name-only "$mb"; git ls-files --others --exclude-standard; } | sort -u)
fi
if [ -z "$changed" ]; then
  echo "affected: nothing changed since $base ($mb)"
  exit 0
fi

meta=$(cargo metadata --format-version 1 --offline 2>/dev/null || cargo metadata --format-version 1)
root=$(pwd)
# "dir name" for every workspace member, dir relative to the root.
members=$(jq -r --arg root "$root/" '
  . as $m | $m.workspace_members[] as $id
  | $m.packages[] | select(.id == $id)
  | "\(.manifest_path | ltrimstr($root) | rtrimstr("/Cargo.toml")) \(.name)"' <<<"$meta")

declare -A pick=()
all=0
while IFS= read -r f; do
  case "$f" in
    Cargo.toml | Cargo.lock | .cargo/*) all=1 ;;
    docs/*) pick[strand-dev]=1; pick[strand-compiler]=1; pick[strand]=1 ;;
    crates/*)
      while read -r dir name; do
        case "$f" in "$dir"/*) pick[$name]=1 ;; esac
      done <<<"$members"
      ;;
  esac
done <<<"$changed"

if [ "$all" = 1 ]; then
  while read -r _ name; do pick[$name]=1; done <<<"$members"
elif [ "$deep" = 1 ] && [ "${#pick[@]}" -gt 0 ]; then
  # Reverse dependents, to a fixed point, over the resolved graph.
  while :; do
    seeds=$(printf '%s\n' "${!pick[@]}" | jq -R . | jq -s .)
    more=$(jq -r --argjson seeds "$seeds" '
      . as $m
      | ($m.packages | map({key: .id, value: .name}) | from_entries) as $names
      | $m.resolve.nodes[]
      | select(.id as $id | $m.workspace_members | index($id))
      | select([.deps[].pkg | $names[.]] | any(. as $d | $seeds | index($d)))
      | $names[.id]' <<<"$meta" | sort -u)
    grew=0
    for name in $more; do
      [ -n "${pick[$name]:-}" ] || { pick[$name]=1; grew=1; }
    done
    [ "$grew" = 1 ] || break
  done
fi

if [ "${#pick[@]}" -eq 0 ]; then
  echo "affected: no crate changed since $base ($mb); ci covers the rest"
  exit 0
fi

crates=$(printf '%s\n' "${!pick[@]}" | sort)
echo "affected since $base ($(git rev-parse --short "$mb" 2>/dev/null || echo "$mb")): $(echo $crates)"
[ "$list" = 1 ] && exit 0
args=()
for c in $crates; do args+=(-p "$c"); done
exec cargo test "${args[@]}" -- "${test_args[@]}"
