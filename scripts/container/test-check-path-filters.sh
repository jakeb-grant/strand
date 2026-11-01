#!/usr/bin/env bash
# Tests scripts/container/check-path-filters.sh on a toy workflow and tree,
# then holds ci.yml's own filters to this checkout.
# Run: scripts/container/run.sh scripts/container/test-check-path-filters.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
CHECK=$HERE/check-path-filters.sh
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0

ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }
# expect WANT NAME [GREP]: the check on $DIR/w.yml and $DIR/tree exits
# WANT and, with GREP, prints it.
expect() {
  local want=$1 name=$2 pat=${3-} got=0
  "$CHECK" "$DIR/w.yml" "$DIR/tree" >"$DIR/out" 2>&1 || got=$?
  if [ "$got" != "$want" ]; then
    no "$name: exit $got, wanted $want"
    sed 's/^/     /' "$DIR/out"
  elif [ -n "$pat" ] && ! grep -q -- "$pat" "$DIR/out"; then
    no "$name: no '$pat' in the output"
    sed 's/^/     /' "$DIR/out"
  else
    ok "$name"
  fi
}

# workflow ENTRY...: a job with one filter of these entries, a comment, a
# list after the block that is not an entry, and a `run:` the same.
workflow() {
  {
    cat <<'EOF'
jobs:
  j:
    steps:
      - uses: dorny/paths-filter@v4
        with:
          filters: |
            a:
              # - 'a comment/is/no/entry'
EOF
    printf "              - '%s'\n" "$@"
    cat <<'EOF'
      - uses: actions/upload-artifact@v7
        with:
          path: |
            - 'not/a/filter/entry'
EOF
  } >"$DIR/w.yml"
}

git init -q "$DIR/tree"
mkdir -p "$DIR/tree/crates/one/src/deep" "$DIR/tree/scripts"
touch "$DIR/tree/crates/one/src/deep/session_lock.rs" "$DIR/tree/scripts/lockvm.sh" "$DIR/tree/Cargo.lock"
git -C "$DIR/tree" add .
touch "$DIR/tree/untracked.rs"

workflow Cargo.lock 'crates/**/*session_lock*' 'scripts/*lockvm*' 'crates/one/**'
expect 0 "literal paths and globs that match" "all 4 entries match"

workflow Cargo.lock crates/strand/src/lock.rs
expect 1 "a literal path that matches nothing fails and is named" "crates/strand/src/lock.rs"

workflow Cargo.lock 'crates/strand/src/services/auth*'
expect 1 "a glob that matches nothing fails and is named" "services/auth\*"

workflow gone/one.rs Cargo.lock gone/two.rs
expect 1 "every dead entry is named, not the first alone" "gone/two.rs"

workflow untracked.rs
expect 1 "a file not tracked does not count"

workflow crates/one/src/deep/session_lock.rs
git -C "$DIR/tree" rm -q --cached crates/one/src/deep/session_lock.rs
expect 1 "a path removed from the index fails"
git -C "$DIR/tree" add crates/one/src/deep/session_lock.rs

workflow 'crates/*/session_lock.rs'
expect 1 "\`*\` does not cross a directory"

workflow 'Cargo.[l]ock'
expect 0 "a character class is a glob"

workflow 'crates/{one,two}/**'
expect 2 "braces are refused rather than guessed at"

workflow '!Cargo.lock'
expect 2 "negation is refused rather than guessed at"

printf 'jobs:\n  j:\n    steps:\n      - run: true\n' >"$DIR/w.yml"
expect 2 "a workflow with no filter fails"

# Double quotes and none read the same.
cat >"$DIR/w.yml" <<'EOF'
          filters: |
            a:
              - "Cargo.lock"
              - scripts/lockvm.sh
              - "gone.rs"
EOF
expect 1 "double-quoted and bare entries are read" "gone.rs"

# The real one: the checkout's filters, and both of them seen.
if "$CHECK" >"$DIR/out" 2>&1; then
  ok "ci.yml's filters match this checkout: $(cat "$DIR/out")"
else
  no "ci.yml's filters"
  sed 's/^/     /' "$DIR/out"
fi
root=$(cd "$HERE/../.." && pwd)
for f in lock package; do
  if grep -q "^            $f:\$" "$root/.github/workflows/ci.yml"; then
    ok "ci.yml has the $f filter"
  else
    no "ci.yml has no $f filter"
  fi
done

[ "$bad" = 0 ] && echo "test-check-path-filters: all passed"
exit "$bad"
