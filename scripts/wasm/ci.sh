#!/usr/bin/env bash
# ci.sh: CI's `wasm` job (.github/workflows/ci.yml; docs/live-examples.md)
# on the laptop, run on the host: the lint, the two builds and their size
# budget in scripts/wasm/run.sh's image, the site built in the container
# suite's (scripts/container/run.sh), the build added to it, and the
# browser check on the host's Docker. scripts/container/ci.sh cannot run
# these steps: its image has no wasm target, wasm-bindgen or wasm-opt,
# and the browser check starts its own container, as shots.sh does.
#   CI_FROM=N starts at step N.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
FROM=${CI_FROM:-1}
n=0
step() {
  local name=$1
  shift
  n=$((n + 1))
  if [ "$n" -lt "$FROM" ]; then
    echo "=== [$n] wasm: $name (skipped: CI_FROM=$FROM)"
    return 0
  fi
  echo "=== [$n] wasm: $name"
  local start=$SECONDS status=0
  "$@" || status=$?
  echo "=== [$n] wasm: $name: exit $status after $((SECONDS - start)) s"
  [ "$status" = 0 ] || { echo "WASM CI FAILED at step $n: $name"; exit "$status"; }
}
step "lint, build and size the live examples" scripts/wasm/run.sh bash scripts/wasm/build.sh
step "build the site" scripts/container/run.sh cargo run -q -p strand-site -- build
step "add the live examples to it" bash scripts/wasm/site.sh
step "the live examples in Chromium, held to eval's frames" bash scripts/wasm/check-browser.sh
echo "WASM CI PASSED: all $n steps"
