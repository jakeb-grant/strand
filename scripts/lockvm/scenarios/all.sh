#!/usr/bin/env bash
# Every S-lock scenario, run as root in the lock VM's guest:
#   scripts/container/run.sh bash scripts/lockvm/scenarios/build.sh
#   scripts/container/lockvm.sh bash scripts/lockvm/scenarios/all.sh
# Each scenario runs as the test user `tester` (never root) and logs to
# target/lockvm/<scenario>.log. Exits non-zero when any fails.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
failed=()
# strand_lock before pam: pam.sh removes /etc/pam.d/strand.
for s in session_lock strand_lock pam; do
  echo "=== scenario $s"
  if bash "$here/$s.sh"; then
    echo "=== $s PASSED"
  else
    echo "=== $s FAILED"
    failed+=("$s")
  fi
done
if [ "${#failed[@]}" -gt 0 ]; then
  echo "LOCK VM SCENARIOS FAILED: ${failed[*]}"
  exit 1
fi
echo "LOCK VM SCENARIOS PASSED"
