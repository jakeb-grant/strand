#!/usr/bin/env bash
# A/B across builds (docs/eval.md, "A/B"): runs the checkout's scenarios
# on revision A and on revision B (default: the checkout as it is, its
# uncommitted changes included) and makes one viewer page of the two,
# nodes matched by key, the first frame each differs at listed.
#
# Each revision is built serially in a detached worktree under
# target/eval/ab/ into the shared target directory (one build at a time;
# the worktree is removed afterwards). Only revisions that have eval/
# can run: older ones have no sandbox. A revision's strand-eval holds a
# committed scenario to its own eval/ (`eval_root`, its build's
# manifest directory), so the checkout's eval/scenarios and
# eval/fixtures are copied into the revision's tree and the scenarios
# named under eval/ are read from there.
#
# Usage: scripts/eval/ab.sh <revA> [<revB>] [-- <scenario.scn|dir>...]
#        (default scenarios: eval/scenarios/curated eval/scenarios/regressions)
# In the container: scripts/container/run.sh eval ab-revs <revA> [<revB>] ...
# Output: target/eval/ab/<A>-vs-<B>/index.html

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
BASE=target/eval/ab
revs=()
scen=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --) shift; scen=("$@"); break ;;
    -h | --help) sed -n '2,17p' "$0"; exit 0 ;;
    *) revs+=("$1"); shift ;;
  esac
done
if [ "${#revs[@]}" -lt 1 ] || [ "${#revs[@]}" -gt 2 ]; then
  sed -n '2,17p' "$0"
  exit 2
fi
[ "${#scen[@]}" -gt 0 ] || scen=(eval/scenarios/curated eval/scenarios/regressions)
mkdir -p "$BASE"

# forget_tree TREE: TREE removed with its worktree entry, and no other.
# shellcheck source=scripts/eval/worktree.sh
source "$ROOT/scripts/eval/worktree.sh"

# run REV OUT: the scenarios on REV ("" for the checkout) into OUT;
# check's exit 1 (a scenario failed) still makes a report.
run() {
  local rev=$1 out=$2 manifest=eval/Cargo.toml tree="" rc s
  local args=("${scen[@]}")
  rm -rf "$out"
  if [ -n "$rev" ]; then
    tree=$BASE/tree-$rev
    if ! git cat-file -e "$rev:eval/Cargo.toml" 2>/dev/null; then
      echo "ab: $rev has no eval/ (A/B needs a revision with the sandbox)"
      return 2
    fi
    forget_tree "$tree"
    git worktree add -q --detach "$tree" "$rev" || { echo "ab: no worktree at $tree"; return 2; }
    manifest=$tree/eval/Cargo.toml
    # The checkout's scenarios, inside the revision's eval/.
    for d in scenarios fixtures; do
      if [ -d "eval/$d" ]; then
        mkdir -p "$tree/eval/$d"
        cp -R "eval/$d/." "$tree/eval/$d/"
      fi
    done
    args=()
    for s in "${scen[@]}"; do
      case "$s" in
        eval/*) args+=("$tree/$s") ;;
        "$ROOT"/eval/*) args+=("$tree/${s#"$ROOT"/}") ;;
        *) args+=("$s") ;;
      esac
    done
  fi
  echo "=== ab: ${rev:-the checkout} -> $out"
  cargo run -q --locked --manifest-path "$manifest" --bin strand-eval -- \
    check --frames all --out "$out" "${args[@]}" >"$out.log" 2>&1
  rc=$?
  tail -n 1 "$out.log"
  if [ -n "$tree" ]; then
    forget_tree "$tree"
  fi
  [ "$rc" -le 1 ] || { echo "ab: the run on ${rev:-the checkout} failed (exit $rc): $out.log"; return 2; }
}

a=$(git rev-parse --short=8 "${revs[0]}^{commit}") || exit 2
if [ "${#revs[@]}" = 2 ]; then
  b=$(git rev-parse --short=8 "${revs[1]}^{commit}") || exit 2
  bname=$b
else
  b=""
  bname="checkout"
fi
run "$a" "$BASE/run-$a" || exit 2
run "$b" "$BASE/run-$bname" || exit 2
out=$BASE/$a-vs-$bname
cargo run -q --locked --manifest-path eval/Cargo.toml --bin strand-eval -- \
  ab "$BASE/run-$a" "$BASE/run-$bname" -o "$out" --label-a "$a" \
  --label-b "$([ -n "$b" ] && echo "$b" || echo "checkout $(git rev-parse --short=8 HEAD)+")"
