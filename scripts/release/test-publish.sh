#!/usr/bin/env bash
# test-publish.sh: publish.sh against a stand-in `gh` that keeps its
# releases in a directory: edge made once and its builds replaced, the
# checksum uploaded before its archive, the build before kept and older
# ones removed, nothing published once main has moved on, a tag's
# release made from its own version only.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
bad=0
ok() { echo "ok   $*"; }
fail() {
  echo "FAIL $*"
  bad=1
}
check() {
  local name=$1
  shift
  if "$@"; then ok "$name"; else fail "$name"; fi
}

mkdir -p "$W/bin" "$W/gh"
# The stand-in: releases/<tag>/<asset> with a creation counter; every
# call is logged to $W/log.
cat >"$W/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
S=$STUB_DIR
echo "gh $*" >>"$S/log"
n() { c=$(cat "$S/counter" 2>/dev/null || echo 0); echo $((c + 1)) >"$S/counter"; echo $((c + 1)); }
put() { local rel=$1 f=$2; cp "$f" "$S/rel/$rel/${f##*/}"; n >"$S/rel/$rel/.${f##*/}.t"; }
case "$1 $2" in
"api repos/"*) echo "$STUB_HEAD" ;;
"release view")
  [ -d "$S/rel/$3" ] || exit 1
  if [ "${4-}" = --json ]; then
    case $6 in
    '.assets[].name') ls "$S/rel/$3" ;;
    *) for f in "$S/rel/$3"/*; do echo "$(cat "$S/rel/$3/.${f##*/}.t") ${f##*/}"; done | sort -rn | cut -d' ' -f2 ;;
    esac
  fi
  ;;
"release create")
  rel=$3
  mkdir "$S/rel/$rel"
  shift 3
  for a in "$@"; do if [ -f "$a" ]; then put "$rel" "$a"; fi; done
  ;;
"release upload")
  rel=$3
  shift 3
  for a in "$@"; do if [ -f "$a" ]; then put "$rel" "$a"; fi; done
  ;;
"release edit") ;;
"release delete-asset") rm "$S/rel/$3/$4" "$S/rel/$3/.$4.t" ;;
*) echo "stub gh: $*" >&2; exit 2 ;;
esac
EOF
chmod +x "$W/bin/gh"
mkdir -p "$W/gh/rel"
export STUB_DIR=$W/gh PATH=$W/bin:$PATH GITHUB_REPOSITORY=o/strand

# dist ID: a dist with ID's archive, checksum and install.sh.
dist() {
  rm -rf "$W/dist" && mkdir "$W/dist"
  echo "$1" >"$W/dist/strand-$1-x86_64-linux.tar.gz"
  echo "sum" >"$W/dist/strand-$1-x86_64-linux.tar.gz.sha256"
  echo "#!/bin/sh" >"$W/dist/install.sh"
}
# push SHA [HEAD]: publish SHA's edge build from main, main at HEAD.
push() {
  dist "edge-${1::12}"
  GITHUB_REF=refs/heads/main GITHUB_SHA=$1 STUB_HEAD=${2:-$1} "$HERE/publish.sh" "$W/dist" >"$W/out" 2>&1
}
edge() { ls "$W/gh/rel/edge" 2>/dev/null | tr '\n' ' '; }
A=$(printf 'a%.0s' {1..40}) B=$(printf 'b%.0s' {1..40}) C=$(printf 'c%.0s' {1..40})

check "the first push to main makes edge" push "$A"
check "  with its build and install.sh" test "$(edge)" = "install.sh strand-edge-aaaaaaaaaaaa-x86_64-linux.tar.gz strand-edge-aaaaaaaaaaaa-x86_64-linux.tar.gz.sha256 "
check "  made once, at that commit" grep -q "release create edge --prerelease --target $A" "$W/gh/log"
first_sum=$(grep -n "upload edge .*\.sha256" "$W/gh/log" | head -1 | cut -d: -f1)
first_tar=$(grep -n "upload edge .*\.tar\.gz --clobber" "$W/gh/log" | head -1 | cut -d: -f1)
check "  the checksum is uploaded before the archive" test "$first_sum" -lt "$first_tar"
check "the next push adds its build" push "$B"
check "  the one before stays" test "$(edge)" = "install.sh strand-edge-aaaaaaaaaaaa-x86_64-linux.tar.gz strand-edge-aaaaaaaaaaaa-x86_64-linux.tar.gz.sha256 strand-edge-bbbbbbbbbbbb-x86_64-linux.tar.gz strand-edge-bbbbbbbbbbbb-x86_64-linux.tar.gz.sha256 "
check "  edge is made only once" test "$(grep -c "release create edge" "$W/gh/log")" = 1
check "a third push" push "$C"
check "  removes the oldest build" test "$(edge)" = "install.sh strand-edge-bbbbbbbbbbbb-x86_64-linux.tar.gz strand-edge-bbbbbbbbbbbb-x86_64-linux.tar.gz.sha256 strand-edge-cccccccccccc-x86_64-linux.tar.gz strand-edge-cccccccccccc-x86_64-linux.tar.gz.sha256 "
before=$(edge)
check "a push main has moved on from: no failure" push "$A" "$C"
check "  and nothing published" test "$(edge)" = "$before"
grep -q "main is at cccccccccccc now" "$W/out" && ok "  says why" || fail "  says why"

dist 0.1.0
check "a tag makes its release" env GITHUB_REF=refs/tags/v0.1.0 GITHUB_SHA="$A" "$HERE/publish.sh" "$W/dist"
check "  from the tag" grep -q "release create v0.1.0 --verify-tag" "$W/gh/log"
check "  with the archive" test -f "$W/gh/rel/v0.1.0/strand-0.1.0-x86_64-linux.tar.gz"
check "a tag of another version: refused" bash -c '! env GITHUB_REF=refs/tags/v0.2.0 GITHUB_SHA="$1" "$0" "$2" >/dev/null 2>&1' "$HERE/publish.sh" "$A" "$W/dist"
check "  and nothing made" test ! -d "$W/gh/rel/v0.2.0"

[ "$bad" = 0 ] && echo "test-publish: all passed"
exit "$bad"
