#!/usr/bin/env bash
# nextest-summary.sh JUNIT: prints what a nextest run's junit file says, and
# adds it to $GITHUB_STEP_SUMMARY when that is set (nextest-summary.py). Exits
# 1 when the file is missing or holds no test, so a run that never happened
# can't pass as an empty summary. A failure's status is the run's own, which
# the caller keeps. Self-test: scripts/container/test-nextest-summary.sh

set -uo pipefail

[ "$#" = 1 ] || { echo "usage: $0 JUNIT" >&2; exit 2; }
if [ ! -s "$1" ]; then
  echo "nextest-summary.sh: no junit file at $1: the run ran no test" >&2
  exit 1
fi
exec python3 -I "$(dirname "$0")/nextest-summary.py" "$1"
