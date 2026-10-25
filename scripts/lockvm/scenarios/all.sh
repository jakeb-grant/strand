#!/usr/bin/env bash
# Every S-lock (and services-round authentication) scenario, run as root in the lock VM's guest:
#   scripts/container/run.sh bash scripts/lockvm/scenarios/build.sh
#   scripts/container/lockvm.sh bash scripts/lockvm/scenarios/all.sh
# Each scenario runs as the test user `tester` (never root) and logs to
# target/lockvm/<scenario>.log. Exits non-zero when any fails.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
failed=()
# pam.sh last: it removes /etc/pam.d/strand (faillock.sh and
# fingerprint.sh put their own stacks there and the image's back).
# (services-round) polkit.sh and greetd.sh: Strand's polkit agent and
# greeter against the real polkitd and greetd.
for s in session_lock strand_lock faillock fingerprint polkit greetd pam; do
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
