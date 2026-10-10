#!/usr/bin/env bash
# The shipping helper against `pam_faillock` (strand-auth's
# tests/vm_pam.rs, `pam_faillock_locks_out_after_three_wrong_passwords`):
# /etc/pam.d/strand becomes a stack that locks the user out after three
# failures (its tally in a directory the test user may write, since the
# helper runs as that user), then the image's stack is put back.
set -uo pipefail
. "$(dirname "$0")/common.sh"
tally=/run/strand-faillock
saved=$(mktemp)
cp /etc/pam.d/strand "$saved"
rm -rf "$tally"
install -d -o tester -g tester -m 0700 "$tally"
cat >/etc/pam.d/strand <<PAM
# strand lock screen with pam_faillock (scripts/lockvm/scenarios/faillock.sh)
auth    requisite                   pam_faillock.so preauth deny=3 unlock_time=600 dir=$tally
auth    [success=1 default=ignore]  pam_unix.so
auth    [default=die]               pam_faillock.so authfail deny=3 unlock_time=600 dir=$tally
auth    sufficient                  pam_faillock.so authsucc deny=3 unlock_time=600 dir=$tally
auth    requisite                   pam_deny.so
@include common-account
PAM
as_tester env STRAND_LOCK_VM_HELPER="$HELPER" STRAND_LOCK_VM_SERVICE=faillock \
  "$VM_PAM_TEST" pam_faillock_locks_out_after_three_wrong_passwords --nocapture 2>&1 \
  | tee "$out/faillock.log"
status=${PIPESTATUS[0]}
cp "$saved" /etc/pam.d/strand
rm -rf "$tally" "$saved"
if grep -q 'skipping ' "$out/faillock.log"; then
  echo "the faillock test skipped inside the VM"
  exit 1
fi
exit "$status"
