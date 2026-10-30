#!/usr/bin/env bash
# test-install.sh MODE: scripts/install.sh against the release archive in
# /dist (scripts/release/package.sh's OUT), inside a throwaway container
# as a user with an empty home; scripts/release/smoke.sh starts it. The
# releases it installs are /dist's archive under other names, served
# from a file:// copy of GitHub's API (STRAND_INSTALL_API).
#
#   full      installs, checks the programs, updates on both channels,
#             keeps versions in use, refuses what it must, uninstalls
#   nolibs    a system without Strand's libraries: refused, nothing kept
#   oldglibc  a glibc older than the builds need: refused, nothing kept
set -uo pipefail
MODE=${1:?mode}
INSTALL=/dist/install.sh
DATA=$HOME/.local/share/strand
BIN=$HOME/.local/bin
W=$(mktemp -d)
bad=0
ok() { echo "ok   $*"; }
fail() {
  echo "FAIL $*"
  bad=1
}
# check NAME CMD...: CMD succeeds.
check() {
  local name=$1
  shift
  if "$@"; then ok "$name"; else fail "$name"; fi
}

src=$(ls /dist/strand-*-x86_64-linux.tar.gz)
orig=${src##*/strand-}
orig=${orig%-x86_64-linux.tar.gz}
mkdir -p "$W/x" "$W/dl" "$W/api/releases/tags"
tar -xzf "$src" -C "$W/x"

# release ID: /dist's archive as strand-ID.
release() {
  local id=$1 name=strand-$1-x86_64-linux.tar.gz
  rm -rf "$W/s" && mkdir "$W/s"
  cp -a "$W/x/strand-$orig" "$W/s/strand-$id"
  tar -C "$W/s" -czf "$W/dl/$name" "strand-$id"
  (cd "$W/dl" && sha256sum "$name" >"$name.sha256")
}
# serve REF ID: the API's REF (latest, tags/v0.1.0, tags/edge) lists ID's archive.
serve() {
  printf '{"tag_name":"x","assets":[{"name":"strand-%s-x86_64-linux.tar.gz","browser_download_url":"file://%s/dl/strand-%s-x86_64-linux.tar.gz"},{"name":"strand-%s-x86_64-linux.tar.gz.sha256","browser_download_url":"file://%s/dl/strand-%s-x86_64-linux.tar.gz.sha256"}]}' \
    "$2" "$W" "$2" "$2" "$W" "$2" >"$W/api/releases/$1"
}
export STRAND_INSTALL_API=file://$W/api
run() { sh "$INSTALL" "$@" >"$W/out" 2>&1; }
said() { grep -q -- "$1" "$W/out" || { cat "$W/out"; return 1; }; }
current() { readlink "$DATA/current"; }
nothing_installed() { [ ! -e "$BIN/strand" ] && [ -z "$(ls -A "$DATA/versions" 2>/dev/null)" ] && [ ! -e "$DATA/current" ]; }

release 0.1.0
serve latest 0.1.0
serve tags/v0.1.0 0.1.0

case $MODE in
nolibs)
  check "without the libraries: refused" bash -c '! sh '"$INSTALL"' >'"$W/out"' 2>&1'
  check "it names the missing libraries" said "missing: libpipewire-0.3.so.0"
  check "and how to install them" said "Debian/Ubuntu: sudo apt install"
  check "nothing installed" nothing_installed
  ;;
oldglibc)
  check "an old glibc: refused" bash -c '! sh '"$INSTALL"' >'"$W/out"' 2>&1'
  check "it says which glibc" said "need glibc 2.39 or newer, and this system has 2.3"
  check "nothing installed" nothing_installed
  ;;
