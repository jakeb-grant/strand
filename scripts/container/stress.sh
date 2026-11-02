#!/usr/bin/env bash
# CI's nightly `stress` job, and the same run here: the tests over and over with
# no retry and the CPU deliberately short, through nextest's stress mode and its
# `stress` profile (.config/nextest.toml), to learn which tests fail, and how
# often, before a push does (decisions.md robustness).
#
# The time goes to blocks of `--stress-count 2`. Three in four run the tests the
# record says fail under load (RECORD below: Wayland, D-Bus, reloads, acceptance
# and the tests named in decisions.md's flake tracks), the fourth the whole
# workspace. Nextest has no shuffle: it always starts tests in the same order. So
# each block picks from the seed how many tests run at once, how many spinning
# processes run beside them, and a number of hash partitions taken in a shuffled
# order, which changes what runs beside what.
#
# Never here: `strand::budgets`, memory measured one shell at a time, and the
# reload fuzzer, whose saves are timed against the watcher and which has the
# nightly job's 10,000 edits alone. Wall-clock gates do run: a miss under this
# load says nothing, so it is listed apart and doesn't fail the run.
#
# Run: scripts/container/run.sh scripts/container/stress.sh [--list]
#      --list prints each tier's test count, checks the names, and runs nothing.
# Env: STRAND_STRESS_MINUTES (30)  the time for the blocks: none starts past it, or
#                                  when one of its tier as long as the last would
#                                  run past it
#      STRAND_STRESS_SEED          replays a run's choices (it prints its own)
#      STRAND_STRESS_TIER          record or whole: only that tier
# Out: target/stress/: summary.md, blocks.tsv, each block's log and junit files.
# Exit: 1 when a test failed (stress-summary.py names each, with its rate) or a
#       block didn't run.

set -uo pipefail
cd "$(dirname "$0")/../.."

MINUTES=${STRAND_STRESS_MINUTES:-30}
SEED=${STRAND_STRESS_SEED:-$(($(date +%s) % 32768))}
TIER=${STRAND_STRESS_TIER:-}
OUT=target/stress
CORES=$(nproc)

NEVER='binary_id(=strand::budgets) or test(random_edits_through_five_save_styles)'
WHOLE="not ($NEVER)"
# What talks to a compositor, a bus or the watcher, every `strand` test binary
# (they boot shells) and the compiler's reload tests.
RECORD_BINARIES='binary_id(/^strand(-surface|-services|-introspect|-watch)?(::|$)/) or binary_id(=strand-compiler::reload)'
# Tests in other binaries that decisions.md's laptop-flakes, m6-flakes and
# graph-flake record failing (some in a module, so a substring). One that is
# renamed fails `check_names`.
RECORD_TESTS=(
  first_frame_of_a_new_surface_has_its_text
  a_dropped_request_wanted_again_is_decoded
  a_dropped_stale_icon_wanted_again_is_decoded
  the_tick_a_history_starts_in_is_a_column_whenever_the_graph_first_draws
)
names=
for name in "${RECORD_TESTS[@]}"; do names+=" or test(~$name)"; done
RECORD="($RECORD_BINARIES$names) and not ($NEVER)"

list() { # FILTER: how many tests it selects
  cargo nextest list --workspace --profile stress -E "$1" --message-format json --cargo-quiet |
    jq '[."rust-suites"[].testcases[] | select(."filter-match".status == "matches")] | length'
}

check_names() {
  local name count bad=0
  for name in "${RECORD_TESTS[@]}"; do
    count=$(list "test(~$name)") || return 1
    if [ "$count" = 0 ]; then
      echo "stress: no test named $name: update RECORD_TESTS" >&2
      bad=1
    fi
  done
  return "$bad"
}

check_names || exit 1
if [ "${1:-}" = --list ]; then
  echo "record: $(list "$RECORD") tests"
  echo "whole:  $(list "$WHOLE") tests"
  exit 0
fi

RANDOM=$SEED
echo "stress: seed $SEED, $MINUTES minutes, $CORES cores"
rm -rf "$OUT"
mkdir -p "$OUT"
printf 'block\ttier\tjobs\tspinners\tpartitions\tseconds\tstatus\n' >"$OUT/blocks.tsv"

spinners=()
stop_spinners() {
  [ "${#spinners[@]}" = 0 ] || kill "${spinners[@]}" 2>/dev/null
  wait "${spinners[@]}" 2>/dev/null
  spinners=()
}
trap stop_spinners EXIT
start_spinners() {
  local i
  for ((i = 0; i < $1; i++)); do
    (while :; do :; done) &
    spinners+=("$!")
  done
}

JOBS=("$CORES" "$((CORES * 3 / 2))" "$((CORES * 2))")
LOADS=("$((CORES / 2))" "$CORES" "$((CORES * 2))")

start=$SECONDS
block=0
declare -A took=([record]=0 [whole]=0)
while :; do
  block=$((block + 1))
  case "$TIER" in
    record | whole) tier=$TIER ;;
    *) if [ "$((block % 4))" = 1 ]; then tier=whole; else tier=record; fi ;;
  esac
  [ "$((SECONDS - start + took[$tier]))" -lt "$((MINUTES * 60))" ] || break
  if [ "$tier" = whole ]; then filter=$WHOLE; else filter=$RECORD; fi
  jobs=${JOBS[RANDOM % 3]}
  load=${LOADS[RANDOM % 3]}
  parts=$((RANDOM % 3 + 1))
  order=()
  for ((i = 1; i <= parts; i++)); do order+=("$i"); done
  for ((i = parts - 1; i > 0; i--)); do
    j=$((RANDOM % (i + 1)))
    t=${order[i]}; order[i]=${order[j]}; order[j]=$t
  done

  echo
  echo "=== stress block $block: $tier, -j $jobs beside $load spinners, partitions ${order[*]} of $parts"
  started=$SECONDS
  status=0
  start_spinners "$load"
  for part in "${order[@]}"; do
    # The junit file of a run that never got to one must not read as this run's.
    rm -f target/nextest/stress/junit.xml
    cargo nextest run --workspace --profile stress -E "$filter" -j "$jobs" \
      --stress-count 2 --partition "hash:$part/$parts" --no-tests warn 2>&1 |
      tee -a "$OUT/block-$block.log" | grep -avE '^\s*(\S*PASS|\S*Stress test|─)' | cut -c1-400
    this=${PIPESTATUS[0]}
    [ ! -f target/nextest/stress/junit.xml ] ||
      mv target/nextest/stress/junit.xml "$OUT/block-$block-$part.xml"
    # 100 is a test failing, which the junit file holds; the rest is no run.
    case "$this" in 0 | 100) ;; *) status=$this ;; esac
  done
  stop_spinners
  took[$tier]=$((SECONDS - started))
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$block" "$tier" "$jobs" "$load" "$parts" "$((SECONDS - started))" "$status" >>"$OUT/blocks.tsv"
done

exec python3 -I scripts/container/stress-summary.py "$OUT"
