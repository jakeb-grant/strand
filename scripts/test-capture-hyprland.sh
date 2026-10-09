#!/usr/bin/env bash
# Offline test of scripts/capture-hyprland.sh against a fake Hyprland:
# .socket.sock answers every request with the current state number, and
# .socket2.sock sends "ev>>N" events, each 50 ms after the state became N
# (an event can trail the change it reports). Events run at the start and
# at the end of the capture, so the script must wait for a quiet moment
# to read its replies. Checks that each mark in marks.txt splits the
# stream exactly where its replies fall: the replies' state number equals
# the number of events before the mark.
#
# Needs only bash and socat; touches no real compositor.
# Usage: scripts/test-capture-hyprland.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT
fake=$work/hypr
mkdir -p "$fake"
echo 0 >"$fake/state"

cat >"$work/emit.sh" <<'EMIT'
#!/usr/bin/env bash
# One subscriber: busy for ~1 s, quiet, busy again from ~2.2 s to ~3.2 s.
state=$1
n=0
burst() {
  local i
  for ((i = 0; i < 10; i++)); do
    n=$((n + 1))
    echo "$n" >"$state"
    sleep 0.05
    echo "ev>>$n"
    sleep 0.05
  done
}
burst
sleep 1.2
burst
sleep 30
EMIT
chmod +x "$work/emit.sh"

cat >"$work/reply.sh" <<'REPLY'
#!/usr/bin/env bash
# One request per connection, as Hyprland: read it, answer, close.
read -r -n64 -t1 _ || true
printf '{"n": %s}' "$(cat "$1")"
REPLY
chmod +x "$work/reply.sh"
socat "UNIX-LISTEN:$fake/.socket.sock,fork" "EXEC:$work/reply.sh $fake/state" &
pids+=($!)
socat "UNIX-LISTEN:$fake/.socket2.sock" "EXEC:$work/emit.sh $fake/state" &
pids+=($!)
for _ in $(seq 50); do
  [ -S "$fake/.socket.sock" ] && [ -S "$fake/.socket2.sock" ] && break
  sleep 0.05
done

out=$work/out
HYPRLAND_DIR=$fake QUIET=0.3 TRIES=40 "$here/capture-hyprland.sh" 2 "$out"

fail=0
check() {
  local name=$1 dir=$2 mark want got
  mark=$(awk -v n="$name" '$1 == n { print $2 }' "$out/marks.txt")
  [[ $mark =~ ^[0-9]+$ ]] || { echo "FAIL $name: no quiet mark ($mark)"; fail=1; return; }
  want=$(head -n "$mark" "$out/events.txt" | grep -c '^ev>>' || true)
  got=$(sed -n 's/^{"n": \([0-9]*\)}$/\1/p' "$dir/clients.json")
  if [ "$got" = "$want" ]; then
    echo "ok   $name: replies at state $got, after stream line $mark"
  else
    echo "FAIL $name: replies at state $got, but $want events before line $mark"
    fail=1
  fi
}
check start "$out/start"
check end "$out/end"
total=$(grep -c '^ev>>' "$out/events.txt" || true)
[ "$total" -ge 10 ] || { echo "FAIL: only $total events captured"; fail=1; }
exit "$fail"