full)
  for id in edge-aaaaaaaaaaaa edge-bbbbbbbbbbbb edge-cccccccccccc; do release $id; done

  # A script cut short by the network runs nothing.
  head -c 3000 "$INSTALL" | sh >/dev/null 2>&1
  check "a truncated script changes nothing" nothing_installed

  check "installs the newest release" run
  check "  strand is linked into ~/.local/bin" test "$(readlink "$BIN/strand")" = "$DATA/current/strand"
  check "  current is 0.1.0" test "$(current)" = versions/0.1.0
  check "  strand --version runs" bash -c '"$0"/strand --version | grep -q "^strand "' "$BIN"
  check "  strand-dev runs" "$BIN/strand-dev" --version
  check "  strand-auth is beside strand, not linked" bash -c '[ -x "$0/versions/0.1.0/strand-auth" ] && [ ! -e "$1/strand-auth" ]' "$DATA" "$BIN"
  check "  the licenses are there" test -s "$DATA/versions/0.1.0/THIRD-PARTY-LICENSES.txt"
  check "  says ~/.local/bin is not on PATH" said "is not on your PATH"
  check "  the channel is release" test "$(cat "$DATA/channel")" = release

  check "again: up to date" run
  check "  says so" said "installed already and up to date"

  # The lock's helper is found beside the installed strand: a config
  # with a lock checks. The same strand copied alone does not.
  "$BIN/strand" new "$HOME/cfg" >/dev/null
  printf '%s\n' 'lock {' '  state secret = ""' '  input { type: password; text: <-> secret' \
    '    on activate { if secret != "" { auth.submit(secret) }; secret = "" }' '  }' '}' \
    >"$HOME/cfg/lock.strand"
  check "a lock checks: the helper is found" "$BIN/strand" check "$HOME/cfg"
  mkdir "$W/alone" && cp "$DATA/current/strand" "$W/alone/"
  check "  (a strand without it does not)" bash -c '! "$0"/alone/strand check "$1" >/dev/null 2>&1' "$W" "$HOME/cfg"
  "$BIN/strand" replay --no-open >"$W/replay" 2>&1
  check "strand replay finds strand-eval" bash -c '! grep -q "replay needs strand-eval" "$0"' "$W/replay"

  serve tags/edge edge-aaaaaaaaaaaa
  check "--edge installs edge" run --edge
  check "  current is edge-aaaa" test "$(current)" = versions/edge-aaaaaaaaaaaa
  check "  the release before is kept" test -d "$DATA/versions/0.1.0"
  check "  the channel is edge" test "$(cat "$DATA/channel")" = edge

  # A program running from edge-aaaa (an editor's language server).
  "$BIN/strand-dev" lsp < <(sleep 600) >/dev/null 2>&1 &
  lsp=$!
  sleep 1
  serve tags/edge edge-bbbbbbbbbbbb
  check "again with no flag: edge is kept, bbbb installed" run
  check "  current is edge-bbbb" test "$(current)" = versions/edge-bbbbbbbbbbbb
  check "  0.1.0, neither current nor the one before, is removed" test ! -d "$DATA/versions/0.1.0"
  check "  says the running one must restart" said "is still running: restart it"
  serve tags/edge edge-cccccccccccc
  check "edge again: cccc" run
  check "  aaaa, still running, is kept" test -d "$DATA/versions/edge-aaaaaaaaaaaa"
  check "  and says so" said "Kept edge-aaaaaaaaaaaa"

  check "uninstall while it runs: refused" bash -c '! sh "$0" --uninstall >"$1" 2>&1' "$INSTALL" "$W/out"
  check "  says what runs" said "strand is running from"
  check "  nothing removed" test -d "$DATA/versions/edge-cccccccccccc"
  kill $lsp 2>/dev/null
  wait $lsp 2>/dev/null

  check "--version 0.1.0 goes back to the release" run --version 0.1.0
  check "  current is 0.1.0" test "$(current)" = versions/0.1.0
  check "  aaaa, no longer running, is removed" test ! -d "$DATA/versions/edge-aaaaaaaaaaaa"
  check "  the channel is release" test "$(cat "$DATA/channel")" = release

  # A damaged download changes nothing.
  serve tags/edge edge-bbbbbbbbbbbb
  rm -rf "$DATA/versions/edge-bbbbbbbbbbbb"
  cp "$W/dl/strand-edge-bbbbbbbbbbbb-x86_64-linux.tar.gz.sha256" "$W/good.sha256"
  printf '%064d  %s\n' 0 strand-edge-bbbbbbbbbbbb-x86_64-linux.tar.gz \
    >"$W/dl/strand-edge-bbbbbbbbbbbb-x86_64-linux.tar.gz.sha256"
  check "a checksum that does not match: refused" bash -c '! sh "$0" --edge >"$1" 2>&1' "$INSTALL" "$W/out"
  check "  says so" said "does not match its checksum"
  check "  current is still 0.1.0" test "$(current)" = versions/0.1.0
  cp "$W/good.sha256" "$W/dl/strand-edge-bbbbbbbbbbbb-x86_64-linux.tar.gz.sha256"

  check "uninstall" run --uninstall
  check "  removes the data and the links" bash -c '[ ! -e "$0" ] && [ ! -e "$1/strand" ] && [ ! -e "$1/strand-dev" ]' "$DATA" "$BIN"
  check "  keeps the config" test -f "$HOME/cfg/lock.strand"

  # A strand that is not the installer's (a copy from an older install).
  echo old >"$BIN/strand"
  check "a strand it did not put there: refused" bash -c '! sh "$0" >"$1" 2>&1' "$INSTALL" "$W/out"
  check "  says so" said "is not this installer's"
  check "  and leaves it" test "$(cat "$BIN/strand")" = old
  check "--force replaces it" run --force
  check "  with the link" test -L "$BIN/strand"

  check "--from installs an archive already downloaded" run --from "$W/dl/strand-edge-bbbbbbbbbbbb-x86_64-linux.tar.gz"
  check "  current is edge-bbbb" test "$(current)" = versions/edge-bbbbbbbbbbbb
  ;;
*)
  echo "test-install: no mode $MODE" >&2
  exit 2
  ;;
esac
[ "$bad" = 0 ] && echo "test-install $MODE: all passed"
exit "$bad"
