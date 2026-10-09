#!/usr/bin/env bash
# Captures a running Hyprland's IPC traffic, read-only, for the fixture
# crates/strand-services/tests/fixtures/hyprland-0.56.2-captured: the
# event socket (.socket2.sock) for SECONDS, subscribed first as the
# adapter subscribes, and the replies to the adapter's four requests
# (j/monitors, j/workspaces, j/clients, j/activewindow, written byte for
# byte as the adapter writes them) right after subscribing and again at
# the end. Nothing else is sent: no dispatch, keyword or reload.
#
# The capture holds the session's window titles and classes. Scrub them
# (consistently, in replies and events alike) before committing; the
# fixture's SOURCE.txt says what was replaced.
#
# Usage: scripts/capture-hyprland.sh [SECONDS] [OUT]
#        (default 60 s into target/hyprland-capture)

set -euo pipefail
SECS=${1:-60}
OUT=${2:-target/hyprland-capture}
: "${HYPRLAND_INSTANCE_SIGNATURE:?not in a Hyprland session}"
DIR=${XDG_RUNTIME_DIR:?}/hypr/$HYPRLAND_INSTANCE_SIGNATURE
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

timeout "$SECS" socat -u "UNIX-CONNECT:$DIR/.socket2.sock" - >"$OUT/events.txt" &
events=$!
sleep 0.2
replies "$OUT/start"
wait "$events" || true
replies "$OUT/end"
printf 'j/version' | socat -t2 - "UNIX-CONNECT:$DIR/.socket.sock" >"$OUT/version.json"
echo "captured $(wc -l <"$OUT/events.txt") event lines in $SECS s into $OUT"
