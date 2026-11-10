#!/usr/bin/env bash
# Every mutant patch still applies (docs/eval.md, "Mutants"). The mutants
# themselves run nightly (eval-matrix.yml, scripts/eval/mutants.sh); this
# is their cheap part, run on every push in ci.sh's step 3, so the commit
# that moves a patched line fails here by the patch's name and
# regenerates it, instead of leaving the nightly STALE.
#
# Checked: every eval/mutants/*.patch passes `git apply --check` against
# the checkout's working tree (clean in CI; locally, uncommitted changes
# count). That mutants.txt names patches that exist is strand-eval's
# `every_mutant_line_names_what_exists`.
#
# Usage: scripts/eval/check-mutants.sh [ROOT]   (default: this checkout)
# Self-test: scripts/eval/test-check-mutants.sh

set -uo pipefail

ROOT=${1:-$(cd "$(dirname "$0")/../.." && pwd)}
cd "$ROOT" || exit 2
DIR=eval/mutants
[ -d "$DIR" ] || { echo "check-mutants: no $DIR" >&2; exit 2; }
bad=0
n=0

for p in "$DIR"/*.patch; do
  [ -e "$p" ] || continue
  n=$((n + 1))
  if ! out=$(git apply --check "$p" 2>&1); then
    echo "check-mutants: STALE $p no longer applies: regenerate it (docs/eval.md, Mutants)"
    printf '%s\n' "$out" | head -n 4 | sed 's/^/  /'
    bad=1
  fi
done

[ "$bad" = 0 ] && echo "check-mutants: $n patches apply"
exit "$bad"
