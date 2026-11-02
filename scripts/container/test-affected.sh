#!/usr/bin/env bash
# Tests affected.sh's choice of crates on stand-in diffs (AFFECTED_FILES):
# a crate's file picks that crate, --deep adds its dependents, docs pick
# the crates whose tests read them, site/ picks strand-site, the root manifest picks every crate
# and the eval sandbox, eval/ and scripts/eval/ pick the sandbox alone, a
# strand-render change picks strand-render, checks its eval-hooks build
# and names the sandbox's command, a file the claims ledger quotes adds
# strand-site's build test, and other scripts alone pick none. A crate with no
# test passes and still reaches its doc-tests.
# Run: scripts/container/run.sh scripts/container/test-affected.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
bad=0

# A ledger of its own, so what the real one cites moves no other case.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cat >"$tmp/claims.toml" <<'LEDGER'
[[claim]]
page   = "site/start/running.md"
text   = "the words `scripts/in-text.sh`"
source = ['scripts/snippet.sh "the words it cites"', "scripts/heading.md#a-heading", 'crates/strand-watch/src/quoted.rs "more"']
proof  = "scripts/in-proof.sh"

[[claim]]
page   = "site/start/running.md"
text   = "more words"
source = ["scripts/bare.sh", '''scripts/triple.sh "it's quoted"''', 'crates/strand-dev/src/quoted.rs "still more"']
proof  = "read"
LEDGER
export AFFECTED_LEDGER=$tmp/claims.toml

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
  "docs/spec.md" "strand strand-compiler strand-dev strand-site"
expect "the site's pages pick the site's generator" "site/index.md" "strand-site"
expect "scripts alone pick nothing" "scripts/container/ci.sh" ""
expect "a script quoted with a snippet runs the site's build test" "scripts/snippet.sh" "strand-site:build"
expect "a file cited by its heading runs it" "scripts/heading.md" "strand-site:build"
expect "a bare path runs it" "scripts/bare.sh" "strand-site:build"
expect "a triple-quoted source runs it" "scripts/triple.sh" "strand-site:build"
expect "a path only in a claim's words or proof does not" \
  $'scripts/in-text.sh\nscripts/in-proof.sh' ""
expect "a quoted file of a crate picks the crate and the build test" \
  "crates/strand-watch/src/quoted.rs" "strand-site:build strand-watch"
expect "another file of that crate picks the crate alone" \
  "crates/strand-watch/src/other.rs" "strand-watch"
expect "the build test is not added to strand-site's own tests" \
  $'scripts/bare.sh\nsite/index.md' "strand-site"
case " $(AFFECTED_FILES=crates/strand-dev/src/quoted.rs "$HERE/affected.sh" --list --deep 2>&1 |
  sed -n 's/^affected since [^:]*: //p') " in
  *" strand-site:build "*) echo "FAIL --deep reaching strand-site still adds its build test"; bad=1 ;;
  *" strand-site "*) echo "ok   --deep reaching strand-site runs its tests, the build test among them" ;;
  *) echo "FAIL --deep from strand-dev does not reach strand-site"; bad=1 ;;
esac
hint=$(AFFECTED_FILES=scripts/bare.sh "$HERE/affected.sh" --list 2>&1)
case "$hint" in
  *"quotes scripts/bare.sh: strand-site's build test runs"*) echo "ok   --list says which file brought it in" ;;
  *) echo "FAIL --list does not name the quoted file: '$hint'"; bad=1 ;;
esac
# The real ledger, read the same way: a script it cites today.
real=$(sed -n "s/^source = \[.scripts\/\([^ \"']*\).*/scripts\/\1/p" "$HERE/../../site/claims.toml" | head -1)
case "$(env -u AFFECTED_LEDGER AFFECTED_FILES="$real" "$HERE/affected.sh" --list 2>&1 |
  sed -n 's/^affected since [^:]*: //p')" in
  strand-site:build) echo "ok   the real ledger's $real runs it" ;;
  *) echo "FAIL the real ledger's '$real' does not run it"; bad=1 ;;
esac
all=$(cargo metadata --format-version 1 --no-deps --offline 2>/dev/null |
  jq -r '.packages[].name' | sort | tr '\n' ' ' | sed 's/ $//')
expect "the root manifest picks every crate and the eval sandbox" "Cargo.lock" "eval $all"
expect "eval/ picks the eval sandbox alone" "eval/src/checks/teleport.rs" "eval"
expect "scripts/eval picks the eval sandbox alone" "scripts/eval/matrix.sh" "eval"
expect "an animation change picks strand-render, not the sandbox" \
  "crates/strand-render/src/anim/mod.rs" "strand-render"
hint=$(AFFECTED_FILES=crates/strand-render/src/anim/mod.rs "$HERE/affected.sh" --list 2>&1)
case "$hint" in
  *"eval-hooks build is checked"*"run.sh eval check eval/scenarios/curated eval/scenarios/regressions"*)
    echo "ok   an animation change names the sandbox's command" ;;
  *) echo "FAIL no sandbox hint for an animation change: '$hint'"; bad=1 ;;
esac
hint=$(AFFECTED_FILES=crates/strand-render/src/clock.rs "$HERE/affected.sh" --list 2>&1)
case "$hint" in
  *"eval-hooks build is checked"*) echo "ok   any strand-render change checks its eval-hooks build" ;;
  *) echo "FAIL no eval-hooks check for a strand-render change: '$hint'"; bad=1 ;;
esac
deep=$(AFFECTED_FILES=crates/strand-core/src/lib.rs "$HERE/affected.sh" --list --deep 2>&1 | sed -n 's/^affected since [^:]*: //p')
case " $deep " in
  *" strand-core "*" strand "* | *" strand "*" strand-core "*) echo "ok   --deep reaches strand from strand-core" ;;
  *) echo "FAIL --deep from strand-core: got '$deep'"; bad=1 ;;
esac
[ "$(AFFECTED_FILES=crates/strand-core/src/lib.rs "$HERE/affected.sh" --list 2>&1 | sed -n 's/^affected since [^:]*: //p')" = strand-core ] &&
  echo "ok   without --deep only the crate itself" ||
  { echo "FAIL without --deep"; bad=1; }

# The run itself, on a stand-in cargo whose nextest exits 4 as the real one does
# when nothing matches, unless told not to fail on that.
real_cargo=$(command -v cargo)
mkdir "$tmp/bin"
cat >"$tmp/bin/cargo" <<STUB
#!/usr/bin/env bash
case "\$1" in
  nextest) echo "cargo \$*" >>"$tmp/ran"; case " \$* " in *" --no-tests warn "*) exit 0 ;; *) exit 4 ;; esac ;;
  test) echo "cargo \$*" >>"$tmp/ran"; exit 0 ;;
  *) exec "$real_cargo" "\$@" ;;
esac
STUB
chmod +x "$tmp/bin/cargo"
if PATH="$tmp/bin:$PATH" AFFECTED_FILES=crates/strand-services-schema/src/lib.rs "$HERE/affected.sh" >/dev/null 2>&1 &&
  grep -q '^cargo test --doc -p strand-services-schema' "$tmp/ran"; then
  echo "ok   a crate with no test passes and reaches its doc-tests"
else
  echo "FAIL a crate with no test: $(cat "$tmp/ran" 2>/dev/null)"
  bad=1
fi

exit "$bad"
