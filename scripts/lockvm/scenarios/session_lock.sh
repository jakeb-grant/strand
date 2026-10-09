#!/usr/bin/env bash
# The session lock on headless sway 1.9 (strand-surface's
# tests/session_lock.rs): every output locked, hotplug, a refused lock
# reported, only a token unlocking whatever the shell does, keys reaching
# the lock. One test at a time: each starts its own sway.
set -uo pipefail
. "$(dirname "$0")/common.sh"
as_tester "$SESSION_LOCK_TEST" --test-threads=1 --nocapture 2>&1 | tee "$out/session_lock.log"
status=${PIPESTATUS[0]}
# A test that skipped did not run: the scenario fails if any did.
if grep -q 'skipping ' "$out/session_lock.log"; then
  echo "a session-lock test skipped inside the VM"
  exit 1
fi
exit "$status"
