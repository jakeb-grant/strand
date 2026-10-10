#!/usr/bin/env bash
# The shipping helper against real pam_unix (strand-auth's
# tests/vm_pam.rs): right, wrong, empty and near-miss passwords through
# /etc/pam.d/strand, then with that file gone, through `login` with the
# one-time warning.
set -uo pipefail
. "$(dirname "$0")/common.sh"
run() {
  as_tester env STRAND_LOCK_VM_HELPER="$HELPER" STRAND_LOCK_VM_SERVICE="$1" \
    "$VM_PAM_TEST" pam_unix_takes_only_the_right_password --nocapture 2>&1 | tee -a "$out/pam.log"
  local status=${PIPESTATUS[0]}
  if grep -q 'skipping ' "$out/pam.log"; then
    echo "the PAM test skipped inside the VM"
    return 1
  fi
  return "$status"
}
: >"$out/pam.log"
echo "--- the strand service"
run strand || exit 1
echo "--- /etc/pam.d/strand removed: the login fallback"
rm -f /etc/pam.d/strand /usr/lib/pam.d/strand
run login || exit 1
