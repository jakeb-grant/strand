#!/usr/bin/env bash
# M0 exit gates (docs/features.md, "M0 Spike"), measured on a headless sway
# with two 2560x1440 outputs (HEADLESS-1 at scale 1.0, HEADLESS-2 at 1.25)
# running `strand run --demo`:
#
#   1. PSS   total from /proc/<pid>/smaps_rollup              gate <= 34 MB
#   2. idle  voluntary + involuntary context switches summed
#            over /proc/<pid>/task/*/status, over a window
#            strictly between two minute ticks (:03 -> :57)   gate: 0
#   3. damage area per clock tick (STRAND_LOG=damage)        gate <= 2000 px^2
#      checked per committed frame and per tick (all outputs together); every
#      tick's damage must also be centred on its bar (the clock)
#
# Then hotplugs a third output, HEADLESS-3 at 1920x1080 and scale 1.0 (the
# same scale as HEADLESS-1, another width), checks that only the new bar
# paints, and checks one more tick on all three bars (gate and centring).
# Also saves grim screenshots of the bars to docs/images/ and runs the
# strand-core 10k-node benchmark (skip with --no-bench).
#
# Usage: scripts/m0-exit.sh [--no-bench] [--no-build] [--no-third] [--ticks N]
# Env:   STRAND_BIN (default target/release/strand), OUT (results dir).
# Exit status is non-zero when a gate fails.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BENCH=1
BUILD=1
THIRD=1
TICKS=2
while [ $# -gt 0 ]; do
  case "$1" in
    --no-bench) BENCH=0 ;;
    --no-build) BUILD=0 ;;
    --no-third) THIRD=0 ;;
    --ticks) TICKS=$2; shift ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
[ "$TICKS" -ge 1 ] || { echo "--ticks must be >= 1" >&2; exit 2; }

PSS_GATE_KB=$((34 * 1024))
DAMAGE_GATE=2000
BIN=${STRAND_BIN:-$ROOT/target/release/strand}
OUT=${OUT:-$ROOT/target/m0-exit}
mkdir -p "$OUT"

for tool in sway swaymsg grim awk date; do
  command -v "$tool" >/dev/null || { echo "$tool is not installed" >&2; exit 2; }
done

if [ "$BUILD" = 1 ]; then
  (cd "$ROOT" && cargo build --release -p strand)
fi
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 2; }

# The sway IPC socket path must fit in sun_path: keep the runtime dir short.
RUNDIR=$(mktemp -d /tmp/strand-m0.XXXXXX)
chmod 700 "$RUNDIR"
SWAY_PID=
STRAND_PID=
cleanup() {
  [ -n "$STRAND_PID" ] && kill "$STRAND_PID" 2>/dev/null || true
  [ -n "$SWAY_PID" ] && kill "$SWAY_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$RUNDIR"
}
trap cleanup EXIT

printf 'xwayland disable\noutput HEADLESS-1 resolution 2560x1440 scale 1\n' >"$RUNDIR/sway.cfg"
env -u WAYLAND_DISPLAY -u SWAYSOCK -u DISPLAY XDG_RUNTIME_DIR="$RUNDIR" \
  WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
  sway -c "$RUNDIR/sway.cfg" >"$OUT/sway.log" 2>&1 &
SWAY_PID=$!
export XDG_RUNTIME_DIR=$RUNDIR
for _ in $(seq 100); do
  SOCK=$(ls "$RUNDIR"/sway-ipc.*.sock 2>/dev/null | head -1 || true)
  DISP=$(ls "$RUNDIR" | grep -E '^wayland-[0-9]+$' | head -1 || true)
  [ -n "$SOCK" ] && [ -n "$DISP" ] && break
  sleep 0.1
done
[ -n "${SOCK:-}" ] || { echo "sway did not start (see $OUT/sway.log)" >&2; exit 1; }
export SWAYSOCK=$SOCK WAYLAND_DISPLAY=$DISP
swaymsg -q create_output
swaymsg -q output HEADLESS-2 resolution 2560x1440 scale 1.25
swaymsg -t get_outputs -r | grep -E '"(name|scale)"' | paste - - | tr -s ' ' >"$OUT/outputs.txt"

