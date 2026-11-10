#!/usr/bin/env bash
# checks-gate.sh: CI's `checks` job, the one deploy and release need. NEEDS
# is the job's `toJSON(needs)`. Passes only when every job of CHECKS
# succeeded, or was skipped while `prior` succeeded and its `proven_<job>`
# says a green run of the same commit on another branch passed it (prior-run.sh,
# decisions.md audit-main-rerun). Anything else, a missing job, a failure, a
# cancel or a skip nothing proved, fails it.

set -uo pipefail

CHECKS="lint test budgets acceptance timing eval site wasm compositors lock-vm package"
# The jobs prior-run.sh can prove (its SKIPPABLE): package and wasm build
# what release and deploy ship, so they always run.
PROVABLE="lint test budgets acceptance timing eval site compositors lock-vm"

needs=${NEEDS:-}
[ -n "$needs" ] || { echo "checks: no NEEDS" >&2; exit 1; }
# A skip counts only when prior itself succeeded.
p=$(jq -r '.prior.result // "missing"' <<<"$needs") || { echo "checks: unreadable NEEDS" >&2; exit 1; }
bad=0
for job in $CHECKS; do
  key=proven_${job//-/_}
  r=$(jq -r --arg j "$job" '.[$j].result // "missing"' <<<"$needs") || r=unreadable
  pr=$(jq -r --arg k "$key" '.prior.outputs[$k] // "false"' <<<"$needs") || pr=false
  [[ " $PROVABLE " == *" $job "* ]] || pr=false
  if [ "$r" = success ]; then
    echo "checks: $job passed"
  elif [ "$r" = skipped ] && [ "$pr" = true ] && [ "$p" = success ]; then
    echo "checks: $job passed on this commit's branch run"
  else
    echo "checks: $job: $r$([ "$r" = skipped ] && echo ", and no branch run proved it")" >&2
    bad=1
  fi
done
exit "$bad"
