# shellcheck shell=bash
# Sourced by scripts/eval/ab.sh and mutants.sh: their detached worktrees.
#
# forget_tree TREE: TREE removed with its worktree entry, and no other.
# Never `git worktree prune`: in the container only this checkout (and
# the common git dir) is mounted, so prune forgets every other worktree
# on the laptop, whose directories it cannot see (docs/decisions.md
# eval-refs). The entry is matched by the path git recorded for it, its
# real path (symlinks resolved), so a tree reached through a symlink, or
# one whose directory is already gone (a deleted target/), is forgotten
# too.
forget_tree() {
  local tree=$1 abs admin
  git worktree remove --force "$tree" 2>/dev/null && return 0
  abs=$(realpath -m -- "$tree")
  rm -rf "$tree"
  for admin in "$(git rev-parse --path-format=absolute --git-common-dir)"/worktrees/*; do
    [ -f "$admin/gitdir" ] || continue
    if [ "$(realpath -m -- "$(cat "$admin/gitdir")")" = "$abs/.git" ]; then
      rm -rf "$admin"
    fi
  done
  return 0
}

# remove_tree_at TREE: TREE gone with its entry, its changes dropped;
# the entry goes even when the directory went first (a deleted target/),
# so the next `git worktree add` there finds nothing registered.
remove_tree_at() {
  local tree=$1
  if [ ! -e "$tree" ]; then
    forget_tree "$tree"
    return 0
  fi
  git -C "$tree" checkout -q -- . 2>/dev/null
  git worktree remove "$tree" 2>/dev/null || forget_tree "$tree"
}
