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
# too. Git is always given the real, absolute path: a relative one is
# first matched as a suffix of every worktree's path, so this checkout's
# target/eval/mutants/tree, unregistered, would name another checkout's
# (docs/decisions.md replay-owner).
forget_tree() {
  local tree=$1 abs admin
  abs=$(realpath -m -- "$tree")
  git worktree remove --force "$abs" 2>/dev/null && return 0
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
  git worktree remove "$(realpath -m -- "$tree")" 2>/dev/null || forget_tree "$tree"
}

# keep_control OUT BIN: a mutants control run that failed on HEAD kept
# beside the next run's, as OUT.failed-<UTC time>, with the sha256 of the
# strand-eval binary that ran it (BIN), so a recurrence can be held to
# it; mutants.sh clears OUT at each run, never what this keeps. Prints
# where it went.
keep_control() {
  local out=$1 bin=$2 kept
  kept=$out.failed-$(date -u +%Y%m%dT%H%M%SZ)
  [ -e "$out" ] || mkdir -p "$out"
  mv "$out" "$kept"
  [ -f "$out.log" ] && cp "$out.log" "$kept/check.log"
  if [ -f "$bin" ]; then
    sha256sum "$bin" >"$kept/strand-eval.sha256"
  else
    echo "no binary at $bin" >"$kept/strand-eval.sha256"
  fi
  echo "$kept"
}
