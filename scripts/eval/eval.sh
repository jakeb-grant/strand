#!/usr/bin/env bash
# strand-eval, the animation evaluation sandbox (docs/eval.md), built and
# run from the repository root: eval/ is a Cargo workspace of its own, so
# it is reached with --manifest-path, which keeps the target directory
# (run.sh's CARGO_TARGET_DIR=target/container, else target/) and the
# shared compile cache the root workspace's (docs/decisions.md
# eval-sandbox).
#
# Usage: scripts/eval/eval.sh <command> [args...]   (strand-eval's commands)
#        scripts/eval/eval.sh selftest [step...]    scripts/eval/ci.sh
#        scripts/eval/eval.sh mutants [name...]     scripts/eval/mutants.sh
#        scripts/eval/eval.sh ab-revs <revA> [<revB>] [-- scenarios]
#                                                    scripts/eval/ab.sh
#        scripts/eval/eval.sh matrix-all [flags...]  scripts/eval/matrix.sh
# In the container: scripts/container/run.sh eval <command> [args...]

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}

if [ "${1:-}" = selftest ]; then
  shift
  exec bash "$ROOT/scripts/eval/ci.sh" "$@"
fi
if [ "${1:-}" = mutants ]; then
  shift
  exec bash "$ROOT/scripts/eval/mutants.sh" "$@"
fi
if [ "${1:-}" = ab-revs ]; then
  shift
  exec bash "$ROOT/scripts/eval/ab.sh" "$@"
fi
if [ "${1:-}" = matrix-all ]; then
  shift
  exec bash "$ROOT/scripts/eval/matrix.sh" "$@"
fi
exec cargo run -q --locked --manifest-path eval/Cargo.toml --bin strand-eval -- "$@"
