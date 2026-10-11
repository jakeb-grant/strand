#!/usr/bin/env bash
# M5 wave-0 spike 3 (decisions.md m5-infra): builds the client and runs
# every scenario in run.sh's image (sway 1.9, headless, pixman).
# Usage: spikes/xdg-activation/run.sh
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$ROOT/target/spike-xdg-activation
mkdir -p "$OUT"
"$ROOT/scripts/container/run.sh" bash -c "cd '$HERE' && CARGO_TARGET_DIR='$OUT/target' cargo build -q --release && bash '$HERE/inside.sh' '$OUT/target/release/xdg-activation-spike' '$OUT'"
