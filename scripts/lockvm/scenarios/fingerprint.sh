#!/usr/bin/env bash
# A fingerprint module ahead of pam_unix (m6-lock): /etc/pam.d/strand
# becomes a stack shaped like pam_fprintd's (pam_echo says "Place your
# finger on the fingerprint reader", as pam_fprintd does, and a
# `sufficient` pam_exec waits 5 s for the finger, a file the tests touch,
# as pam_fprintd waits for the reader), then the image's common-auth.
# Runs the shipping helper's conversation test (strand-auth's
# tests/vm_pam.rs, `pam_fingerprint_then_password`) and the strand
# binary's fingerprint tests (strand's tests/lock.rs, `fingerprint_*`:
# the lock shows the text and the prompt, a finger unlocks, a password
# typed while the reader waits is tried after it, and the fallback's
# field works through the same stack), then puts the image's stack back.
set -uo pipefail
. "$(dirname "$0")/common.sh"
fp=/run/strand-finger
lib=/usr/local/lib/strand-finger
saved=$(mktemp)
cp /etc/pam.d/strand "$saved"
rm -rf "$fp" "$lib"
install -d -o tester -g tester -m 0700 "$fp"
install -d -m 0755 "$lib"
printf 'Place your finger on the fingerprint reader\n' >"$lib/say"
# Waits up to 5 s for the finger and takes it (removed, so the next
# conversation waits again); pam_exec gives it no PATH.
cat >"$lib/reader" <<SH
#!/bin/sh
PATH=/usr/bin:/bin
i=0
while [ ! -e $fp/finger ]; do
  [ \$i -lt 100 ] || exit 1
  sleep 0.05
  i=\$((i + 1))
done
rm -f $fp/finger
SH
chmod 0755 "$lib/reader"
cat >/etc/pam.d/strand <<PAM
# strand lock screen with a fingerprint-shaped module (scripts/lockvm/scenarios/fingerprint.sh)
auth    optional    pam_echo.so file=$lib/say
auth    sufficient  pam_exec.so quiet $lib/reader
@include common-auth
@include common-account
PAM
status=0
: >"$out/fingerprint.log"
as_tester env STRAND_LOCK_VM_HELPER="$HELPER" STRAND_LOCK_VM_SERVICE=fingerprint \
  STRAND_LOCK_VM_FINGER="$fp/finger" \
  "$VM_PAM_TEST" pam_fingerprint_then_password --nocapture 2>&1 | tee -a "$out/fingerprint.log"
[ "${PIPESTATUS[0]}" -eq 0 ] || status=1
as_tester env STRAND_LOCK_VM_FINGER="$fp/finger" \
  "$STRAND_LOCK_TEST" fingerprint --test-threads=1 --nocapture 2>&1 | tee -a "$out/fingerprint.log"
[ "${PIPESTATUS[0]}" -eq 0 ] || status=1
cp "$saved" /etc/pam.d/strand
rm -rf "$fp" "$lib" "$saved"
if grep -q 'skipping ' "$out/fingerprint.log"; then
  echo "a fingerprint test skipped inside the VM"
  exit 1
fi
# Both test binaries ran their tests (a filter that matched nothing
# passes with "running 0 tests").
if grep -q 'running 0 tests' "$out/fingerprint.log"; then
  echo "a fingerprint filter matched no test"
  exit 1
fi
exit "$status"
