#!/usr/bin/env bash
# Tests matrix-pin.sh on a copy of the Dockerfile and against stand-ins for docker
# and curl: a recent pin passes and says its age, one past the limit fails naming
# the refresh, refresh rewrites both lines, and a date the archive lacks changes
# nothing. And the real Dockerfile.matrix has both lines.
# Run: scripts/container/run.sh scripts/container/test-matrix-pin.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }

mkdir "$DIR/bin"
cat >"$DIR/bin/curl" <<'STUB'
#!/usr/bin/env bash
[ -z "${NO_SNAPSHOT:-}" ]
STUB
cat >"$DIR/bin/docker" <<'STUB'
#!/usr/bin/env bash
case "$1" in
  pull) ;;
  image) echo "archlinux@sha256:feed" ;;
esac
STUB
chmod +x "$DIR/bin/curl" "$DIR/bin/docker"
export PATH="$DIR/bin:$PATH"
export MATRIX_PIN_FILE=$DIR/Dockerfile
unset GITHUB_STEP_SUMMARY
PIN=$HERE/matrix-pin.sh

grep -q '^FROM archlinux@sha256:' "$HERE/Dockerfile.matrix" && ok "the real base is pinned by digest" || no "no digest"
grep -q '^ARG ARCH_DATE=[0-9-]*$' "$HERE/Dockerfile.matrix" && ok "the real snapshot has a date" || no "no date"

file() { printf 'FROM archlinux@sha256:old\nARG ARCH_DATE=%s\n' "$1" >"$MATRIX_PIN_FILE"; }

file "$(date -u -d '3 days ago' +%F)"
"$PIN" check >"$DIR/out" 2>&1 && ok "a recent pin passes" || no "a recent pin failed"
grep -q '3 days ago' "$DIR/out" && ok "and says its age" || no "no age"
file "$(date -u -d '31 days ago' +%F)"
"$PIN" check >"$DIR/out" 2>&1 && no "an old pin passed" || ok "one past the limit fails"
grep -q 'matrix-pin.sh refresh' "$DIR/out" && ok "and names the refresh" || no "no refresh named"
MATRIX_PIN_NOTE=$DIR/note/pin.md "$PIN" check >/dev/null 2>&1
grep -q 'days ago' "$DIR/note/pin.md" && ok "the note for the issue says its age" || no "no note for the issue"
GITHUB_STEP_SUMMARY=$DIR/summary "$PIN" check >/dev/null 2>&1
grep -q '31 days ago' "$DIR/summary" && ok "the job summary gets it" || no "no job summary"

file 2020-01-01
"$PIN" refresh 2026-01-02 >/dev/null 2>&1 && ok "refresh passes" || no "refresh failed"
grep -q '^FROM archlinux@sha256:feed$' "$MATRIX_PIN_FILE" && ok "it rewrites the digest" || no "digest"
grep -q '^ARG ARCH_DATE=2026-01-02$' "$MATRIX_PIN_FILE" && ok "and the date" || no "date"
file 2020-01-01
NO_SNAPSHOT=1 "$PIN" refresh 2026-01-02 >/dev/null 2>&1 && no "a missing snapshot passed" || ok "a date the archive lacks fails"
grep -q '^ARG ARCH_DATE=2020-01-01$' "$MATRIX_PIN_FILE" && ok "and changes nothing" || no "changed"
exit "$bad"
