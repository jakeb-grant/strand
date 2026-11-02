#!/usr/bin/env bash
# Tests what mutants.sh leaves for a red night's issue (eval-matrix.yml's
# `nightly-red` quotes target/eval/mutants/summary.md): a mutant that survives,
# one whose patch went stale and a scenario failing on HEAD are each named there
# and fail the run, and a run with every mutant caught leaves no summary. The real
# script, in a scratch repository, with a stand-in for `cargo`: the stand-in passes
# a scenario on a clean tree and fails its one check on a patched tree, or as
# asked by name. Run: scripts/container/run.sh bash scripts/eval/test-mutants-summary.sh

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
bad=0

repo=$tmp/repo
mkdir -p "$repo/scripts/eval" "$repo/eval/mutants" "$repo/eval/scenarios" "$tmp/bin"
cp "$ROOT/scripts/eval/mutants.sh" "$ROOT/scripts/eval/worktree.sh" "$repo/scripts/eval/"
printf 'target/\n' >"$repo/.gitignore"
printf 'on\n' >"$repo/eval/fix"
for s in caught survives red; do printf 'id = "%s"\n' "$s" >"$repo/eval/scenarios/$s.scn"; done

# Reverts the fix, as a mutant's patch does.
printf -- '--- a/eval/fix\n+++ b/eval/fix\n@@ -1 +1 @@\n-on\n+off\n' >"$repo/eval/mutants/reverts.patch"
printf -- '--- a/eval/fix\n+++ b/eval/fix\n@@ -1 +1 @@\n-gone\n+off\n' >"$repo/eval/mutants/stale.patch"

cat >"$tmp/bin/cargo" <<'STUB'
#!/usr/bin/env bash
# `cargo run ... --manifest-path TREE/eval/Cargo.toml --bin strand-eval -- check SCN --frames all --out OUT`
manifest='' scn='' out=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --manifest-path) manifest=$2; shift ;;
    check) scn=$2; shift ;;
    --out) out=$2; shift ;;
  esac
  shift
done
[ -n "$scn" ] || exit 0 # the A/B page
id=$(basename "$scn" .scn)
tree=$(dirname "$(dirname "$manifest")")
mkdir -p "$out"
[ "$id" = red ] && { echo "$id settle: FAIL" >"$out/summary.txt"; exit 1; }
if [ "$id" = caught ] && [ "$(cat "$tree/eval/fix")" = off ]; then
  printf '%s settle: FAIL\n' "$id" >"$out/summary.txt"
  exit 1
fi
: >"$out/summary.txt"
exit 0
STUB
chmod +x "$tmp/bin/cargo"

git -C "$repo" init -q
git -C "$repo" add -A
git -C "$repo" -c user.name=t -c user.email=t@example.invalid commit -qm scratch

# run EXIT WANTED... with eval/mutants/mutants.txt from stdin: the run exits
# EXIT, and summary.md holds each WANTED, or is absent when none is given.
run() {
  local want=$1 name=$2 got=0 summary=$repo/target/eval/mutants/summary.md
  shift 2
  cat >"$repo/eval/mutants/mutants.txt"
  (cd "$repo" && PATH="$tmp/bin:$PATH" CARGO_TARGET_DIR=target bash scripts/eval/mutants.sh) >"$tmp/log" 2>&1 || got=$?
  if [ "$got" != "$want" ]; then
    echo "FAIL $name: exit $got, wanted $want"
    sed 's/^/     /' "$tmp/log"
    bad=1
    return
  fi
  if [ "$#" = 0 ]; then
    if [ -e "$summary" ]; then echo "FAIL $name: a summary was left"; bad=1; return; fi
  else
    for word in "$@"; do
      if ! grep -qF -- "$word" "$summary" 2>/dev/null; then
        echo "FAIL $name: summary.md doesn't say \`$word\`"
        bad=1
        return
      fi
    done
  fi
  echo "ok   $name"
}

run 0 "every mutant caught leaves no summary" <<<'reverts.patch caught.scn settle'
run 1 "a mutant that survives is named" "1 of 1 failed" "reverts survived" "survives.scn" <<<'reverts.patch survives.scn settle'
run 1 "a stale patch is named" "stale is stale" "stale.patch" <<<'stale.patch caught.scn settle'
run 1 "a scenario failing on HEAD is named" "red.scn does not pass on HEAD" <<<'reverts.patch red.scn settle'
run 1 "the failures of one run are listed together" "2 of 3 failed" "stale is stale" "reverts survived" <<'LIST'
reverts.patch caught.scn settle
stale.patch caught.scn settle
reverts.patch survives.scn settle
LIST
run 0 "and the next clean run takes the old summary away" <<<'reverts.patch caught.scn settle'

[ "$bad" = 0 ] && echo "ok   mutants.sh's summary"
exit "$bad"
