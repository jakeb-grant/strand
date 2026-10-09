#!/usr/bin/env bash
# Builds what the lock VM scenarios run, on the HOST side: through
# scripts/container/run.sh on the laptop (its image is the guest's
# Ubuntu release, so the binaries run there), natively in CI's lock-vm
# job. Writes target/lockvm/bins.env, which all.sh reads in the guest:
#   SESSION_LOCK_TEST  strand-surface's tests/session_lock.rs
#   VM_PAM_TEST        strand-auth's tests/vm_pam.rs
#   HELPER             the strand-auth helper with the default features
#                      (the one that ships; release profile)
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
# Its own target directory: the test build above has the `faults`
# feature on (strand-auth's dev-dependency on itself).
cargo build -p strand-auth --bin strand-auth --release --target-dir "$out/helper"
helper=$out/helper/release/strand-auth
[ -x "$helper" ] || { echo "build.sh: no helper at $helper" >&2; exit 1; }

{
  printf 'SESSION_LOCK_TEST=%q\n' "$session_lock"
  printf 'VM_PAM_TEST=%q\n' "$vm_pam"
  printf 'HELPER=%q\n' "$helper"
} >"$out/bins.env"
cat "$out/bins.env"
