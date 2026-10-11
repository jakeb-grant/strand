#!/usr/bin/env bash
# M5 wave-0 spike 2 (decisions.md m5-infra), inside run.sh's image (no
# node installed): can the pinned prebuilt tree-sitter CLI run
# `generate` from grammar.js, and from grammar.json?
# Usage: inside.sh TREE_SITTER_BIN WORKDIR
set -uo pipefail
TS=$1 W=$2
rm -rf "$W" && mkdir -p "$W" && cp "$(dirname "$0")/grammar/grammar.js" "$W/"
cd "$W"
echo "node: $(command -v node || echo none) · deno: $(command -v deno || echo none) · bun: $(command -v bun || echo none)"
"$TS" --version
echo "== generate (grammar.js, default runtime)"
"$TS" generate 2>&1; echo "exit $?"; ls src 2>/dev/null
echo "== generate --js-runtime native"
"$TS" generate --js-runtime native 2>&1; echo "exit $?"; ls -l src/parser.c src/grammar.json 2>/dev/null
cp src/grammar.json grammar.json.kept 2>/dev/null
echo "== generate src/grammar.json (no JS at all)"
rm -f src/parser.c
"$TS" generate src/grammar.json 2>&1; echo "exit $?"; ls -l src/parser.c 2>/dev/null
echo "== generate is deterministic: regenerate and diff"
cp src/parser.c parser.c.1 && "$TS" generate src/grammar.json >/dev/null 2>&1 && cmp parser.c.1 src/parser.c && echo "parser.c identical"
echo "== abi"; grep -m1 'LANGUAGE_VERSION' src/parser.c
echo "== parse with the generated parser (cc needed)"
printf 'let a = 1 + b\npanel p { let c = 2 }\n' >t.spike
"$TS" parse t.spike 2>&1 | head -20; echo "exit ${PIPESTATUS[0]}"
