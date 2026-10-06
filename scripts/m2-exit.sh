#!/usr/bin/env bash
# M2 budget re-check (docs/m2-report.md): the M0 gates measured again on
# design.md's own bar instead of the M0 demo, and the full shell's memory
# with the launcher open against design.md's 59-64 MB estimate. Headless
# sway with two 2560x1440 outputs (HEADLESS-1 at scale 1.0, HEADLESS-2 at
# 1.25), as scripts/m0-exit.sh, running `strand run` on a copy of the
# fixtures (byte for byte design.md's code blocks) with the mock desktop
# (STRAND_MOCK=desktop: real clock, mock services until M3).
#
#   1. bar only (theme.strand + bar.strand):
#      PSS from /proc/<pid>/smaps_rollup                     gate <= 34 MB
#      context switches over every thread, :03 -> :57        gate 0
#      damage per clock tick, all outputs (STRAND_LOG=damage) gate <= 2000 px^2
#   1b. the midnight tick: the bar again with TZ set so that local
#      midnight falls on the first minute boundary at least 100 s away (an
#      ordinary tick first repairs the age-2 buffer's boot leftovers); the
#      00:00 tick (the day name changes width, so the centred clock moves
#      and repaints whole) and the 00:01 tick (HEADLESS-1's age-2 buffer
#      still holds the old day). Reported against M0's 2,000 px^2 and
#      against the documented midnight exception (decisions.md,
#      wave3-pixels exit fixer r3): each output within design.md's
#      "about 60x20 px" per tick, 1,200 x scale^2 px^2.
#   2. full shell (all five files), launcher opened with
#      `strand set launcher.open true`, two toasts up: PSS reported against
#      design.md's 59-64 MB estimate (not a gate in M2: M3 measures it with
#      real services); then the launcher closed, PSS again.
#   3. the same with HEADLESS-1 at scale 2, as design.md's estimate
#      budgets them ("launcher buffers at 2x"): PSS with the launcher open.
#
# The headless seat has no keyboard (WLR_LIBINPUT_NO_DEVICES=1), so the
# launcher's `keyboard: exclusive` never gives its input focus: the shots
# show it with no caret and no selected row (the acceptance tests attach a
# virtual keyboard; see crates/strand/tests/refs/acceptance/launcher_*.png).
#
# Usage: scripts/m2-exit.sh [--no-build] [--ticks N] [--images]
#   --images  also copy the shots into docs/images (m2-bar.png, m2-shell.png)
# Env:   STRAND_BIN (default target/release/strand), OUT (results dir, where
#        the shots always go).
# Exit status is non-zero when a gate fails.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BUILD=1
TICKS=2
IMAGES=0
while [ $# -gt 0 ]; do
  case "$1" in
    --no-build) BUILD=0 ;;
    --ticks) TICKS=$2; shift ;;
    --images) IMAGES=1 ;;
    -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

PSS_GATE_KB=$((34 * 1024))
DAMAGE_GATE=2000
BIN=${STRAND_BIN:-$ROOT/target/release/strand}
OUT=${OUT:-$ROOT/target/m2-exit}
FIX=$ROOT/crates/strand-compiler/tests/fixtures
mkdir -p "$OUT"

for tool in sway swaymsg grim awk date; do
  command -v "$tool" >/dev/null || { echo "$tool is not installed" >&2; exit 2; }
done
if [ "$BUILD" = 1 ]; then
  (cd "$ROOT" && cargo build --release -p strand)
fi
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 2; }

RUNDIR=$(mktemp -d /tmp/strand-m2.XXXXXX)
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

printf 'xwayland disable\noutput HEADLESS-1 resolution 2560x1440 position 0 0 scale 1\n' >"$RUNDIR/sway.cfg"
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
swaymsg -q output HEADLESS-2 resolution 2560x1440 position 2560 0 scale 1.25
swaymsg -q focus output HEADLESS-1

