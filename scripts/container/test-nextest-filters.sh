#!/usr/bin/env bash
# Tests nextest-filters.sh --static: the real config passes, a retry on a
# budget, on a logic crate, on a test holding a gate or a wake count, on a binary
# or a name that isn't there, in a form it can't read, more than once or outside
# `ci` fails, a stress profile with retries of its own fails, and a nightly job
# naming nextest fails.
# Run: scripts/container/run.sh scripts/container/test-nextest-filters.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
bad=0

# expect WANT NAME: --static on $tmp/config.toml and $tmp/ci.yml exits WANT.
expect() {
  local want=$1 name=$2 got=0
  NEXTEST_FILTERS_CONFIG=$tmp/config.toml NEXTEST_FILTERS_WORKFLOW=$tmp/ci.yml \
    "$HERE/nextest-filters.sh" --static >/dev/null 2>&1 || got=$?
  if [ "$got" = "$want" ]; then echo "ok   $name"; else echo "FAIL $name: exit $got, wanted $want"; bad=1; fi
}
config() { printf '%s\n' "$1" >"$tmp/config.toml"; }

printf 'jobs:\n  test:\n    steps:\n      - run: cargo nextest run\n  nightly:\n    steps:\n      - run: cargo test\n  other:\n    steps: []\n' >"$tmp/ci.yml"

cp "$ROOT/.config/nextest.toml" "$tmp/config.toml"
expect 0 "the real config, with a nightly job on cargo test"
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand-surface::(compose)\$/)'
retries = 1"
expect 0 "a Wayland binary may have a retry"
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand-services::(media)\$/)'
retries = 1"
expect 1 "not one whose test holds a gate"
config "[[profile.ci.overrides]]
filter = '(binary_id(/^strand-services::(media)\$/)) & not (test(=a_frozen_player_does_not_hold_up_the_others))'
retries = 1"
expect 0 "unless the retry leaves that test out"
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand-services::custom\$/)'
retries = 1"
expect 1 "nor one whose test holds a wake count"
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand-surface::(sway)\$/)'
retries = 1"
expect 1 "nor one whose test holds an idle count"
config "[[profile.ci.overrides]]
filter = '(binary_id(/^strand-surface::(compose)\$/)) & not (test(=no_such_test))'
retries = 1"
expect 1 "a name left out that is in none of its binaries fails"
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand-surface::(compose|no_such_binary)\$/)'
retries = 1"
expect 1 "a binary among others that isn't there fails"
config "[[profile.ci.overrides]]
filter = 'binary_id(=strand::budgets)'
retries = 1"
expect 1 "a budget named with = fails, as a form it can't read"
config "[[profile.ci.overrides]]
filter = 'test(budget)'
retries = 1"
expect 1 "a test() of its own fails"
config "[[profile.ci.overrides]]
filter = 'binary_id(/budgets/)'
retries = 1"
expect 1 "an unanchored regex fails"
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand-surface::(compose)\$/) | all()'
retries = 1"
expect 1 "and so does anything beside the binaries"
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand::(clipboard|budgets)\$/)'
retries = 1"
expect 1 "a budget among them may not"
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand-render::(motion)\$/)'
retries = 1"
expect 1 "nor a render binary"
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand-render::theme_swap_bench\$/)'
retries = 1"
expect 1 "nor a timing bench named alone"
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand-surface::(compose)\$/)'
retries = 2"
expect 1 "nor two retries"
config "[[profile.default.overrides]]
filter = 'binary_id(/^strand-surface::(compose)\$/)'
retries = 1"
expect 1 "nor one outside the ci profile"
config "[profile.ci]
retries = 0
[[profile.ci.overrides]]
filter = 'binary_id(/^strand-surface::(compose)\$/)'
retries = 1
[profile.stress]
retries = 1"
expect 1 "nor may the stress profile have one of its own"
cp "$ROOT/.config/nextest.toml" "$tmp/config.toml"
printf 'jobs:\n  nightly:\n    steps:\n      - run: cargo nextest run\n' >"$tmp/ci.yml"
expect 1 "a nightly job naming nextest fails"
exit "$bad"
