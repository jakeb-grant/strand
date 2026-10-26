#!/usr/bin/env bash
# The render path checked for wasm32-unknown-unknown (docs/wasm-spike.md).
# Run inside scripts/wasm/run.sh.
set -euo pipefail
exec cargo check --target wasm32-unknown-unknown --no-default-features \
  -p strand-scene -p strand-core -p strand-text -p strand-theme -p strand-icons \
  -p strand-compiler -p strand-render -p strand-services-schema "$@"
