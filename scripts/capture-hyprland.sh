#!/usr/bin/env bash
# Captures a running Hyprland's IPC traffic, read-only, for the fixture
# crates/strand-services/tests/fixtures/hyprland-0.56.2-captured: the
# event socket (.socket2.sock) for SECONDS, subscribed first as the
# adapter subscribes, and the replies to the adapter's four requests
# (j/monitors, j/workspaces, j/clients, j/activewindow, written byte for
# byte as the adapter writes them) right after subscribing and again at
# the end, while the event stream is still open. Nothing else is sent: no
# dispatch, keyword or reload.
#
# Each set of replies is read while the stream is quiet: the script notes
# the stream's line count, reads the four replies, waits QUIET seconds and
# counts again, and repeats the read until no event line came in between.
# marks.txt then records the line after which each set was read
# ("start N", "end N"), so the stream can be split exactly where the
# replies fall. An event that changed the state but reached the socket
# more than QUIET seconds after the read could still slip past; if no
# quiet read happens within TRIES attempts the bracket is recorded as
# "start A-B" and the script exits 2.
#
# The capture holds the session's window titles and classes. Scrub them
# (consistently, in replies and events alike) before committing; the
# fixture's SOURCE.txt says what was replaced.
#
# Usage: scripts/capture-hyprland.sh [SECONDS] [OUT]
#        (default 60 s into target/hyprland-capture)
# Env:   QUIET (default 0.3 s), TRIES (default 20); HYPRLAND_DIR overrides
#        the socket directory (the script's own test uses a fake one).

set -euo pipefail
SECS=${1:-60}
OUT=${2:-target/hyprland-capture}
QUIET=${QUIET:-0.3}
TRIES=${TRIES:-20}
if [ -n "${HYPRLAND_DIR:-}" ]; then
  DIR=$HYPRLAND_DIR
else
  : "${HYPRLAND_INSTANCE_SIGNATURE:?not in a Hyprland session}"
  DIR=${XDG_RUNTIME_DIR:?}/hypr/$HYPRLAND_INSTANCE_SIGNATURE
fi
[ -S "$DIR/.socket.sock" ] && [ -S "$DIR/.socket2.sock" ] || { echo "no Hyprland sockets in $DIR" >&2; exit 1; }
command -v socat >/dev/null || { echo "socat is needed" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT/start" "$OUT/end"
replies() {
  local r
  for r in monitors workspaces clients activewindow; do
    printf 'j/%s' "$r" | socat -t2 - "UNIX-CONNECT:$DIR/.socket.sock" >"$1/$r.json"
  done
}

lines() { wc -l <"$OUT/events.txt"; }
bytes() { stat -c %s "$OUT/events.txt"; }

# Reads the replies into $2 until no event line arrives from just before
# the read to QUIET seconds after it; appends "$1 N" (or "$1 A-B") to
# marks.txt.
quiet_replies() {
  local name=$1 dir=$2 a b i size
  for ((i = 1; i <= TRIES; i++)); do
    size=$(bytes)
    a=$(lines)
    replies "$dir"
    sleep "$QUIET"
    b=$(lines)
    if [ "$size" -eq "$(bytes)" ]; then
      echo "$name $a" >>"$OUT/marks.txt"
      return 0
    fi
  done
  echo "$name $a-$b" >>"$OUT/marks.txt"
  echo "no quiet moment for the $name replies in $TRIES tries" >&2
  return 1
}

: >"$OUT/events.txt"
: >"$OUT/marks.txt"
socat -u "UNIX-CONNECT:$DIR/.socket2.sock" - >"$OUT/events.txt" &
events=$!
trap 'kill "$events" 2>/dev/null || true' EXIT
# The adapter subscribes before it reads; give socat time to connect.
sleep 0.2
kill -0 "$events" 2>/dev/null || { echo "could not subscribe to $DIR/.socket2.sock" >&2; exit 1; }
status=0
quiet_replies start "$OUT/start" || status=2
sleep "$SECS"
quiet_replies end "$OUT/end" || status=2
kill "$events" 2>/dev/null || true
wait "$events" 2>/dev/null || true
trap - EXIT
printf 'j/version' | socat -t2 - "UNIX-CONNECT:$DIR/.socket.sock" >"$OUT/version.json"
echo "captured $(lines) event lines over about $SECS s into $OUT; marks: $(tr '\n' ' ' <"$OUT/marks.txt")"
exit "$status"
