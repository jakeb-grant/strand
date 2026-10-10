#!/usr/bin/env bash
# The strand binary's lock fault matrix (strand's tests/lock.rs, built
# with `--features faults`) on headless sway 1.9 with real PAM: logic
# panic and stall, text worker death, the PAM helper crashing, hanging,
# answering garbage or missing, a runtime fault in the lock, no first
# frame, SIGTERM, SIGKILL and SIGABRT with a restart, `finished` after
# `locked` (through a Wayland proxy), a refused lock and hotplug. Each
# asserts the session stayed locked, the built-in password field showed
# and only the right password unlocked. One test at a time: each starts
# its own sway.
set -uo pipefail
. "$(dirname "$0")/common.sh"
as_tester "$STRAND_LOCK_TEST" --test-threads=1 --nocapture 2>&1 | tee "$out/strand_lock.log"
status=${PIPESTATUS[0]}
# A test that skipped did not run: the scenario fails if any did.
if grep -q 'skipping ' "$out/strand_lock.log"; then
  echo "a strand lock test skipped inside the VM"
  exit 1
fi
exit "$status"
