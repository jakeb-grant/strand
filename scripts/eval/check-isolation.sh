#!/usr/bin/env bash
# The evaluation sandbox stays out of the product (docs/decisions.md
# eval-sandbox; docs/architecture.md, "Crate graph"):
#
#   (a) no Cargo.toml of the root workspace (the root's, crates/**) names
#       strand-eval or a path into eval/, or eval-hooks, except
#       strand-render's own `eval-hooks = []` feature line; and the
#       root's members do not reach eval/;
#   (b) the root workspace's resolution never enables strand-render's
#       eval-hooks (`cargo tree --workspace -e features -i strand-render`);
#   (c) every cfg(feature = "eval-hooks") under crates/ is in
#       crates/strand-render/src (the count is printed, so growth shows in
#       review).
#
# Usage: scripts/eval/check-isolation.sh [ROOT]   (default: this checkout)
# Env:   EVAL_ISOLATION_TREE=FILE reads (b)'s cargo tree output from FILE
#        instead of running cargo (the self-test,
#        scripts/eval/test-check-isolation.sh).

set -uo pipefail

ROOT=${1:-$(cd "$(dirname "$0")/../.." && pwd)}
cd "$ROOT" || exit 2
bad=0
no() { echo "check-isolation: $*"; bad=1; }

# (a)
manifests=(Cargo.toml)
while IFS= read -r f; do manifests+=("$f"); done < <(find crates -name Cargo.toml -not -path '*/target/*' | sort)
for f in "${manifests[@]}"; do
  while IFS= read -r hit; do
    no "(a) $f:$hit names strand-eval"
  done < <(grep -n 'strand-eval' "$f" | grep -v '^[0-9]*:[[:space:]]*#')
  while IFS= read -r hit; do
    no "(a) $f:$hit has a path into eval/"
  done < <(grep -nE 'path[[:space:]]*=[[:space:]]*"([^"]*/)?eval(/[^"]*)?"' "$f")
  while IFS= read -r hit; do
    line=${hit#*:}
    if [ "$f" = crates/strand-render/Cargo.toml ] && [ "$(tr -d '[:space:]' <<<"$line")" = 'eval-hooks=[]' ]; then
      continue
    fi
    no "(a) $f:$hit names eval-hooks"
  done < <(grep -n 'eval-hooks' "$f" | grep -v '^[0-9]*:[[:space:]]*#')
done
# The root's members reach eval/ (a glob such as "*" or "eval").
members=$(awk '/^members[[:space:]]*=/,/\]/' Cargo.toml)
if grep -qE '"(\*|eval(/[^"]*)?|\./eval)"' <<<"$members"; then
  no "(a) the root Cargo.toml's members reach eval/: $members"
fi

# (b)
if [ -n "${EVAL_ISOLATION_TREE:-}" ]; then
  tree=$(cat "$EVAL_ISOLATION_TREE")
else
  tree=$(cargo tree --workspace -e features -i strand-render 2>&1) || {
    no "(b) cargo tree failed: $tree"
    tree=
  }
fi
if ! grep -q 'strand-render' <<<"$tree"; then
  no "(b) cargo tree did not list strand-render's features"
fi
if grep -q 'eval-hooks' <<<"$tree"; then
  no "(b) the root resolution enables eval-hooks:"
  grep 'eval-hooks' <<<"$tree" | sed 's/^/     /'
fi

# (c)
count=0
while IFS= read -r hit; do
  count=$((count + 1))
  case "$hit" in
    crates/strand-render/src/*) ;;
    *) no "(c) $hit: eval-hooks code outside crates/strand-render/src" ;;
  esac
done < <(grep -rnE 'feature[[:space:]]*=[[:space:]]*"eval-hooks"' crates --include='*.rs' 2>/dev/null)
echo "check-isolation: $count cfg(feature = \"eval-hooks\") sites, all expected under crates/strand-render/src"

[ "$bad" = 0 ] && echo "check-isolation: eval/ is isolated from the root workspace"
exit "$bad"
