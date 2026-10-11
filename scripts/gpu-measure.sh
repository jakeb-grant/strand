#!/usr/bin/env bash
# owner-run only; never run by an agent.
#
# The promoted GPU cost on hardware (features.md M5, "The promoted GPU
# cost on hardware", branch (B) of decisions.md m5-owner Q19): the
# owner runs this in their own session, on a compositor driving a KMS
# card, on their own machine. Agents never run it: it starts a client on
# the live session, which CLAUDE.md's laptop rules forbid to agents
# (decisions.md m5-infra). Branch (A), `scripts/container/gpu.sh` on the
# render node, is what agents use.
#
# What it does: `strand run` on a private config whose panel `Glow`
# (640x420, top-left) animates a 640x320 shader, so render promotes it
# to the GPU after its 500 ms window. It records PSS before the GPU,
# while promoted (five samples a second apart), and after the device
# drops; the attach mode (Present or Readback) from strand's log; and
# `strand report --json` while promoted when this strand has `report`
# (frame times; M5's S-tools stream adds it). Nothing outside a
# temporary directory and OUTDIR is written: its own socket, config,
# state and cache directories, and the mock services
# (STRAND_MOCK=acceptance), so the owner's shell and settings are not
# touched. The panel shows on the session for about 20 s.
#
# Usage: STRAND_OWNER_GPU_MEASURE=1 scripts/gpu-measure.sh [STRAND_BIN [OUTDIR]]
#        STRAND_BIN defaults to target/release/strand (build it with
#        `cargo build --release -p strand`; a debug binary pages in
#        several times more code and measures nothing useful).
#        OUTDIR defaults to target/gpu-measure-<date>; summary.txt there
#        is what goes into decisions.md and the features.md box.

set -euo pipefail

if [ "${STRAND_OWNER_GPU_MEASURE:-}" != 1 ]; then
  echo "gpu-measure.sh is owner-run only; never run by an agent." >&2
  echo "It starts a client on the live Wayland session. The owner runs it with" >&2
  echo "STRAND_OWNER_GPU_MEASURE=1 on a compositor driving a KMS card." >&2
  exit 2
fi

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${1:-$ROOT/target/release/strand}
OUT=${2:-$ROOT/target/gpu-measure-$(date +%Y%m%d-%H%M%S)}
[ -x "$BIN" ] || { echo "no strand binary at $BIN (cargo build --release -p strand)" >&2; exit 2; }
[ -n "${WAYLAND_DISPLAY:-}" ] || { echo "WAYLAND_DISPLAY is not set: run it inside your session" >&2; exit 2; }
mkdir -p "$OUT"

TMP=$(mktemp -d /tmp/strand-gpu-measure.XXXXXX)
STRAND_PID=
cleanup() {
  [ -n "$STRAND_PID" ] && kill "$STRAND_PID" 2>/dev/null
  wait 2>/dev/null || true
  cp "$TMP/strand.log" "$OUT/strand.log" 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

mkdir -p "$TMP/config" "$TMP/state" "$TMP/cache" "$TMP/xdg-config"
cat >"$TMP/config/glow.strand" <<'EOF'
export state on = false

panel Glow { anchor: top_left; width: 640; height: 420; open: true
  col {
    box { width: 640; height: 320
      if on { shader "aurora.wgsl" { width: 640; height: 320; u_speed: 1 } }
    }
    row { width: 640; height: 100; pad: 10; gap: 10; bg: $surface
      text "Strand GPU measurement (scripts/gpu-measure.sh)" { color: $fg }
    }
  }
}
EOF
cat >"$TMP/config/aurora.wgsl" <<'EOF'
@group(1) @binding(0) var<uniform> u_speed: f32;
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let t = fract(strand.time * u_speed + v.uv.x);
    return vec4<f32>(t * 0.5, 0.2, 0.6 * (1.0 - t), 1.0);
}
EOF

SOCK=$TMP/strand.sock
s() { STRAND_SOCKET=$SOCK "$BIN" "$@"; }
pss() { awk '/^Pss:/ {print $2}' "/proc/$STRAND_PID/smaps_rollup"; }
log() { cat "$TMP/strand.log" 2>/dev/null || true; }
wait_for() { # SECONDS DESCRIPTION CMD...
  local limit=$1 what=$2 i
  shift 2
  for i in $(seq $((limit * 10))); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "timed out after ${limit}s waiting for $what; strand's log:" >&2
  log | tail -30 >&2
  exit 1
}

{
  echo "strand GPU measurement, $(date -Iseconds)"
  echo "binary: $BIN ($(stat -c %s "$BIN") bytes)"
  echo "kernel: $(uname -r)"
  echo "session: XDG_CURRENT_DESKTOP=${XDG_CURRENT_DESKTOP:-?} WAYLAND_DISPLAY=$WAYLAND_DISPLAY"
  for d in /sys/class/drm/card[0-9]; do
    [ -e "$d/device/vendor" ] && echo "card: ${d##*/} $(cat "$d/device/vendor"):$(cat "$d/device/device")"
  done
  command -v vulkaninfo >/dev/null && vulkaninfo --summary 2>/dev/null | grep -E 'deviceName|driverName|driverInfo' | sed 's/^ */vulkan: /'
} | tee "$OUT/summary.txt"

HOME=$TMP XDG_CONFIG_HOME=$TMP/xdg-config XDG_STATE_HOME=$TMP/state XDG_CACHE_HOME=$TMP/cache \
  STRAND_MOCK=acceptance STRAND_SOCKET=$SOCK STRAND_LOG=info STRAND_GPU_IDLE_MS=1500 \
  "$BIN" run "$TMP/config" </dev/null 2>"$TMP/strand.log" &
STRAND_PID=$!

wait_for 30 "the shell's socket" s set glow.on false
sleep 2
base=$(pss)
echo "PSS before the GPU: $base kB" | tee -a "$OUT/summary.txt"

attached=$(log | grep -c 'attached (' || true)
s set glow.on true
promoted() { [ "$(log | grep -c 'attached (')" -gt "$attached" ]; }
wait_for 30 "the panel's promotion" promoted
mode=$(log | grep -o 'attached ([A-Za-z]*' | tail -1 | sed 's/attached (//')
echo "attached for: $mode (Present: the compositor took the GPU's buffers; Readback: it did not)" | tee -a "$OUT/summary.txt"
sleep 5
samples=()
for _ in 1 2 3 4 5; do
  samples+=("$(pss)")
  sleep 1
done
echo "PSS while promoted: ${samples[*]} kB (each minus the baseline: $(for p in "${samples[@]}"; do printf '%s ' $((p - base)); done)kB)" | tee -a "$OUT/summary.txt"

if s report --json >"$OUT/report.json" 2>"$OUT/report.err"; then
  echo "strand report --json while promoted: $OUT/report.json" | tee -a "$OUT/summary.txt"
else
  echo "strand report: not in this strand ($(head -1 "$OUT/report.err")); frame times not recorded" | tee -a "$OUT/summary.txt"
  rm -f "$OUT/report.json"
fi

s set glow.on false
dropped() { log | grep -q 'GPU: the thread ended'; }
wait_for 30 "the device to drop" dropped
sleep 2
after=$(pss)
echo "PSS after the device dropped: $after kB ($((after - base)) kB over the baseline)" | tee -a "$OUT/summary.txt"
echo "written: $OUT/summary.txt"
