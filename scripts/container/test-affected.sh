#!/usr/bin/env bash
# Tests affected.sh's choice of crates on stand-in diffs (AFFECTED_FILES):
# a crate's file picks that crate, --deep adds its dependents, docs pick
# the crates whose tests read them, the root manifest picks every crate,
# and scripts alone pick none.
# Run: scripts/container/run.sh scripts/container/test-affected.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
bad=0

# expect NAME FILES WANT [ARGS...]: affected.sh --list ARGS on FILES names
# exactly the crates in WANT (space-separated, sorted; empty for none).
expect() {
  local name=$1 files=$2 want=$3 got
  shift 3
  got=$(AFFECTED_FILES=$files "$HERE/affected.sh" --list "$@" 2>&1 | sed -n 's/^affected since [^:]*: //p')
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: got '$got', wanted '$want'"
    bad=1
  fi
}

expect "a crate's file picks that crate" \
  "crates/strand-watch/src/lib.rs" "strand-watch"
expect "two crates, a test file among them" \
  $'crates/strand-text/src/lib.rs\ncrates/strand-services/tests/network.rs' "strand-services strand-text"
expect "docs pick the crates whose tests read them" \
  "docs/spec.md" "strand strand-compiler strand-dev"
expect "scripts alone pick nothing" "scripts/container/ci.sh" ""
all=$(cargo metadata --format-version 1 --no-deps --offline 2>/dev/null |
  jq -r '.packages[].name' | sort | tr '\n' ' ' | sed 's/ $//')
expect "the root manifest picks every crate" "Cargo.lock" "$all"
deep=$(AFFECTED_FILES=crates/strand-core/src/lib.rs "$HERE/affected.sh" --list --deep 2>&1 | sed -n 's/^affected since [^:]*: //p')
case " $deep " in
  *" strand-core "*" strand "* | *" strand "*" strand-core "*) echo "ok   --deep reaches strand from strand-core" ;;
  *) echo "FAIL --deep from strand-core: got '$deep'"; bad=1 ;;
esac
[ "$(AFFECTED_FILES=crates/strand-core/src/lib.rs "$HERE/affected.sh" --list 2>&1 | sed -n 's/^affected since [^:]*: //p')" = strand-core ] &&
  echo "ok   without --deep only the crate itself" ||
  { echo "FAIL without --deep"; bad=1; }

exit "$bad"
