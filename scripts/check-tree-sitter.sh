#!/usr/bin/env bash
# The tree-sitter grammar's generated files are committed
# (crates/strand-dev/tree-sitter-strand/src/{parser.c, grammar.json,
# node-types.json, tree_sitter/}; docs/architecture.md, "tree-sitter
# layout"). This regenerates them from grammar.js in a scratch copy with
# the pinned CLI and fails on any difference, so they never drift from
# grammar.js. `--js-runtime native` runs grammar.js on the CLI's
# built-in QuickJS: no node (decisions.md m5-infra, spike 2).
#
# Usage: scripts/check-tree-sitter.sh [PACKAGE_DIR]
#        (default crates/strand-dev/tree-sitter-strand; CI's lint job
#        and scripts/container/run.sh ci run it)
# Exit:  0 up to date (or no grammar.js yet), 1 out of date, 2 no CLI or
#        the wrong version.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PKG=${1:-$ROOT/crates/strand-dev/tree-sitter-strand}

if [ ! -f "$PKG/grammar.js" ]; then
  echo "check-tree-sitter: no $PKG/grammar.js yet; nothing to check"
  exit 0
fi
command -v tree-sitter >/dev/null || {
  echo "check-tree-sitter: no tree-sitter CLI on PATH (scripts/container/Dockerfile pins it)" >&2
  exit 2
}
want=$(sed -n 's/^ARG TREE_SITTER_VERSION=//p' "$ROOT/scripts/container/Dockerfile")
got=$(tree-sitter --version | awk '{print $2}')
[ "$got" = "$want" ] || {
  echo "check-tree-sitter: tree-sitter $got on PATH, but the pinned CLI is $want" >&2
  exit 2
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cp -r "$PKG" "$tmp/pkg"
# Generate into a clean output: drop the four generated paths first, so a
# committed file the CLI no longer writes (a header a newer CLI renamed,
# say) is compared against nothing and caught, instead of against its own
# copy. The rest of src/ (an external scanner.c) stays for generate.
for f in parser.c grammar.json node-types.json tree_sitter; do
  rm -rf "$tmp/pkg/src/$f"
done
(cd "$tmp/pkg" && tree-sitter generate --js-runtime native) >"$tmp/generate.log" 2>&1 || {
  cat "$tmp/generate.log" >&2
  echo "check-tree-sitter: tree-sitter generate failed on $PKG/grammar.js" >&2
  exit 1
}
stale=0
for f in parser.c grammar.json node-types.json tree_sitter; do
  if [ -e "$tmp/pkg/src/$f" ] || [ -e "$PKG/src/$f" ]; then
    if ! diff -r -q "$PKG/src/$f" "$tmp/pkg/src/$f" >/dev/null 2>&1; then
      echo "check-tree-sitter: src/$f differs from what grammar.js generates" >&2
      stale=1
    fi
  fi
done
if [ "$stale" = 1 ]; then
  echo "check-tree-sitter: run \`tree-sitter generate --js-runtime native\` in $PKG (tree-sitter $want) and commit src/" >&2
  exit 1
fi
echo "check-tree-sitter: $PKG/src matches grammar.js (tree-sitter $got)"
