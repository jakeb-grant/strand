#!/usr/bin/env bash
# (services-round) Strand as greetd's greeter, against the real greetd
# and its PAM service (strand-services' tests/vm_auth.rs,
# `greetd_greeter`): greetd (no VT: `vt = "none"`) runs the test as its
# greeter, as `tester`; the greeter logs `tester` in with a wrong
# password, then `strand-test`, and starts a session that writes its
# user's name to a file. The scenario waits for that file, ends greetd
# (which would start its greeter again) and checks the greeter's report.
set -uo pipefail
. "$(dirname "$0")/common.sh"
log=$out/greetd.log
: >"$log"
result=/tmp/strand-greetd-result
session=/tmp/strand-greetd-session
rm -f "$result" "$session"
mkdir -p /etc/greetd
cat >/etc/greetd/config.toml <<TOML
[terminal]
vt = "none"

[default_session]
command = "env STRAND_LOCK_VM=1 STRAND_LOCK_VM_GREETD=$result RUST_BACKTRACE=1 $VM_AUTH_TEST greetd_greeter --exact --test-threads=1 --nocapture"
user = "tester"
TOML
greetd --config /etc/greetd/config.toml >>"$log" 2>&1 &
greetd=$!
for _ in $(seq 600); do
  [ -s "$session" ] && break
  kill -0 "$greetd" 2>/dev/null || break
  sleep 0.1
done
kill "$greetd" 2>/dev/null
sleep 0.5
pkill -f "$VM_AUTH_TEST" 2>/dev/null
cat "$log"
echo "--- the greeter said: $(cat "$result" 2>/dev/null)"
echo "--- the session ran as: $(cat "$session" 2>/dev/null)"
[ "$(cat "$result" 2>/dev/null)" = ok ] || exit 1
[ "$(cat "$session" 2>/dev/null)" = tester ] || exit 1
if grep -q 'running 0 tests' "$log"; then
  echo "the greeter filter matched no test"
  exit 1
fi
exit 0
