#!/usr/bin/env bash
# Tests scripts/container/prune-rust-caches.sh against a stand-in `gh`: the
# newest of each name kept per ref, a generation changed by either hash
# pruned, a branch's cache never taking main's, other caches left alone,
# a dry run deleting nothing, and a failed call failing the script.
# Run: scripts/container/run.sh scripts/container/test-prune-rust-caches.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
PRUNE=$HERE/prune-rust-caches.sh
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0

ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }

mkdir "$DIR/bin"
cat >"$DIR/bin/gh" <<'EOF'
#!/usr/bin/env bash
case "$1 $2" in
  "cache list")
    [ -z "${GH_FAIL_LIST-}" ] || exit 1
    # All of them, or the script's limit would hide some.
    case " $* " in *" --limit 1000 "*) ;; *) echo "no --limit" >&2; exit 1 ;; esac
    cat "$STUB/list.json"
    ;;
  "cache delete")
    [ "${GH_FAIL_ID-}" != "$3" ] || exit 1
    echo "$3" >>"$STUB/deleted"
    ;;
  *) echo "gh: unexpected: $*" >&2; exit 1 ;;
esac
EOF
chmod +x "$DIR/bin/gh"
export STUB=$DIR PATH="$DIR/bin:$PATH"

# entry ID KEY REF CREATED
entry() {
  printf '{"id":%s,"key":"%s","ref":"%s","sizeInBytes":1048576,"createdAt":"2026-%s:00Z"}' "$1" "$2" "$3" "$4"
}
M=refs/heads/main
{
  echo '['
  # Newest second, so order in the listing decides nothing.
  entry 1 v0-rust-test-Linux-x64-63ec8b6c-f61c49c3 $M 10-31T18:35; echo ,
  entry 2 v0-rust-test-Linux-x64-63ec8b6c-b4987f66 $M 11-01T13:37; echo ,
  entry 3 v0-rust-test-Linux-x64-63ec8b6c-aaaaaaaa $M 10-30T01:00; echo ,
  # A new env hash supersedes the old one too.
  entry 4 v0-rust-eval-Linux-x64-11111111-b4987f66 $M 10-20T01:00; echo ,
  entry 5 v0-rust-eval-Linux-x64-63ec8b6c-b4987f66 $M 11-01T13:30; echo ,
  # The only one of its name.
  entry 6 v0-rust-compositors-compositors-Linux-x64-63ec8b6c-b4987f66 $M 11-01T13:22; echo ,
  # A branch's newer cache leaves main's alone; its older one goes.
  entry 7 v0-rust-test-Linux-x64-63ec8b6c-cccccccc refs/heads/laptop/x 11-01T20:00; echo ,
  entry 8 v0-rust-test-Linux-x64-63ec8b6c-dddddddd refs/heads/laptop/x 11-01T19:00; echo ,
  # Not rust-cache's, however old.
  entry 9 cargo-about-0.9.3-rust-1.97.0 $M 01-01T00:00; echo ,
  entry 10 wasm-bindgen-cli-0.2.129-rust-1.97.0 $M 01-02T00:00; echo ,
  entry 11 cargo-about-0.9.2-rust-1.97.0 $M 01-03T00:00; echo ,
  # Not the shape of a key: left alone rather than guessed at.
  entry 12 v0-rust-odd $M 01-04T00:00
  echo ']'
} >"$DIR/list.json"

WANT=$(printf '%s\n' 1 3 4 8)
deleted() { sort -n "$DIR/deleted" 2>/dev/null; }

: >"$DIR/deleted"
if "$PRUNE" --dry-run >"$DIR/out" 2>&1 && [ ! -s "$DIR/deleted" ] &&
  [ "$(grep -c '^would delete' "$DIR/out")" = 4 ] && grep -q '4 caches, 4 MiB (dry run)' "$DIR/out"; then
  ok "a dry run names the four and deletes nothing"
else
  no "the dry run"
  sed 's/^/     /' "$DIR/out"
fi

: >"$DIR/deleted"
if "$PRUNE" >"$DIR/out" 2>&1 && [ "$(deleted)" = "$WANT" ]; then
  ok "only the older of each name in each ref goes"
else
  no "deleted $(deleted | tr '\n' ' '), wanted $(echo "$WANT" | tr '\n' ' ')"
  sed 's/^/     /' "$DIR/out"
fi

: >"$DIR/deleted"
got=0
GH_FAIL_ID=3 "$PRUNE" >"$DIR/out" 2>&1 || got=$?
if [ "$got" = 1 ] && [ "$(deleted | tr '\n' ' ')" = "1 4 8 " ] && grep -q '::warning::could not delete' "$DIR/out"; then
  ok "a delete that fails warns, goes on, and fails the run"
else
  no "a failed delete: exit $got, deleted $(deleted | tr '\n' ' ')"
  sed 's/^/     /' "$DIR/out"
fi

: >"$DIR/deleted"
got=0
GH_FAIL_LIST=1 "$PRUNE" >"$DIR/out" 2>&1 || got=$?
if [ "$got" != 0 ] && [ ! -s "$DIR/deleted" ]; then
  ok "a listing that fails fails the run and deletes nothing"
else
  no "a failed listing: exit $got"
fi

echo '[]' >"$DIR/list.json"
if "$PRUNE" >"$DIR/out" 2>&1 && grep -q ' 0 caches' "$DIR/out"; then
  ok "no caches: nothing to do"
else
  no "an empty listing"
  sed 's/^/     /' "$DIR/out"
fi

got=0
"$PRUNE" --bogus >"$DIR/out" 2>&1 || got=$?
if [ "$got" = 2 ]; then ok "an unknown argument is refused"; else no "--bogus: exit $got"; fi

[ "$bad" = 0 ] && echo "test-prune-rust-caches: all passed"
exit "$bad"
