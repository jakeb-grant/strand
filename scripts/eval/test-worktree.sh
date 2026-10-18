#!/usr/bin/env bash
# Tests scripts/eval/worktree.sh's forget_tree on a toy repository: a
# tree reached through a symlink, and one whose directory is already gone,
# is forgotten; another worktree whose directory cannot be seen (as in the
# container, where only this checkout is mounted) keeps its entry; the
# path can be added again at once.
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

[ "$bad" = 0 ] && echo "test-worktree: all passed"
exit "$bad"
