#!/usr/bin/env bash
# The M5 size ledger line:
# crates/strand/tests/budgets.rs's `.text` gate test adds each release
# build's half to STRAND_LEDGER_FILE ("cpu ..." or "gpu ..."); this
# prints them as one line, and adds it to the GitHub job summary when
# there is one. The latest half of each build wins; a build that did
# not run reads "not measured".
#
# Usage: scripts/container/ledger.sh FILE
#   .text CPU-only 15,657,671 (Δ +0 vs 15,657,671; gate 15,925,248) · GPU ...

set -uo pipefail

file=${1:?usage: ledger.sh FILE}
cpu= gpu=
if [ -f "$file" ]; then
  cpu=$(sed -n 's/^cpu //p' "$file" | tail -1)
  gpu=$(sed -n 's/^gpu //p' "$file" | tail -1)
fi
line=".text ${cpu:-CPU-only not measured} · ${gpu:-GPU not measured}"
echo "$line"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  printf '### Size ledger\n\n%s\n' "$line" >>"$GITHUB_STEP_SUMMARY"
fi