LOG=$OUT/strand.log
frames() { grep -c '^strand: damage' "$LOG" || true; }
damage_from() { grep '^strand: damage' "$LOG" | tail -n +"$1" || true; }
areas() { sed -n 's/.* area=\([0-9]*\).*/\1/p'; }
sum() { awk '{ s += $1 } END { print s + 0 }'; }
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
sec() { date +%-S; }
sleep_until_sec() {
  local target=$1 now t
  now=$(date +%s%N)
  t=$((now - now % 60000000000 + target * 1000000000))
  [ "$t" -gt "$now" ] || t=$((t + 60000000000))
  sleep "$(((t - now) / 1000000000)).$(printf '%09d' $(((t - now) % 1000000000)))"
}
# `strand run` on the named fixtures, in a fresh home outside /tmp: the
# config watcher's light watches on its ancestors would wake for other
# processes' directories coming and going in /tmp (a desktop's ~/.config
# has no such neighbours).
start() {
  local home=$OUT/home-$1 f
  rm -rf "$home"
  shift
  mkdir -p "$home/.config/strand"
  for f in "$@"; do cp "$FIX/$f" "$home/.config/strand/$f"; done
  HOME=$home XDG_CACHE_HOME=$home/cache XDG_STATE_HOME=$home/state \
    STRAND_MOCK=desktop STRAND_LOG=damage DBUS_SESSION_BUS_ADDRESS= \
    "$BIN" run "$home/.config/strand" 2>"$LOG" &
  STRAND_PID=$!
  for _ in $(seq 150); do
    [ "$(frames)" -ge 2 ] && break
    kill -0 "$STRAND_PID" 2>/dev/null || { cat "$LOG" >&2; exit 1; }
    sleep 0.1
  done
  [ "$(frames)" -ge 2 ] || { echo "bars did not paint" >&2; cat "$LOG" >&2; exit 1; }
  wait_quiet
}
stop() {
  kill "$STRAND_PID" 2>/dev/null || true
  wait "$STRAND_PID" 2>/dev/null || true
  STRAND_PID=
}

fail=0
echo "== 1. design.md's bar on two 2560x1440 outputs (1.0, 1.25)"
start bar theme.strand bar.strand
BOOT_FRAMES=$(frames)
BOOT_PSS=$(pss_kb)
echo "boot frames: $BOOT_FRAMES; PSS after boot: $BOOT_PSS kB"
for i in $(seq "$TICKS"); do
  [ "$(sec)" -ge 3 ] && [ "$(sec)" -lt 10 ] || sleep_until_sec 3
  before=$(frames)
  s0=$(switches)
  sleep_until_sec 57
  s1=$(switches)
  delta=$((s1 - s0))
  sleep_until_sec 3
  lines=$(damage_from $((before + 1)))
  n=$(echo "$lines" | grep -c . || true)
  max=$(echo "$lines" | areas | sort -n | tail -1)
  area=$(echo "$lines" | areas | sum)
  echo "window $i (:03 -> :57): $delta context switches (gate 0)"
  echo "tick $i: $n frames, largest ${max:-none} px^2, all ${area} px^2 (gate $DAMAGE_GATE per tick)"
  echo "$lines" | sed 's/^/  /'
  [ "$delta" -eq 0 ] || { echo "  FAIL: wakeups between ticks"; fail=1; }
  [ "$n" -ge 2 ] || { echo "  FAIL: a bar did not tick"; fail=1; }
  [ "$area" -le "$DAMAGE_GATE" ] || { echo "  FAIL: damage per tick"; fail=1; }
done
PSS=$(pss_kb)
echo "PSS after $TICKS tick(s): $PSS kB (gate $PSS_GATE_KB kB)"
[ "$PSS" -le "$PSS_GATE_KB" ] || { echo "  FAIL: PSS"; fail=1; }
grep -E '^(Rss|Pss|Pss_Anon|Pss_File|Pss_Shmem):' "/proc/$STRAND_PID/smaps_rollup" >"$OUT/bar_smaps_rollup.txt"
grim -g "0,0 2560x64" "$OUT/m2-bar.png"
stop

echo
echo "== 1b. the midnight tick (TZ moved so local midnight is two to three minutes away)"
: >"$LOG"
now=$(date -u +%s)
u=$((now % 86400))
m=$(((u + 100 + 59) / 60 * 60))
MIDNIGHT=$((now - u + m))
off=$(((86400 - m % 86400) % 86400))
# POSIX TZ: the sign is west of UTC, so local = UTC + off is "-off".
if [ "$off" -gt 43200 ]; then
  off=$((off - 86400))
