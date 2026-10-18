#!/usr/bin/env bash
# Tests scripts/eval/local-mounts.sh on a toy checkout: what `run.sh eval
# local` mounts read-only (docs/eval.md, "Local configs and fonts").
# Run: scripts/container/run.sh bash scripts/eval/test-local-mounts.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0

ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }
has() { # NAME WORD: the mounts hold WORD
  local w
  for w in "${mounts[@]}"; do [ "$w" = "$2" ] && { ok "$1"; return; }; done
  no "$1: no \`$2\` in: ${mounts[*]}"
}
lacks() { # NAME TEXT: no mount mentions TEXT
  case " ${mounts[*]} " in *"$2"*) no "$1: \`$2\` in: ${mounts[*]}" ;; *) ok "$1" ;; esac
}

ROOT=$DIR/checkout
mkdir -p "$ROOT/eval/scenarios/local" "$ROOT/inside" "$DIR/shells/a" "$DIR/shells/b" "$DIR/shells/c" "$DIR/elsewhere"
touch "$DIR/shells/a/shell.strand" "$DIR/shells/b/shell.strand"
export TEST_SHELLS=$DIR/shells
cat >"$ROOT/eval/scenarios/local/a.scn" <<SCN
id     = "a"
config = "$DIR/shells/a/shell.strand"
=== steps
config = "$DIR/elsewhere/not-a-header.strand"
rest
SCN
cat >"$ROOT/eval/scenarios/local/b.scn" <<'SCN'
id         = "b"
config_dir = "${TEST_SHELLS}/b"
=== steps
rest
SCN
cat >"$ROOT/eval/scenarios/local/c.scn" <<SCN
id     = "c"
config = "$ROOT/inside/shell.strand"
=== steps
rest
SCN

mounts=()
envs=()
# shellcheck source=local-mounts.sh
source "$HERE/local-mounts.sh" new d --config "$DIR/shells/c" 2>"$DIR/err"
has "a config named by an absolute path is mounted read-only at its own path" "$DIR/shells/a:$DIR/shells/a:ro"
has "a \${VAR} in a header is the host's" "$DIR/shells/b:$DIR/shells/b:ro"
has "an absolute path among the arguments is mounted" "$DIR/shells/c:$DIR/shells/c:ro"
lacks "a path in the steps is not a header's" "$DIR/elsewhere"
lacks "the checkout itself is run.sh's already" "$ROOT/inside"
n=0
for w in "${mounts[@]}"; do case "$w" in *":ro") ;; -v) ;; *) n=1 ;; esac; done
[ "$n" = 0 ] && ok "every mount is read-only" || no "a mount is not read-only: ${mounts[*]}"
conf=$ROOT/target/eval/local/fonts.conf
grep -q '<include ignore_missing="yes">/etc/fonts/fonts.conf</include>' "$conf" &&
  ok "the fontconfig file includes the image's" || no "the fontconfig file lacks the image's: $(cat "$conf")"
case " ${envs[*]} " in
  *"FONTCONFIG_FILE=$conf"*) ok "FONTCONFIG_FILE names it" ;;
  *) no "FONTCONFIG_FILE is not set: ${envs[*]}" ;;
esac
for d in "${local_fonts[@]}"; do
  grep -q "<dir>$d</dir>" "$conf" || no "font directory $d is not in the fontconfig file"
done
ok "each host font directory mounted is in the fontconfig file (${#local_fonts[@]})"
grep -q "read-only" "$DIR/err" && ok "it says what it mounted" || no "it printed nothing: $(cat "$DIR/err")"

exit "$bad"
