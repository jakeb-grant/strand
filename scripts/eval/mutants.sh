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
# Never `git worktree prune`: in the container only this checkout (and
# the common git dir) is mounted, so prune forgets every other worktree
# on the laptop, whose directories it cannot see (docs/decisions.md
# eval-refs).
forget_tree() {
  local tree=$1 abs admin
  git worktree remove --force "$tree" 2>/dev/null && return 0
  abs=$(cd "$(dirname "$tree")" 2>/dev/null && pwd)/$(basename "$tree")
  rm -rf "$tree"
  for admin in "$(git rev-parse --path-format=absolute --git-common-dir)"/worktrees/*; do
    if [ "$(cat "$admin/gitdir" 2>/dev/null)" = "$abs/.git" ]; then
      rm -rf "$admin"
    fi
  done
  return 0
}

remove_tree() {
  [ -e "$TREE" ] || return 0
  git -C "$TREE" checkout -q -- . 2>/dev/null
  git worktree remove "$TREE" 2>/dev/null || forget_tree "$TREE"
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

# expected SUMMARY ID SPEC: whether summary.txt shows the failure SPEC.
expected() {
  local summary=$1 id=$2 spec=$3 check node axis
  IFS=: read -r check node axis <<<"$spec"
  grep -qxF "$id $check: FAIL" "$summary" || return 1
  [ -n "$node" ] || return 0
  local label="[a-z_-]+${node//./\\.}"
  local re="^  $check \\[[^]]*\\] $label \\([^)]*\\) k[0-9]+ "
  [ -n "$axis" ] && re+="$axis "
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