fi
if [ "$off" -ge 0 ]; then sign=-; a=$off; else sign=+; a=$((-off)); fi
MIDNIGHT_TZ=$(printf 'MID%s%d:%02d' "$sign" $((a / 3600)) $((a % 3600 / 60)))
echo "TZ=$MIDNIGHT_TZ: local now $(TZ=$MIDNIGHT_TZ date +%H:%M:%S)"
export TZ=$MIDNIGHT_TZ
start midnight theme.strand bar.strand
unset TZ
sleep_until_epoch() {
  local now
  now=$(date +%s%N)
  [ "$1"000000000 -gt "$now" ] || return 0
  sleep "$((($1 * 1000000000 - now) / 1000000000)).$(printf '%09d' $((($1 * 1000000000 - now) % 1000000000)))"
}
per_output() {
  awk '{ for (i = 1; i <= NF; i++) { if ($i ~ /^scale=/) s = substr($i, 7); if ($i ~ /^area=/) a = substr($i, 6) }
         t[s] += a }
       END { for (s in t) printf "%s %d\n", s, t[s] }'
}
for k in 0 1; do
  label=00:0$k
  sleep_until_epoch $((MIDNIGHT + 60 * k - 3))
  before=$(frames)
  sleep_until_epoch $((MIDNIGHT + 60 * k + 4))
  lines=$(damage_from $((before + 1)))
  area=$(echo "$lines" | areas | sum)
  echo "tick $label (local $(TZ=$MIDNIGHT_TZ date +%a\ %H:%M)): all $area px^2 (M0 gate $DAMAGE_GATE)"
  echo "$lines" | sed 's/^/  /'
  while read -r sc a; do
    [ -n "$sc" ] || continue
    lim=$(awk -v s="$sc" 'BEGIN { printf "%d", 1200 * s * s }')
    echo "  scale $sc: $a px^2 (midnight exception: <= $lim per output)"
    [ "$a" -le "$lim" ] || { echo "  FAIL: midnight tick over 60x20 px at scale $sc"; fail=1; }
  done < <(echo "$lines" | per_output)
  [ "$(echo "$lines" | grep -c .)" -ge 2 ] || { echo "  FAIL: a bar did not tick"; fail=1; }
done
stop

echo
echo "== 2. the full shell: bar, launcher (open), toasts (two), OSD, theme"
: >"$LOG"
start full theme.strand bar.strand launcher.strand toasts.strand osd.strand
SHELL_PSS=$(pss_kb)
surfaces() { grep '^strand: damage' "$LOG" | grep -o 'surface=[0-9]*' | sort -u | wc -l; }
before=$(surfaces)
"$BIN" set launcher.open true
for _ in $(seq 50); do
  [ "$(surfaces)" -gt "$before" ] && break
  sleep 0.1
done
[ "$(surfaces)" -gt "$before" ] || { echo "  FAIL: the launcher did not open"; fail=1; }
wait_quiet
OPEN_PSS=$(pss_kb)
grep -E '^(Rss|Pss|Pss_Anon|Pss_File|Pss_Shmem):' "/proc/$STRAND_PID/smaps_rollup" >"$OUT/full_smaps_rollup.txt"
awk '/^[0-9a-f]+-[0-9a-f]+ / { name = $6; if (name == "") name = "[anon]" }
     /^Pss:/ { pss[name] += $2 }
     END { for (n in pss) printf "%8d kB  %s\n", pss[n], n }' "/proc/$STRAND_PID/smaps" |
  sort -rn | head -25 >"$OUT/full_pss_by_mapping.txt"
grim -g "0,0 2560x1440" "$OUT/m2-shell.png"
"$BIN" set launcher.open false
wait_quiet
sleep 1
CLOSED_PSS=$(pss_kb)
echo "PSS, every surface but the launcher: $SHELL_PSS kB"
echo "PSS, launcher open on HEADLESS-1 at 1.0: $OPEN_PSS kB (design.md estimate 59-64 MB = $((59 * 1024))-$((64 * 1024)) kB, with the launcher at 2x)"
echo "PSS, launcher closed again: $CLOSED_PSS kB"
stop

echo
echo "== 3. the full shell with HEADLESS-1 at scale 2 (the launcher's buffers at 2x)"
swaymsg -q output HEADLESS-1 scale 2
: >"$LOG"
start full2 theme.strand bar.strand launcher.strand toasts.strand osd.strand
SHELL2_PSS=$(pss_kb)
before=$(surfaces)
"$BIN" set launcher.open true
for _ in $(seq 50); do
  [ "$(surfaces)" -gt "$before" ] && break
  sleep 0.1
done
[ "$(surfaces)" -gt "$before" ] || { echo "  FAIL: the launcher did not open"; fail=1; }
wait_quiet
OPEN2_PSS=$(pss_kb)
grep -E '^(Rss|Pss|Pss_Anon|Pss_File|Pss_Shmem):' "/proc/$STRAND_PID/smaps_rollup" >"$OUT/full2x_smaps_rollup.txt"
grep '^strand: damage' "$LOG" | grep -o 'buffer=[0-9x]* scale=[0-9.]*' | sort -u >"$OUT/full2x_buffers.txt"
echo "PSS at 2x, every surface but the launcher: $SHELL2_PSS kB"
echo "PSS at 2x, launcher open: $OPEN2_PSS kB (design.md estimate 59-64 MB)"
stop

if [ "$IMAGES" = 1 ]; then
  mkdir -p "$ROOT/docs/images"
  cp "$OUT/m2-bar.png" "$OUT/m2-shell.png" "$ROOT/docs/images/"
fi

exit "$fail"
