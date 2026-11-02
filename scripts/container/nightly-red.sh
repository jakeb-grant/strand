#!/usr/bin/env bash
# A nightly job that failed, where the owner sees it: one open issue labelled
# `nightly-red`, a new comment on it for each red night, closed by hand once
# triaged. GitHub's own mail for a scheduled run goes only to whoever last edited
# the cron line, and only if their settings ask for it; an issue reaches every
# watcher, and `gh issue list --label nightly-red` shows it from the laptop
# (docs/development.md). Runs on the runner, in ci.yml's `nightly-red` job, the
# only job whose token may write issues.
#
# Usage: nightly-red.sh "JOB: RESULT"...
# Env:   GH_TOKEN, GH_REPO, RUN_URL; SUMMARIES, a directory of the jobs' summary
#        files to quote, if there is one.
# Self-test: scripts/container/test-nightly-red.sh

set -euo pipefail

[ "$#" -gt 0 ] || { echo "usage: $0 'JOB: RESULT'..." >&2; exit 2; }

body=$(mktemp)
trap 'rm -f "$body"' EXIT
{
  echo "A nightly run went red: ${RUN_URL:?}"
  echo
  for line in "$@"; do
    case "$line" in *": failure" | *": cancelled") echo "- \`$line\`" ;; esac
  done
  if [ -n "${SUMMARIES:-}" ] && [ -d "$SUMMARIES" ]; then
    for file in "$SUMMARIES"/*.md; do
      [ -f "$file" ] || continue
      echo
      # An issue's comment holds 65,536 characters.
      head -c 20000 "$file"
    done
  fi
} >"$body"

gh label create nightly-red --color B60205 \
  --description "A nightly CI job failed: scripts/container/nightly-red.sh" --force
open=$(gh issue list --label nightly-red --state open --limit 1 --json number --jq '.[0].number // empty')
if [ -n "$open" ]; then
  gh issue comment "$open" --body-file "$body"
else
  gh issue create --title "Nightly CI is red" --label nightly-red --body-file "$body"
fi
