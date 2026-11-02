#!/usr/bin/env bash
# The debug workspace tests of CI's `test` job and ci.sh, in one place so the
# two can't drift: the same selection `cargo test --workspace -- --skip ... `
# made, run by cargo-nextest under its `ci` profile (.config/nextest.toml;
# decisions.md robustness), each test in a process of its own. libtest's --skip
# and nextest's test() both match a substring of the name. Then the job's
# summary, whatever the run's result, whose status it keeps.
# Run: scripts/container/run.sh scripts/container/workspace-tests.sh [NEXTEST ARGS...]
# `--list` instead prints the selection as nextest's JSON and runs nothing.

set -uo pipefail
cd "$(dirname "$0")/../.."

# The reload fuzzer runs on its own (timing-sensitive saves), and the 100
# reloads too: steps of their own after this one.
FILTER='not (test(random_edits_through_five_save_styles) or test(a_hundred_reloads_reconnect_and_restart_nothing))'

if [ "${1:-}" = --list ]; then
  shift
  exec cargo nextest list --workspace --profile ci -E "$FILTER" --message-format json --cargo-quiet "$@"
fi

status=0
cargo nextest run --workspace --profile ci -E "$FILTER" "$@" || status=$?
summary=0
scripts/container/nextest-summary.sh target/nextest/ci/junit.xml || summary=$?
[ "$status" != 0 ] || status=$summary
exit "$status"
