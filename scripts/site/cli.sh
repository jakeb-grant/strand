#!/usr/bin/env bash
# cli.sh: captures the strand binary's help for the site's CLI page
# (tools/cli.html): builds strand, then `strand-site cli` writes
# crates/strand-site/cli-help.txt from `strand --help` and each command's
# own --help (crates/strand-site/src/cli.rs). Run it after a command or
# flag changes; `strand-site build --strand BIN` (CI's site step) fails
# until the capture is what the binary prints. Runs through
# scripts/container/run.sh; SITE_DIRECT=1 runs it as it is.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
body='cargo build -q -p strand --bin strand &&
  cargo run -q -p strand-site -- cli "${CARGO_TARGET_DIR:-target}/debug/strand"'
if [ "${SITE_DIRECT:-0}" = 1 ]; then
  bash -c "$body"
else
  "$ROOT/scripts/container/run.sh" bash -c "$body"
fi