LOG=$OUT/strand.log
STRAND_LOG=damage "$BIN" run --demo 2>"$LOG" &
STRAND_PID=$!

frames() { grep -c '^strand: damage' "$LOG" || true; }
# Damage lines only, so offsets count frames whatever else is logged.
damage_lines() { grep '^strand: damage' "$LOG" || true; }
# Damage lines from frame number $1 on (1-based).
damage_from() { damage_lines | tail -n +"$1"; }
areas() { sed -n 's/.* area=\([0-9]*\).*/\1/p'; }
sum() { awk '{ s += $1 } END { print s + 0 }'; }
# Frames whose damage is not centred on their bar (a clock tick damages
# only the clock): the bounding box of the rects must be centred within 5%
# of the buffer width.
off_centre() {
  awk '{
    w = 0; minx = -1; maxx = -1
    for (i = 1; i <= NF; i++) {
      if ($i ~ /^buffer=/) { split(substr($i, 8), b, "x"); w = b[1] }
      if ($i ~ /^rects=/) {
        n = split(substr($i, 7), r, ",")
        for (j = 1; j <= n; j++) {
          split(r[j], p, "[x+]")
          if (minx < 0 || p[3] < minx) minx = p[3]
          if (p[3] + p[1] > maxx) maxx = p[3] + p[1]
        }
      }
    }
    c = (minx + maxx) / 2
    if (w == 0 || c < w / 2 - w * 0.05 || c > w / 2 + w * 0.05) print
  }'
}
# Wait until no damage line has appeared for 500 ms (late boot frames).
wait_quiet() {
  local n m
  n=$(frames)
  for _ in $(seq 40); do
    sleep 0.5
    m=$(frames)
    [ "$m" -eq "$n" ] && return 0
    n=$m
  done
}
# Both bars painted.
for _ in $(seq 100); do
  [ "$(frames)" -ge 2 ] && break
  kill -0 "$STRAND_PID" 2>/dev/null || { cat "$LOG" >&2; exit 1; }
  sleep 0.1
done
[ "$(frames)" -ge 2 ] || { echo "bars did not paint" >&2; cat "$LOG" >&2; exit 1; }
wait_quiet

