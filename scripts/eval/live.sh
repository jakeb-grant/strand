#!/usr/bin/env bash
# The live sandbox (docs/eval.md, "Live sandbox"; docs/decisions.md
# eval-live): builds strand-eval in the container (the timing profile, so
# the CPU leg keeps up) and runs `strand-eval live` here, a window on the
# session this shell runs in. Only the window runs on the host: the
# build, and every test of it, stay in the container.
#
# Usage: scripts/eval/live.sh <shell.strand|scenario.scn> [flags...]
#        flags as `strand-eval live --help` lists them: --screen WxH[@S],
#        --hz N, --fonts bundled|local, --speed F, --paused,
#        --record-to FILE, --ring N, --max-frames N.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
if [ "$#" -eq 0 ] || [ "$1" = -h ] || [ "$1" = --help ]; then
  sed -n '2,11p' "$0"
  exit 0
fi
if [ -z "${WAYLAND_DISPLAY:-}" ]; then
  echo "live.sh: WAYLAND_DISPLAY is not set: run it from a terminal in your desktop session" >&2
  exit 2
fi
if [ -f /.dockerenv ]; then
  echo "live.sh: runs on the host (it opens a window there), not in the container" >&2
  exit 2
fi
(cd "$ROOT" && scripts/container/run.sh cargo build --locked --manifest-path eval/Cargo.toml \
  --profile timing --bin strand-eval)
# The arguments are left as given: paths are the caller's, from here.
exec "$ROOT/target/container/timing/strand-eval" live "$@"
