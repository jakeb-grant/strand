#!/usr/bin/env bash
# The mutants (docs/eval.md, "Mutants"): the proof that the regression
# scenarios catch the bugs they are named for. In one detached worktree
# of HEAD (target/eval/mutants/tree), each line of eval/mutants/mutants.txt
# runs its scenario on HEAD, which must pass, then applies its patch (a
# fix reverted), runs the scenario again, which must fail with at least
# the expected checks, and reverts the patch. Runs are serial; the
# worktree is removed at the end. A patch that no longer applies is
# reported STALE (regenerate it), not as a product failure. Each mutant
# leaves an A/B page of HEAD against it, every frame scrubbable:
# target/eval/mutants/<name>-ab/index.html.
#
# Usage: scripts/eval/mutants.sh [NAME...]   (NAME: a patch without .patch;
#        default: every line)
# In the container: scripts/container/run.sh eval mutants [NAME...]
# It runs HEAD: commit what it should see first (uncommitted changes
# under crates/ and eval/ are named, and not in it).

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
LIST=eval/mutants/mutants.txt
BASE=target/eval/mutants
TREE=$BASE/tree
want=("$@")

if [ -n "$(git status --porcelain -- crates eval)" ]; then
  echo "NOTE: uncommitted changes under crates/ or eval/ are not in the mutants (they run HEAD $(git rev-parse --short HEAD))"
fi

# forget_tree TREE: TREE removed with its worktree entry, and no other.
# shellcheck source=scripts/eval/worktree.sh
source "$ROOT/scripts/eval/worktree.sh"

remove_tree() {
  remove_tree_at "$TREE"
}

# check SCENARIO OUT: strand-eval check from the worktree's own build;
# prints its exit status (0 pass, 1 a check failed, 2 the run failed).
check() {
  cargo run -q --locked --manifest-path "$TREE/eval/Cargo.toml" --bin strand-eval -- \
    check "$1" --frames all --out "$2" >"$2.log" 2>&1
  echo $?
}

# ab HEAD_OUT MUTANT_OUT NAME: the A/B page of the two runs, made by the
# checkout's own strand-eval (it reads reports; no product code differs).
ab() {
  cargo run -q --locked --manifest-path eval/Cargo.toml --bin strand-eval -- \
    ab "$1" "$2" -o "$2-ab" --label-a "HEAD $(git rev-parse --short=8 HEAD)" \
    --label-b "mutant $3 (fix reverted)" >"$2-ab.log" 2>&1 &&
    echo "  A/B page: $2-ab/index.html" ||
    echo "  (no A/B page: $2-ab.log)"
}

# unmatched LOG SPECS...: every SPEC is `unmatched:SELECTOR` and the run
# stopped because SELECTOR matched no node (the scenario error a lost
# name gives: docs/eval.md, Selectors).
unmatched() {
  local log=$1 spec sel
  shift
  [ "$#" -gt 0 ] || return 1
  for spec in "$@"; do
    case "$spec" in unmatched:*) ;; *) return 1 ;; esac
    sel=${spec#unmatched:}
    grep -qF "\`$sel\` matched no node" "$log" || return 1
  done
}

# expected SUMMARY ID SPEC: whether summary.txt shows the failure SPEC.
expected() {
  local summary=$1 id=$2 spec=$3 check node axis
  IFS=: read -r check node axis <<<"$spec"
  grep -qxF "$id $check: FAIL" "$summary" || return 1
  [ -n "$node" ] || return 0
  local label="[a-z_-]+${node//./\\.}"
  # `k12 x 33.3 …` for a finding on an axis with a value, `k54: …` for
  # one with neither (twin, expect). A rule check's finding may name no
  # segment (`continuity`'s are per spring, not per segment).
  local re="^  $check (\\[[^]]*\\] )?$label \\([^)]*\\) k[0-9]+"
  if [ -n "$axis" ]; then re+=" $axis "; else re+="[ :]"; fi
  grep -qE "$re" "$summary"
}

mkdir -p "$BASE"
remove_tree
trap remove_tree EXIT
git worktree add -q --detach "$TREE" HEAD || { echo "mutants: no worktree at $TREE"; exit 2; }

failed=0
ran=0
declare -A control
declare -A control_out
while read -r patch scenario specs; do
  case "$patch" in "" | \#*) continue ;; esac
  name=${patch%.patch}
  if [ "${#want[@]}" -gt 0 ] && [[ ! " ${want[*]} " =~ " $name " ]]; then
    continue
  fi
  ran=$((ran + 1))
  scn="$TREE/eval/scenarios/$scenario"
  id=$(sed -n 's/^id *= *"\(.*\)".*/\1/p' "$scn" | head -n1)
  out="$BASE/$name"
  rm -rf "$out" "$out-head" "$out-ab"
  echo "=== mutant $name: $scenario ($id)"
  # The control: the scenario passes on HEAD.
  if [ -z "${control[$scenario]:-}" ]; then
    rc=$(check "$scn" "$out-head")
    control[$scenario]=$rc
    control_out[$scenario]="$out-head"
    if [ "$rc" != 0 ]; then
      echo "FAIL: $scenario does not pass on HEAD (exit $rc): $out-head.log"
      sed -n '1,40p' "$out-head.log"
      failed=1
      continue
    fi
    echo "  HEAD: pass"
  fi
  if ! git -C "$TREE" apply --check "$ROOT/eval/mutants/$patch" 2>/dev/null; then
    echo "STALE: $patch no longer applies to HEAD: regenerate it (docs/eval.md, Mutants)"
    failed=1
    continue
  fi
  git -C "$TREE" apply "$ROOT/eval/mutants/$patch"
  rc=$(check "$scn" "$out")
  git -C "$TREE" apply -R "$ROOT/eval/mutants/$patch"
  [ "$rc" = 2 ] || ab "${control_out[$scenario]}" "$out" "$name"
  case "$rc" in
    0)
      echo "SURVIVED: $scenario passes with $patch applied"
      failed=1
      continue
      ;;
    1) ;;
    *)
      # shellcheck disable=SC2086 # the specs are words
      if [ "$rc" = 2 ] && unmatched "$out.log" $specs; then
        grep -F "matched no node" "$out.log" | head -n 3 | sed 's/^/  /'
        echo "  caught: $specs"
        continue
      fi
      echo "ERROR: $scenario did not run with $patch applied (exit $rc): $out.log"
      sed -n '1,40p' "$out.log"
      failed=1
      continue
      ;;
  esac
  missing=()
  for spec in $specs; do
    expected "$out/summary.txt" "$id" "$spec" || missing+=("$spec")
  done
  grep -E "^$id [a-z-]+: FAIL|^  " "$out/summary.txt" | sed 's/^/  /'
  if [ "${#missing[@]}" -gt 0 ]; then
    echo "WEAK: $name fails, but not with ${missing[*]}"
    failed=1
  else
    echo "  caught: $specs"
  fi
done <"$LIST"

if [ "$ran" = 0 ]; then
  echo "mutants: no line of $LIST matches ${want[*]:-anything}"
  exit 2
fi
if [ "$failed" != 0 ]; then
  echo "=== mutants: FAILED"
  exit 1
fi
echo "=== mutants: every mutant caught ($ran)"
