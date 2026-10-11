#!/usr/bin/env bash
# M5 wave-0 spike 2 (decisions.md m5-infra): fetches the pinned
# tree-sitter CLI (the same release and sha256 as
# scripts/container/Dockerfile), then runs inside.sh in run.sh's image.
# Usage: spikes/tree-sitter-cli/run.sh
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
VER=0.27.0
SHA=20a1f39ec1c45f2211492dcb8881c802b643b554bb196869a29ac3778277fa77
OUT=$ROOT/target/spike-tree-sitter
mkdir -p "$OUT"
curl -sSfL -o "$OUT/ts.gz" "https://github.com/tree-sitter/tree-sitter/releases/download/v$VER/tree-sitter-linux-x64.gz"
echo "$SHA  $OUT/ts.gz" | sha256sum -c -
gunzip -f "$OUT/ts.gz" && chmod +x "$OUT/ts"
"$ROOT/scripts/container/run.sh" bash "$HERE/inside.sh" "$OUT/ts" "$OUT/work"
# The same grammar through node (a node:22 trixie image, glibc 2.41: the CLI needs 2.39; the CI image has no
# node), to compare grammar.json and parser.c with the native runtime's.
if [ "${SPIKE_NODE:-1}" = 1 ]; then
  rm -rf "$OUT/node" && mkdir -p "$OUT/node" && cp "$HERE/grammar/grammar.js" "$OUT/node/"
  docker run --rm --user "$(id -u):$(id -g)" -v "$OUT:$OUT" -w "$OUT/node" node:22-trixie-slim \
    "$OUT/ts" generate
  for f in grammar.json parser.c node-types.json; do
    cmp "$OUT/node/src/$f" "$OUT/work/src/$f" && echo "node and native agree on src/$f"
  done
fi