pss_kb() { awk '/^Pss:/ { print $2 }' "/proc/$STRAND_PID/smaps_rollup"; }
switches() {
  local total=0 v n f
  for f in /proc/"$STRAND_PID"/task/*/status; do
    v=$(awk '/^voluntary_ctxt_switches/ { print $2 }' "$f")
    n=$(awk '/^nonvoluntary_ctxt_switches/ { print $2 }' "$f")
    total=$((total + v + n))
  done
  echo "$total"
}
per_thread() {
  local f t
  for f in /proc/"$STRAND_PID"/task/*; do
    t=$(cat "$f/comm")
    awk -v t="$t" '/ctxt_switches/ { s += $2 } END { printf "%s=%d ", t, s }' "$f/status"
  done
  echo
}
# Seconds past the minute, and sleeping until a given second.
sec() { date +%-S; }
sleep_until_sec() {
  local target=$1 now t
  now=$(date +%s%N)
  t=$((now - now % 60000000000 + target * 1000000000))
  [ "$t" -gt "$now" ] || t=$((t + 60000000000))
  sleep "$(((t - now) / 1000000000)).$(printf '%09d' $(((t - now) % 1000000000)))"
}

echo "outputs: $(tr '\n' ';' <"$OUT/outputs.txt")"
BOOT_FRAMES=$(frames)
echo "boot frames: $BOOT_FRAMES"
BOOT_PSS=$(pss_kb)

declare -a WINDOW_DELTA TICK_MAX TICK_AREA TICK_OFF
for i in $(seq "$TICKS"); do
  # Window strictly between two ticks: from :03 to :57 of one minute.
  [ "$(sec)" -ge 3 ] && [ "$(sec)" -lt 10 ] || sleep_until_sec 3
  before_frames=$(frames)
  s0=$(switches); t0=$(per_thread); w0=$(date +%T.%N)
  sleep_until_sec 57
  s1=$(switches); t1=$(per_thread); w1=$(date +%T.%N)
  WINDOW_DELTA[$i]=$((s1 - s0))
  echo "window $i: $w0 -> $w1  switches $s0 -> $s1 (delta ${WINDOW_DELTA[$i]})"
  echo "  threads before: $t0"
  echo "  threads after:  $t1"
  [ "$(frames)" -eq "$before_frames" ] || echo "  frames painted inside the window: $(( $(frames) - before_frames ))"
  # The tick: frames after the minute boundary.
  sleep_until_sec 3
  lines=$(damage_from $((before_frames + 1)))
  echo "tick $i frames:"
  echo "$lines" | sed 's/^/  /'
  TICK_MAX[$i]=$(echo "$lines" | areas | sort -n | tail -1)
  TICK_AREA[$i]=$(echo "$lines" | areas | sum)
  TICK_OFF[$i]=$(echo "$lines" | grep . | off_centre | grep -c . || true)
done
TICKS_END=$(frames)
PSS=$(pss_kb)
grep -E '^(Rss|Pss|Pss_Anon|Pss_File|Pss_Shmem|Private_Dirty|Shared_Clean):' "/proc/$STRAND_PID/smaps_rollup" >"$OUT/smaps_rollup.txt"
# Largest mappings by PSS, for the report.
awk '/^[0-9a-f]+-[0-9a-f]+ / { name = $6; if (name == "") name = "[anon]" }
     /^Pss:/ { pss[name] += $2 }
     END { for (n in pss) printf "%8d kB  %s\n", pss[n], n }' "/proc/$STRAND_PID/smaps" |
  sort -rn | head -25 >"$OUT/pss_by_mapping.txt"

mkdir -p "$ROOT/docs/images"
grim -g "0,0 2560x32" "$ROOT/docs/images/m0-bar.png"
grim -s 1.25 -g "2560,0 2048x32" "$ROOT/docs/images/m0-bar-125.png"

if [ "$THIRD" = 1 ]; then
  # Same scale as HEADLESS-1, another width: the shared bar node needs a
  # text layout per width. Hotplug well clear of a minute tick.
  [ "$(sec)" -ge 3 ] && [ "$(sec)" -lt 40 ] || sleep_until_sec 3
  swaymsg -q create_output
  swaymsg -q output HEADLESS-3 resolution 1920x1080 position 4608 0 scale 1
  for _ in $(seq 100); do
    damage_from $((TICKS_END + 1)) | grep -q 'buffer=1920x32 ' && break
    sleep 0.1
  done
  wait_quiet
  HOTPLUG=$(damage_from $((TICKS_END + 1)))
  HOTPLUG_END=$(frames)
  echo "hotplug frames:"
  echo "$HOTPLUG" | sed 's/^/  /'
  # The new bar's first paint is full; the others must not repaint.
  HOTPLUG_OTHER=$(echo "$HOTPLUG" | grep . | grep -vc 'buffer=1920x32 ' || true)
  # The next minute tick, on all three bars.
  sleep_until_sec 3
  lines=$(damage_from $((HOTPLUG_END + 1)))
  echo "tick on three bars:"
  echo "$lines" | sed 's/^/  /'
  T3_N=$(echo "$lines" | grep -c . || true)
  T3_MAX=$(echo "$lines" | areas | sort -n | tail -1)
  T3_AREA=$(echo "$lines" | areas | sum)
  T3_OFF=$(echo "$lines" | grep . | off_centre | grep -c . || true)
  grim -g "4608,0 1920x32" "$ROOT/docs/images/m0-bar-1920.png"
fi
DROPPED=$(grep -c '^strand: dropped' "$LOG" || true)

echo
echo "== results"
fail=0
echo "PSS after boot: $BOOT_PSS kB; after $TICKS tick(s): $PSS kB (gate $PSS_GATE_KB kB)"
[ "$PSS" -le "$PSS_GATE_KB" ] || { echo "  FAIL: PSS"; fail=1; }
for i in $(seq "$TICKS"); do
  echo "window $i: ${WINDOW_DELTA[$i]} context switches between ticks (gate 0)"
  [ "${WINDOW_DELTA[$i]}" -eq 0 ] || { echo "  FAIL: wakeups"; fail=1; }
  echo "tick $i: largest frame damage ${TICK_MAX[$i]:-none} px^2, all frames ${TICK_AREA[$i]:-0} px^2 (gate $DAMAGE_GATE per frame and per tick); off-centre frames: ${TICK_OFF[$i]}"
  [ -n "${TICK_MAX[$i]}" ] || { echo "  FAIL: no frame at the tick"; fail=1; }
  [ "${TICK_MAX[$i]:-0}" -le "$DAMAGE_GATE" ] || { echo "  FAIL: damage per frame"; fail=1; }
  [ "${TICK_AREA[$i]:-0}" -le "$DAMAGE_GATE" ] || { echo "  FAIL: damage per tick"; fail=1; }
  [ "${TICK_OFF[$i]}" -eq 0 ] || { echo "  FAIL: tick damage not centred (misaligned clock)"; fail=1; }
done
# Every frame after boot is a clock tick (nothing else changes), including
# a tick that fell before the first window.
POST=$(damage_from $((BOOT_FRAMES + 1)) | head -n $((TICKS_END - BOOT_FRAMES)) | areas)
POST_N=$(echo "$POST" | grep -c . || true)
POST_MAX=$(echo "$POST" | sort -n | tail -1)
echo "all $POST_N frames after boot: largest damage ${POST_MAX:-none} px^2 (gate $DAMAGE_GATE)"
[ "${POST_MAX:-0}" -le "$DAMAGE_GATE" ] || { echo "  FAIL: damage after boot"; fail=1; }
if [ "$THIRD" = 1 ]; then
  echo "hotplug of HEADLESS-3 (1920x1080 at 1.0): $HOTPLUG_OTHER frames on the other bars (gate 0)"
  [ "$HOTPLUG_OTHER" -eq 0 ] || { echo "  FAIL: hotplug repainted other bars"; fail=1; }
  echo "tick on three bars: $T3_N frames, largest ${T3_MAX:-none} px^2, all ${T3_AREA:-0} px^2 (gate $DAMAGE_GATE per frame and per tick); off-centre frames: $T3_OFF"
  [ "$T3_N" -ge 3 ] || { echo "  FAIL: a bar did not tick"; fail=1; }
  [ "${T3_MAX:-0}" -le "$DAMAGE_GATE" ] || { echo "  FAIL: damage per frame"; fail=1; }
  [ "${T3_AREA:-0}" -le "$DAMAGE_GATE" ] || { echo "  FAIL: damage per tick"; fail=1; }
  [ "$T3_OFF" -eq 0 ] || { echo "  FAIL: tick damage not centred (misaligned clock)"; fail=1; }
fi
echo "frames painted but not committed: $DROPPED"
[ "$DROPPED" -eq 0 ] || { echo "  FAIL: dropped frames"; fail=1; }
echo "pss by mapping: $OUT/pss_by_mapping.txt; screenshots: docs/images/m0-bar*.png"

kill "$STRAND_PID" 2>/dev/null || true
STRAND_PID=

if [ "$BENCH" = 1 ]; then
  echo
  echo "== strand-core 10k-node bench"
  (cd "$ROOT" && cargo bench -p strand-core --bench graph 2>&1) | tee "$OUT/bench.txt" |
    grep -E '^[a-z_/ 0-9]+$|time:|per node|allocations' || true
fi

exit "$fail"
