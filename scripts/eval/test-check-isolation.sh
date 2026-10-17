#!/usr/bin/env bash
# Tests scripts/eval/check-isolation.sh on a toy checkout.
# Run: scripts/container/run.sh bash scripts/eval/test-check-isolation.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
CHECK=$HERE/check-isolation.sh
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

T=$DIR/tree
CLEAN=$DIR/clean.txt
cat >"$CLEAN" <<'TXT'
strand-render v0.0.0 (/x/crates/strand-render)
├── strand-render feature "default"
│   └── strand v0.0.0 (/x/crates/strand)
└── strand-render feature "gpu"
    └── strand v0.0.0 (/x/crates/strand)
TXT
HOOKED=$DIR/hooked.txt
sed 's/feature "gpu"/feature "eval-hooks"/' "$CLEAN" >"$HOOKED"

# A toy checkout as the real one is: strand-render's feature line and
# its cfg'd code.
fresh() {
  rm -rf "$T"
  mkdir -p "$T/crates/strand-render/src" "$T/crates/strand/src" "$T/eval"
  cat >"$T/Cargo.toml" <<'TOML'
[workspace]
members = ["crates/*", "crates/strand-dev/tree-sitter-strand"]
TOML
  cat >"$T/crates/strand-render/Cargo.toml" <<'TOML'
[package]
name = "strand-render"

[features]
default = ["gpu"]
# eval-hooks: the sandbox's hooks (a comment naming it is fine)
eval-hooks = []
TOML
  cat >"$T/crates/strand/Cargo.toml" <<'TOML'
[package]
name = "strand"

[dependencies]
strand-render = { path = "../strand-render" }
TOML
  printf '#[cfg(feature = "eval-hooks")]\npub mod probe;\n' >"$T/crates/strand-render/src/lib.rs"
  printf 'fn main() {}\n' >"$T/crates/strand/src/main.rs"
}
run() { EVAL_ISOLATION_TREE=${TREE:-$CLEAN} "$CHECK" "$T"; }

fresh
expect 0 "the real layout passes" run
grep -q '1 cfg(feature = "eval-hooks") sites' "$DIR/out" && ok "it counts the cfg sites" ||
  no "it counts the cfg sites: $(cat "$DIR/out")"

fresh
sed -i 's|^strand-render = .*|&\nstrand-eval = { path = "../../eval" }|' "$T/crates/strand/Cargo.toml"
expect 1 "a crate depending on strand-eval fails" run

fresh
printf '[dev-dependencies]\nsandbox = { path = "../../eval/sandbox" }\n' >>"$T/crates/strand/Cargo.toml"
expect 1 "a path into eval/ fails" run

fresh
sed -i 's|^strand-render = .*|strand-render = { path = "../strand-render", features = ["eval-hooks"] }|' "$T/crates/strand/Cargo.toml"
expect 1 "a crate turning eval-hooks on fails" run

fresh
sed -i 's|^default = .*|default = ["gpu", "eval-hooks"]|' "$T/crates/strand-render/Cargo.toml"
expect 1 "strand-render enabling eval-hooks by default fails" run

fresh
sed -i 's|^members = .*|members = ["crates/*", "eval"]|' "$T/Cargo.toml"
expect 1 "eval/ as a root member fails" run

fresh
expect 1 "a resolution enabling eval-hooks fails" env TREE="$HOOKED" bash -c 'EVAL_ISOLATION_TREE=$TREE "$0" "$1"' "$CHECK" "$T"

fresh
printf 'strand-render v0.0.0\n' >"$DIR/bare.txt"
expect 0 "a resolution without the feature passes" env TREE="$DIR/bare.txt" bash -c 'EVAL_ISOLATION_TREE=$TREE "$0" "$1"' "$CHECK" "$T"
: >"$DIR/empty.txt"
expect 1 "an empty cargo tree output fails" env TREE="$DIR/empty.txt" bash -c 'EVAL_ISOLATION_TREE=$TREE "$0" "$1"' "$CHECK" "$T"

fresh
printf '#[cfg(feature = "eval-hooks")]\nfn hook() {}\n' >"$T/crates/strand/src/main.rs"
expect 1 "eval-hooks code outside strand-render fails" run

[ "$bad" = 0 ] && echo "test-check-isolation: all passed"
exit "$bad"
