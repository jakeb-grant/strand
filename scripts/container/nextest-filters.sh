#!/usr/bin/env bash
# Holds .config/nextest.toml's filters to the tests there are, which nextest
# itself doesn't: a filter matching nothing is silently no filter, so a renamed
# binary would lose its group or its retries with nothing said. Fails when
#   - any override's filter matches no test;
#   - a test with a retry is in a binary or a test that may never have one: a
#     budget, a timing gate, a pixel reference (decisions.md robustness);
#   - the job that stresses nightly names nextest.
# Runs after the build in CI's `test` job and ci.sh. Needs cargo-nextest, so the
# test binaries; static checks only with --static.
# Self-test: scripts/container/test-nextest-filters.sh

set -uo pipefail
cd "$(dirname "$0")/../.."

CONFIG=${NEXTEST_FILTERS_CONFIG:-.config/nextest.toml}
WORKFLOW=${NEXTEST_FILTERS_WORKFLOW:-.github/workflows/ci.yml}
bad=0
no() { echo "nextest-filters: $*"; bad=1; }

# The overrides' filters, one per line, as `profile<TAB>retries<TAB>filter`.
overrides=$(python3 -I - "$CONFIG" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    config = tomllib.load(f)
for profile, body in config["profile"].items():
    for o in body.get("overrides", []):
        print(f"{profile}\t{o.get('retries', 0)}\t{o['filter']}")
PY
) || { echo "nextest-filters: can't read $CONFIG"; exit 1; }
[ -n "$overrides" ] || no "$CONFIG has no overrides"

# Binaries and names a retry may never reach. `strand-render` and the
# compiler's are logic and pixels; `demo`, `budgets`, `acceptance` and
# `services` hold memory or idle gates; `m5` holds the newcomer's references and
# its gate; `*_idle` and `idle` are wake counts; `reloads` is its own gate.
FORBIDDEN='^strand-(render|compiler|scene|core|text|theme|gpu|site|web|dev|fuzzy|icons)(::|$)|::(budgets|demo|acceptance|services|m5|reloads|gpu_idle|presented_quiet|dbus_idle|audio_idle|idle|.*bench.*)$|^strand$|^strand-services$|^strand-surface$'

while IFS=$'\t' read -r profile retries filter; do
  [ -n "$profile" ] || continue
  if [ "$retries" != 0 ]; then
    [ "$profile" = ci ] || no "profile $profile has retries; only ci may"
    [ "$retries" = 1 ] || no "retries = $retries; at most one"
    while read -r id; do
      [ -n "$id" ] || continue
      grep -Eq "$FORBIDDEN" <<<"$id" && no "the retry filter names $id, which may not be retried"
    done < <(grep -oE '\^[^$]+\$' <<<"$filter" | while read -r re; do
      # ^strand-services::(a|b)$ -> each binary id.
      prefix=${re%%\(*}
      case "$re" in
        *\(*) body=${re#*\(}; body=${body%\)\$}; tr '|' '\n' <<<"$body" | sed "s/^/${prefix#^}/" ;;
        *) re=${re#^}; echo "${re%\$}" ;;
      esac
    done)
  fi
  [ "${1:-}" = --static ] && continue
  count=$(cargo nextest list --workspace --profile "$profile" -E "$filter" --message-format json --cargo-quiet 2>/dev/null |
    jq -r '[."rust-suites"[].testcases[] | select(."filter-match".status == "matches")] | length')
  if [ "${count:-0}" = 0 ]; then
    no "profile $profile: no test matches: $filter"
  else
    echo "ok   $profile: $count tests match: ${filter:0:70}"
  fi
done <<<"$overrides"

# Nightly's stress job, whose failures are what it's for, has no retries: it
# doesn't use nextest.
if awk '/^  nightly:$/ { on = 1; next } /^  [a-z_-]+:$/ { on = 0 } on' "$WORKFLOW" | grep -q nextest; then
  no "the nightly job names nextest: it gets no retries"
fi

[ "$bad" = 0 ] && echo "ok   the nextest filters hold"
exit "$bad"
