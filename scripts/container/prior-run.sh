#!/usr/bin/env bash
# prior-run.sh: CI's `prior` job. On a push to main, which check jobs a
# green CI run of the same commit, pushed to another branch, already passed,
# so main's run can skip them (decisions.md audit-main-rerun). With `gh`
# (GH_TOKEN with actions: read and contents: read) it looks for a completed
# push run of ci.yml on that SHA whose conclusion is success, newest first,
# and holds each job of SKIPPABLE to it: proven only when that job's
# conclusion there is success and, for a job a path filter can empty, the
# step that does its work succeeded too. It also wants the cache's inputs
# unchanged since the push's previous head, so main's caches, which every
# branch reads, stay those of main's own commits.
#
# Writes `green` and `proven_<job>` (`-` as `_`) to GITHUB_OUTPUT, each
# `true` only on a push to main where all of that holds; any error, a
# timeout or an answer it cannot read gives `false`. On every other push
# it prints what it found and writes `false`, a dry run of the query.
# Always exits 0: it is never what fails a run.
#
# Env: GITHUB_REPOSITORY, GITHUB_SHA, GITHUB_REF, GITHUB_EVENT_NAME,
# BEFORE (github.event.before), GITHUB_OUTPUT.

set -uo pipefail

SKIPPABLE="lint test budgets acceptance timing eval site compositors lock-vm"
# job<TAB>step: the step a job's path filter can skip, which must have run.
WORK_STEP=$'lock-vm\tLock VM scenarios'
# Files rust-cache's keys hash (the lockfile and manifests, the toolchain,
# the image, the CARGO_* env) and the setup that passes them.
CACHE_INPUTS='^(Cargo\.lock|(.*/)?Cargo\.toml|\.cargo/config(\.toml)?|rust-toolchain(\.toml)?|scripts/container/Dockerfile|\.github/workflows/ci\.yml|\.github/actions/setup/.*)$'

out=${GITHUB_OUTPUT:-/dev/null}
repo=${GITHUB_REPOSITORY:-}
sha=${GITHUB_SHA:-}
ref=${GITHUB_REF:-}
event=${GITHUB_EVENT_NAME:-}
before=${BEFORE:-}

api() { timeout 60 gh api "$@"; }

proven=""
reason=""
decide() {
  if [ "$event" != push ]; then reason="not a push ($event)"; return; fi
  if ! [[ $sha =~ ^[0-9a-f]{40}$ ]]; then reason="no commit SHA"; return; fi
  local runs run jobs job name step
  runs=$(api "repos/$repo/actions/workflows/ci.yml/runs?head_sha=$sha&event=push&status=completed&per_page=100") ||
    { reason="the runs query failed"; return; }
  run=$(jq -r --arg sha "$sha" '
    [ .workflow_runs[]
      | select(.head_sha == $sha and .event == "push" and .status == "completed"
               and .conclusion == "success" and .head_branch != "main") ]
    | sort_by(.updated_at) | last | .id // empty' <<<"$runs") || { reason="unreadable runs"; return; }
  if ! [[ $run =~ ^[0-9]+$ ]]; then reason="no green push run of ${sha::12} on another branch"; return; fi
  echo "prior: run $run passed ${sha::12}"
  jobs=$(api "repos/$repo/actions/runs/$run/jobs?filter=latest&per_page=100") || { reason="the jobs query failed"; return; }
  for job in $SKIPPABLE; do
    name=$(jq -r --arg j "$job" '[.jobs[] | select(.name == $j)] | if length == 1 then .[0].conclusion else "none" end' <<<"$jobs") ||
      { reason="unreadable jobs"; proven=""; return; }
    step=$(awk -F'\t' -v j="$job" '$1 == j { print $2 }' <<<"$WORK_STEP")
    if [ "$name" = success ] && [ -n "$step" ]; then
      name=$(jq -r --arg j "$job" --arg s "$step" '
        [.jobs[] | select(.name == $j) | .steps[]? | select(.name == $s)]
        | if length == 1 then .[0].conclusion else "none" end' <<<"$jobs") || { reason="unreadable steps"; proven=""; return; }
      [ "$name" = success ] || name="filtered ($step: $name)"
    fi
    if [ "$name" = success ]; then proven="$proven $job"; else echo "prior: $job not proven there: $name"; fi
  done
  if ! [[ $before =~ ^[0-9a-f]{40}$ ]] || [ "$before" = 0000000000000000000000000000000000000000 ]; then
    reason="no previous head to compare the cache inputs with"; proven=""; return
  fi
  local cmp files
  cmp=$(api "repos/$repo/compare/$before...$sha") || { reason="the compare query failed"; proven=""; return; }
  files=$(jq -r 'if (.status == "ahead" or .status == "identical") and ((.files // []) | length) < 300
                 then (.files // [])[].filename else "!" end' <<<"$cmp") || { reason="unreadable compare"; proven=""; return; }
  if [ "$files" = "!" ]; then reason="not a fast-forward of under 300 files"; proven=""; return; fi
  if grep -Eq "$CACHE_INPUTS" <<<"$files"; then
    reason="a cache input changed: $(grep -E "$CACHE_INPUTS" <<<"$files" | head -3 | tr '\n' ' ')"; proven=""; return
  fi
  [ -n "$proven" ] || reason="no job proven there"
}
decide

main=false
[ "$event" = push ] && [ "$ref" = refs/heads/main ] && main=true
green=false
[ -n "$proven" ] && [ -z "$reason" ] && green=true
if [ "$green" = true ]; then
  echo "prior: proven:$proven"
else
  echo "prior: none proven: $reason"
fi
if [ "$main" != true ]; then
  [ "$green" = true ] && echo "prior: dry run, not main: every job runs"
  green=false
fi
{
  echo "green=$green"
  for job in $SKIPPABLE; do
    v=false
    if [ "$green" = true ] && [[ " $proven " == *" $job "* ]]; then v=true; fi
    echo "proven_${job//-/_}=$v"
  done
} >>"$out"
exit 0
