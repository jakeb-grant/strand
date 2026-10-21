#!/usr/bin/env bash
# Tests scripts/eval/worktree.sh's forget_tree on a toy repository: a
# tree reached through a symlink, and one whose directory is already gone,
# is forgotten; another worktree whose directory cannot be seen (as in the
# container, where only this checkout is mounted) keeps its entry; the
# path can be added again at once; another checkout's tree at the same
# relative path is never the one removed; a failed mutants control is
# kept (keep_control).
# Run: scripts/container/run.sh bash scripts/eval/test-worktree.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0

ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }
# registered PATH: git lists a worktree at PATH (its real path).
registered() {
  git worktree list --porcelain | grep -qxF "worktree $(realpath -m -- "$1")"
}

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
git init -q "$DIR/repo"
cd "$DIR/repo" || exit 2
git -c user.name=t -c user.email=t@t commit -q --allow-empty -m one
# shellcheck source=scripts/eval/worktree.sh
source "$HERE/worktree.sh"

mkdir -p "$DIR/real"
ln -s "$DIR/real" "$DIR/link"

# Another worktree whose directory this process cannot see.
git worktree add -q --detach "$DIR/elsewhere/other" HEAD
mv "$DIR/elsewhere" "$DIR/hidden"

# 1. A tree through a symlink, still there.
git worktree add -q --detach "$DIR/link/t1" HEAD
forget_tree "$DIR/link/t1"
if registered "$DIR/link/t1" || [ -e "$DIR/real/t1" ]; then no "a tree through a symlink is forgotten"; else ok "a tree through a symlink is forgotten"; fi

# 2. A tree through a symlink whose directory went first.
git worktree add -q --detach "$DIR/link/t2" HEAD
rm -rf "$DIR/real/t2"
forget_tree "$DIR/link/t2"
if registered "$DIR/link/t2"; then no "a gone tree through a symlink is forgotten"; else ok "a gone tree through a symlink is forgotten"; fi
if git worktree add -q --detach "$DIR/link/t2" HEAD 2>"$DIR/err"; then ok "its path is added again"; else no "its path is added again: $(cat "$DIR/err")"; fi
forget_tree "$DIR/link/t2"

# 3. A relative tree whose parent directory is gone (a deleted target/).
git worktree add -q --detach target/eval/tree HEAD
rm -rf target
forget_tree target/eval/tree
if registered "$DIR/repo/target/eval/tree"; then no "a relative tree under a deleted directory is forgotten"; else ok "a relative tree under a deleted directory is forgotten"; fi
if git worktree add -q --detach target/eval/tree HEAD 2>"$DIR/err"; then ok "and added again"; else no "and added again: $(cat "$DIR/err")"; fi
forget_tree target/eval/tree

# 4. remove_tree_at, as mutants.sh runs it before and after: its tree's
# directory gone with target/, the entry still goes.
git worktree add -q --detach target/eval/mutants/tree HEAD
rm -rf target
remove_tree_at target/eval/mutants/tree
if git worktree add -q --detach target/eval/mutants/tree HEAD 2>"$DIR/err"; then ok "a tree removed after its directory went is added again"; else no "a tree removed after its directory went is added again: $(cat "$DIR/err")"; fi
echo change >target/eval/mutants/tree/file
remove_tree_at target/eval/mutants/tree
if registered "$DIR/repo/target/eval/mutants/tree" || [ -e target/eval/mutants/tree ]; then no "a tree with changes is removed"; else ok "a tree with changes is removed"; fi

# 5. The worktree it could not see is still registered.
if registered "$DIR/elsewhere/other"; then ok "another worktree keeps its entry"; else no "another worktree keeps its entry"; fi

# 6. (replay-owner) Two checkouts of the repository, each with its own
# target/eval/mutants/tree: from A, whose tree is not registered (a run's
# cleanup at its start), with B's checkout out of sight (as in A's
# container), remove_tree_at and forget_tree touch only A's path. Git
# matches a relative path as a suffix of every worktree's, so the
# relative one named B's.
git worktree add -q "$DIR/A" -b a HEAD
git worktree add -q "$DIR/B" -b b HEAD
git -C "$DIR/B" worktree add -q --detach target/eval/mutants/tree HEAD
mv "$DIR/B" "$DIR/hiddenB"
(cd "$DIR/A" && remove_tree_at target/eval/mutants/tree && forget_tree target/eval/mutants/tree)
mv "$DIR/hiddenB" "$DIR/B"
if registered "$DIR/B/target/eval/mutants/tree" && [ -f "$DIR/B/target/eval/mutants/tree/.git" ]; then
  ok "another checkout's tree of the same name keeps its entry"
else
  no "another checkout's tree of the same name keeps its entry: $(git worktree list)"
fi
# And A's own, registered, goes alone.
git -C "$DIR/A" worktree add -q --detach target/eval/mutants/tree HEAD
(cd "$DIR/A" && remove_tree_at target/eval/mutants/tree)
if registered "$DIR/A/target/eval/mutants/tree"; then no "A's own tree is removed"; else ok "A's own tree is removed"; fi
if registered "$DIR/B/target/eval/mutants/tree"; then ok "B's is still there"; else no "B's is still there"; fi

# 7. (replay-owner) keep_control: a failed control run kept beside the
# next run's, with its log and the binary's hash; a run's clearing of
# OUT leaves it.
mkdir -p "$DIR/m/x-head"
echo frame >"$DIR/m/x-head/k7.png"
echo "control log" >"$DIR/m/x-head.log"
echo binary >"$DIR/m/strand-eval"
kept=$(keep_control "$DIR/m/x-head" "$DIR/m/strand-eval")
case "$kept" in "$DIR/m/x-head.failed-"*) ok "the failed control is kept as OUT.failed-<time>" ;; *) no "the failed control is kept as OUT.failed-<time>: $kept" ;; esac
[ -f "$kept/k7.png" ] && grep -q "control log" "$kept/check.log" && ok "with its frames and its log" || no "with its frames and its log"
grep -q "$(sha256sum "$DIR/m/strand-eval" | cut -d' ' -f1)" "$kept/strand-eval.sha256" && ok "and the binary's hash" || no "and the binary's hash: $(cat "$kept/strand-eval.sha256")"
rm -rf "$DIR/m/x-head" "$DIR/m/x-head.log"
[ -d "$kept" ] && ok "clearing OUT leaves it" || no "clearing OUT leaves it"

[ "$bad" = 0 ] && echo "test-worktree: all passed"
exit "$bad"
