#!/usr/bin/env bash
# Builds what the lock VM scenarios run, on the HOST side: through
# scripts/container/run.sh on the laptop (its image is the guest's
# Ubuntu release, so the binaries run there), natively in CI's lock-vm
# job. Writes target/lockvm/bins.env, which all.sh reads in the guest:
#   SESSION_LOCK_TEST  strand-surface's tests/session_lock.rs
#   VM_PAM_TEST        strand-auth's tests/vm_pam.rs
#   HELPER             the strand-auth helper with the default features
#                      (the one that ships; release profile)
#   STRAND_LOCK_TEST   strand's tests/lock.rs (the binary's fault matrix),
#                      built with `--features faults`; the strand binary
#                      it runs and a strand-auth helper with its fault
#                      points sit beside each other in cargo's debug
#                      directory, where the binary looks for the helper
#
# Usage: scripts/container/run.sh bash scripts/lockvm/scenarios/build.sh
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd)
out=$root/target/lockvm
mkdir -p "$out"
cd "$root"

# The path of the test executable `name` from cargo's JSON messages.
test_exe() {
  local pkg=$1 name=$2 exe
  exe=$(cargo test -p "$pkg" --test "$name" --no-run --message-format=json \
    | grep -o '"executable":"[^"]*"' | cut -d'"' -f4 | grep "/$name-" | tail -1)
  [ -x "$exe" ] || { echo "build.sh: no executable for $pkg --test $name" >&2; exit 1; }
  echo "$exe"
}

session_lock=$(test_exe strand-surface session_lock)
vm_pam=$(test_exe strand-auth vm_pam)
strand_lock=$(cargo test -p strand --features faults --test lock --no-run --message-format=json \
  | grep -o '"executable":"[^"]*"' | cut -d'"' -f4 | grep "/lock-" | tail -1)
[ -x "$strand_lock" ] || { echo "build.sh: no executable for strand --test lock" >&2; exit 1; }
# The binary the test runs (CARGO_BIN_EXE_strand) and, beside it, the
# helper its lock spawns (strand_auth::default_helper).
cargo build -p strand --features faults --bin strand
cargo build -p strand-auth --features faults --bin strand-auth
debug=$(dirname "$(dirname "$strand_lock")")
for b in strand strand-auth; do
  [ -x "$debug/$b" ] || { echo "build.sh: no $debug/$b" >&2; exit 1; }
done
# Its own target directory: the test build above has the `faults`
# feature on (strand-auth's dev-dependency on itself).
cargo build -p strand-auth --bin strand-auth --release --target-dir "$out/helper"
helper=$out/helper/release/strand-auth
[ -x "$helper" ] || { echo "build.sh: no helper at $helper" >&2; exit 1; }

{
  printf 'SESSION_LOCK_TEST=%q\n' "$session_lock"
  printf 'VM_PAM_TEST=%q\n' "$vm_pam"
  printf 'HELPER=%q\n' "$helper"
  printf 'STRAND_LOCK_TEST=%q\n' "$strand_lock"
} >"$out/bins.env"
cat "$out/bins.env"
