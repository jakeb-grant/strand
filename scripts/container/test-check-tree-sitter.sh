#!/usr/bin/env bash
# Tests scripts/check-tree-sitter.sh on a toy grammar, and that the
# Dockerfile and ci.yml's lint job pin the same tree-sitter CLI.
# Run: scripts/container/run.sh scripts/container/test-check-tree-sitter.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
CHECK=$ROOT/scripts/check-tree-sitter.sh
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0

ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }
# expect WANT NAME CMD...: CMD exits WANT.
expect() {
  local want=$1 name=$2 got=0
  shift 2
  "$@" >"$DIR/out" 2>&1 || got=$?
  if [ "$got" = "$want" ]; then ok "$name"; else no "$name: exit $got, wanted $want"; sed 's/^/     /' "$DIR/out"; fi
}

# The pins agree.
d_ver=$(sed -n 's/^ARG TREE_SITTER_VERSION=//p' "$HERE/Dockerfile")
d_sha=$(sed -n 's/^ARG TREE_SITTER_SHA256=//p' "$HERE/Dockerfile")
c_ver=$(sed -n 's/^ *TREE_SITTER_VERSION: *"\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' "$ROOT/.github/workflows/ci.yml")
c_sha=$(sed -n 's/^ *TREE_SITTER_SHA256: *"\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' "$ROOT/.github/workflows/ci.yml")
if [ -n "$d_ver" ] && [ "$d_ver" = "$c_ver" ] && [ -n "$d_sha" ] && [ "$d_sha" = "$c_sha" ]; then
  ok "Dockerfile and ci.yml pin tree-sitter $d_ver with the same sha256"
else
  no "the pins differ: Dockerfile $d_ver $d_sha, ci.yml $c_ver $c_sha"
fi
case "$(grep -c 'TREE_SITTER_SHA256:' "$ROOT/.github/workflows/ci.yml")" in
  1) ok "ci.yml fetches the CLI in one job" ;;
  *) no "ci.yml names TREE_SITTER_SHA256 more than once (the CLI belongs to the lint job only)" ;;
esac

PKG=$DIR/pkg
mkdir -p "$PKG"
expect 0 "no grammar.js yet: nothing to check" "$CHECK" "$PKG"

cat >"$PKG/grammar.js" <<'JS'
module.exports = grammar({
  name: 'toy',
  extras: $ => [/\s/],
  rules: {
    source_file: $ => repeat($.binding),
    binding: $ => seq('let', $.identifier, '=', $.number),
    identifier: _ => /[a-z]+/,
    number: _ => /\d+/,
  },
});
JS
(cd "$PKG" && tree-sitter generate --js-runtime native) >"$DIR/gen" 2>&1 || { no "toy grammar generates"; cat "$DIR/gen"; }
expect 0 "freshly generated files match" "$CHECK" "$PKG"

cp "$PKG/src/parser.c" "$DIR/parser.c"
echo "/* edited */" >>"$PKG/src/parser.c"
expect 1 "a hand-edited parser.c is caught" "$CHECK" "$PKG"
cp "$DIR/parser.c" "$PKG/src/parser.c"

cp "$PKG/grammar.js" "$DIR/grammar.js"
sed -i "s/'let'/'var'/" "$PKG/grammar.js"
expect 1 "grammar.js changed without regenerating is caught" "$CHECK" "$PKG"
cp "$DIR/grammar.js" "$PKG/grammar.js"

mv "$PKG/src/grammar.json" "$DIR/grammar.json"
expect 1 "a missing grammar.json is caught" "$CHECK" "$PKG"
mv "$DIR/grammar.json" "$PKG/src/grammar.json"
expect 0 "restored files match again" "$CHECK" "$PKG"

echo "/* stale */" >"$PKG/src/tree_sitter/stale.h"
expect 1 "a committed header generate no longer writes is caught" "$CHECK" "$PKG"
rm "$PKG/src/tree_sitter/stale.h"

echo "/* scanner */" >"$PKG/src/scanner.c"
expect 0 "a hand-written src/scanner.c beside the generated files is kept" "$CHECK" "$PKG"
rm "$PKG/src/scanner.c"

mkdir -p "$DIR/fake"
printf '#!/bin/sh\necho "tree-sitter 0.0.1"\n' >"$DIR/fake/tree-sitter"
chmod +x "$DIR/fake/tree-sitter"
expect 2 "a CLI other than the pinned one is refused" env PATH="$DIR/fake:$PATH" "$CHECK" "$PKG"

[ "$bad" = 0 ] && echo "test-check-tree-sitter: all passed"
exit "$bad"
