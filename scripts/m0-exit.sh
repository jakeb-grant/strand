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
#
# Also saves grim screenshots of both bars to docs/images/ and runs the
# strand-core 10k-node benchmark (skip with --no-bench).
#
# Usage: scripts/m0-exit.sh [--no-bench] [--no-build] [--ticks N]
# Env:   STRAND_BIN (default target/release/strand), OUT (results dir).
# Exit status is non-zero when a gate fails.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BENCH=1
BUILD=1
TICKS=2
while [ $# -gt 0 ]; do
  case "$1" in
    --no-bench) BENCH=0 ;;
    --no-build) BUILD=0 ;;
    --ticks) TICKS=$2; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
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

for tool in sway swaymsg grim; do
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
# Both bars painted.
for _ in $(seq 100); do
  [ "$(frames)" -ge 2 ] && break
  kill -0 "$STRAND_PID" 2>/dev/null || { cat "$LOG" >&2; exit 1; }
  sleep 0.1
done
[ "$(frames)" -ge 2 ] || { echo "bars did not paint" >&2; cat "$LOG" >&2; exit 1; }
sleep 1

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
  local target=$1 now
  now=$(date +%s.%N)
  python3 -c "import sys,time; n=float('$now'); m=n-n%60; t=m+$target; t=t if t>n else t+60; time.sleep(t-n)"
}

echo "outputs: $(tr '\n' ';' <"$OUT/outputs.txt")"
BOOT_FRAMES=$(frames)
echo "boot frames: $BOOT_FRAMES"
BOOT_PSS=$(pss_kb)

declare -a WINDOW_DELTA TICK_MAX TICK_AREA
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
  lines=$(tail -n +$((before_frames + 1)) "$LOG" | grep '^strand: damage' || true)
  echo "tick $i frames:"
  echo "$lines" | sed 's/^/  /'
  TICK_MAX[$i]=$(echo "$lines" | sed -n 's/.* area=\([0-9]*\).*/\1/p' | sort -n | tail -1)
  TICK_AREA[$i]=$(echo "$lines" | sed -n 's/.* area=\([0-9]*\).*/\1/p' | paste -sd+ | bc)
done
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

echo
echo "== results"
fail=0
echo "PSS after boot: $BOOT_PSS kB; after $TICKS tick(s): $PSS kB (gate $PSS_GATE_KB kB)"
[ "$PSS" -le "$PSS_GATE_KB" ] || { echo "  FAIL: PSS"; fail=1; }
for i in $(seq "$TICKS"); do
  echo "window $i: ${WINDOW_DELTA[$i]} context switches between ticks (gate 0)"
  [ "${WINDOW_DELTA[$i]}" -eq 0 ] || { echo "  FAIL: wakeups"; fail=1; }
  echo "tick $i: largest frame damage ${TICK_MAX[$i]:-none} px^2, all frames ${TICK_AREA[$i]:-0} px^2 (gate $DAMAGE_GATE per frame)"
  [ -n "${TICK_MAX[$i]}" ] || { echo "  FAIL: no frame at the tick"; fail=1; }
  [ "${TICK_MAX[$i]:-0}" -le "$DAMAGE_GATE" ] || { echo "  FAIL: damage"; fail=1; }
done
# Every frame after boot is a clock tick (nothing else changes), including
# a tick that fell before the first window.
POST=$(tail -n +$((BOOT_FRAMES + 1)) "$LOG" | sed -n 's/^strand: damage .* area=\([0-9]*\).*/\1/p')
POST_N=$(echo "$POST" | grep -c . || true)
POST_MAX=$(echo "$POST" | sort -n | tail -1)
echo "all $POST_N frames after boot: largest damage ${POST_MAX:-none} px^2 (gate $DAMAGE_GATE)"
[ "${POST_MAX:-0}" -le "$DAMAGE_GATE" ] || { echo "  FAIL: damage after boot"; fail=1; }
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
