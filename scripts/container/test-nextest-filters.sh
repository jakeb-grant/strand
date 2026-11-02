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
filter = '(binary_id(/^strand-services::(media)\$/)) & not (test(=a_frozen_player_does_not_hold_up_the_others) | test(=media_follows_the_active_player_without_polling) | test(=players_list_every_player_and_select_picks_the_one_media_follows))'
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
# A tree of its own for what a binary includes: a helper holding a gate, by each
# way a test file includes one, and one that isn't there.
fake=$tmp/crates/strand-surface/tests
mkdir -p "$fake/common" "$fake/pipewire/deep"
helper() {
  printf 'pub fn helped() {\n    let _ = 1;\n}\n' >"$fake/common/mod.rs"
  printf 'pub mod deep;\n' >"$fake/pipewire/mod.rs"
  printf 'pub fn helped() {\n    let _ = 1;\n}\n' >"$fake/pipewire/deep.rs"
}
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand-surface::(fake)\$/)'
retries = 1"
expect_fake() {
  local want=$1 name=$2 got=0
  NEXTEST_FILTERS_CRATES=$tmp/crates NEXTEST_FILTERS_CONFIG=$tmp/config.toml NEXTEST_FILTERS_WORKFLOW=$tmp/ci.yml \
    "$HERE/nextest-filters.sh" --static >/dev/null 2>&1 || got=$?
  if [ "$got" = "$want" ]; then echo "ok   $name"; else echo "FAIL $name: exit $got, wanted $want"; bad=1; fi
}
helper
printf 'mod common;\n#[path = "pipewire/mod.rs"]\nmod pipewire;\n#[test]\nfn t() {}\n' >"$fake/fake.rs"
expect_fake 0 "a binary whose helpers hold nothing may have a retry"
printf 'pub fn helped() {\n    let g = Gates::default();\n}\n' >"$fake/common/mod.rs"
expect_fake 1 "not when its \`mod\` helper holds a gate"
helper
printf 'pub fn helped() {\n    let g = Gates::default();\n}\n' >"$fake/pipewire/deep.rs"
expect_fake 1 "nor a \`mod\` inside a \`#[path]\` helper"
helper
printf 'mod gone;\n#[test]\nfn t() {}\n' >"$fake/fake.rs"
expect_fake 1 "a \`mod\` it can't find fails"
# Work done unread or idle, as the third review found it left under the retry.
held_by() {
  printf '#[test]\n%s\n' "$1" >"$fake/fake.rs"
  expect_fake 1 "$2"
}
held_by 'fn scans_only_while_read() {}' "a test named for working only while read fails"
held_by 'fn t() {
    // Nobody reads it: no scan.
}' "a test whose comment says nobody reads fails"
held_by 'fn t() {
    // Idle: nothing changes, nothing is sent.
}' "one whose comment says it is idle fails"
held_by 'fn t() {
    assert!(!runs(), "it runs for nobody");
}' "one whose message says for nobody fails"
held_by 'fn t() {
    assert_eq!(c.quiet_for(Duration::from_millis(500)).await, 0);
}' "a quiet window asserted zero fails"
held_by 'fn t() {
    assert_eq!(
        c.quiet_for(Duration::from_millis(300)).await,
        0,
        "the same state"
    );
}' "and on lines of its own"
printf '#[test]\nfn t() {\n    c.quiet_for(Duration::from_millis(500)).await;\n    // Read again.\n}\n' >"$fake/fake.rs"
expect_fake 0 "a quiet window only waited through doesn't"
printf '/// Runs only while read, as a doc comment on the next.\n#[test]\nfn t() {}\n' >"$fake/fake.rs"
expect_fake 0 "nor a doc comment's words"
helper
printf 'mod common;\n#[test]\nfn t() {}\n' >"$fake/fake.rs"
config "[[profile.ci.overrides]]
filter = 'binary_id(/^strand-services::(network)\$/)'
retries = 1"
expect 1 "nor one whose test counts work done while nobody reads"
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
