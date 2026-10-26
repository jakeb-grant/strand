#!/usr/bin/env bash
# lint.sh: clippy for wasm32-unknown-unknown on every workspace crate
# the live examples build (strand-web's tree, with the features it
# builds them with), with scripts/wasm/clippy.toml: a call to std's
# clock, which panics there at its first call and which no native test
# sees, fails (docs/live-examples.md, "The clock"). scripts/wasm/build.sh
# runs it first; run inside scripts/wasm/run.sh.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target/wasm}
# The members strand-web builds for the browser: `cargo tree -p
# strand-web --target wasm32-unknown-unknown -e normal` lists them, and
# a new one fails here until it is named.
crates=(strand-web strand-compiler strand-core strand-icons strand-render strand-scene
  strand-services-schema strand-text strand-theme)
tree=$(cargo tree -q -p strand-web --target wasm32-unknown-unknown -e normal --prefix none \
  | sed -n 's/^\(strand[a-z-]*\) v.*/\1/p' | sort -u)
for c in $tree; do
  [[ " ${crates[*]} " == *" $c "* ]] || { echo "lint.sh: strand-web builds $c for the browser: name it in scripts/wasm/lint.sh" >&2; exit 1; }
done
args=()
for c in "${crates[@]}"; do args+=(-p "$c"); done
CLIPPY_CONF_DIR=$ROOT/scripts/wasm cargo clippy -q --target wasm32-unknown-unknown --no-default-features \
  "${args[@]}" --lib -- -D clippy::disallowed_methods
