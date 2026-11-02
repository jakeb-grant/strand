#!/usr/bin/env bash
# Tests nextest-filters.sh --static: the real config passes, a retry on a
# budget, on a logic crate, more than once or outside `ci` fails, a stress
# profile with retries of its own fails, and a nightly job naming nextest fails.
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
filter = 'binary_id(/^strand-surface::(sway)\$/)'
retries = 1"
expect 0 "a Wayland binary may have a retry"
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
filter = 'binary_id(/^strand-surface::(sway)\$/)'
retries = 2"
expect 1 "nor two retries"
config "[[profile.default.overrides]]
filter = 'binary_id(/^strand-surface::(sway)\$/)'
retries = 1"
expect 1 "nor one outside the ci profile"
config "[profile.ci]
retries = 0
[[profile.ci.overrides]]
filter = 'binary_id(/^strand-surface::(sway)\$/)'
retries = 1
[profile.stress]
retries = 1"
expect 1 "nor may the stress profile have one of its own"
cp "$ROOT/.config/nextest.toml" "$tmp/config.toml"
printf 'jobs:\n  nightly:\n    steps:\n      - run: cargo nextest run\n' >"$tmp/ci.yml"
expect 1 "a nightly job naming nextest fails"
exit "$bad"
