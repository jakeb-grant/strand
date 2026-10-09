#!/usr/bin/env bash
# The scenario scripts/container/capture-niri.sh runs inside the matrix
# image, as an unprivileged user; writes to /out.
set -uo pipefail
OUT=/out
RT=$(mktemp -d /tmp/cap.XXXXXX); chmod 700 "$RT"
mkdir -p "$RT/host" "$RT/niri"; chmod 700 "$RT/host" "$RT/niri"
printf 'xwayland disable\ndefault_border none\noutput HEADLESS-1 resolution 1280x720 position 0 0 scale 1\n' >"$RT/host/sway.cfg"
env -u WAYLAND_DISPLAY XDG_RUNTIME_DIR="$RT/host" WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
  sway -c "$RT/host/sway.cfg" >"$OUT/sway.log" 2>&1 &
SWAY=$!
for _ in $(seq 100); do ls "$RT/host" | grep -q '^wayland-[0-9]*$' && break; sleep 0.1; done
HOST=$RT/host/$(ls "$RT/host" | grep '^wayland-[0-9]*$' | head -1)
cat >"$RT/niri.kdl" <<'EOF'
hotkey-overlay {
    skip-at-startup
}
animations {
    off
}
prefer-no-csd
layout {
    gaps 8
}
workspace "chat"
EOF
env XDG_RUNTIME_DIR="$RT/niri" WAYLAND_DISPLAY="$HOST" LIBGL_ALWAYS_SOFTWARE=1 \
  niri -c "$RT/niri.kdl" >"$OUT/niri.log" 2>&1 &
NIRI=$!
for _ in $(seq 200); do ls "$RT/niri" | grep -q '^niri\..*\.sock$' && break; sleep 0.1; done
export XDG_RUNTIME_DIR=$RT/niri
export NIRI_SOCKET=$RT/niri/$(ls "$RT/niri" | grep '^niri\..*\.sock$' | head -1)
export WAYLAND_DISPLAY=$(ls "$RT/niri" | grep '^wayland-[0-9]*$' | head -1)
niri msg version >"$OUT/version.txt" 2>&1

req() { # NAME JSON
  printf '%s\n' "$2" | socat -t1 - UNIX-CONNECT:"$NIRI_SOCKET" >"$OUT/$1"
}
snap() { # DIR
  mkdir -p "$OUT/$1"
  req "$1/reply-workspaces.json" '"Workspaces"'
  req "$1/reply-windows.json" '"Windows"'
  req "$1/reply-focused-output.json" '"FocusedOutput"'
  req "$1/reply-outputs.json" '"Outputs"'
  req "$1/reply-focused-window.json" '"FocusedWindow"'
}
act() { local l=$1; shift; echo "## $l" >>"$OUT/marks.txt"; echo "$(date +%s.%N) $l" >>"$OUT/marks-stamped.txt"; "$@"; sleep 1; }
wid() { niri msg --json windows | jq ".[] | select(.app_id==\"$1\") | .id"; }

snap boot
( printf '"EventStream"\n'; sleep 600 ) | socat - UNIX-CONNECT:"$NIRI_SOCKET" 2>/dev/null |
  while IFS= read -r line; do printf '%s %s\n' "$(date +%s.%N)" "$line"; done >"$OUT/events-stamped.txt" &
sleep 1
act open-term sh -c 'foot --app-id term --title "~" sh -c "sleep 600" >/dev/null 2>&1 & sleep 1'
act open-editor sh -c 'foot --app-id editor --title "notes.txt" sh -c "sleep 600" >/dev/null 2>&1 & sleep 1'
act open-titled sh -c 'foot --app-id titled --title "first" sh -c "sleep 2; printf \"\\033]2;second, with a comma\\007\"; sleep 600" >/dev/null 2>&1 & sleep 1'
sleep 2
snap opened
act focus-term niri msg action focus-window --id "$(wid term)"
act move-term-to-2 niri msg action move-window-to-workspace 2
snap moved
act focus-chat niri msg action focus-workspace chat
act focus-empty niri msg action focus-workspace 3
act focus-chat-again niri msg action focus-workspace chat
act action-ok req action-ok.json "{\"Action\":{\"FocusWindow\":{\"id\":$(wid editor)}}}"
act action-unknown-workspace req action-err.json '{"Action":{"FocusWorkspace":{"reference":{"Id":99}}}}'
act request-unknown req request-unknown.json '"NoSuchRequest"'
act close-focused niri msg action close-window
snap closed
act reload sh -c "printf '// edited\n' >>$RT/niri.kdl; sleep 1"
act reload-failed sh -c "printf 'this is not kdl {{{\n' >>$RT/niri.kdl; sleep 1"
act reload-fixed sh -c "sed -i '\$d' $RT/niri.kdl; sleep 1"
act close-rest sh -c 'pkill -x foot; sleep 1'
snap end
exit 0
