#!/usr/bin/env bash
# The lock VM harness's smoke (scripts/container/lockvm.sh with no
# command), run as root in the guest: the test user logs in, sway runs
# headless for it, and pam_unix (through the setuid unix_chkpwd, as the
# lock screen will) accepts its password and refuses a wrong one.
set -uo pipefail
fail() { echo "SMOKE FAILED: $*"; exit 1; }
as_tester() { su -l tester -c "$1"; }

echo "--- login as the test user"
as_tester 'id; echo "home $HOME, shell $SHELL"' || fail "su -l tester"

echo "--- sway --version"
as_tester 'sway --version' || fail "sway --version"

echo "--- headless sway for the test user"
as_tester 'export XDG_RUNTIME_DIR=/run/user/1000 WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
  sway -c /dev/null >/tmp/sway.log 2>&1 & pid=$!
  for i in $(seq 100); do
    for s in "$XDG_RUNTIME_DIR"/sway-ipc.*.sock; do [ -S "$s" ] && export SWAYSOCK=$s; done
    [ -n "${SWAYSOCK:-}" ] && timeout 5 swaymsg -t get_version >/dev/null 2>&1 && break
    kill -0 $pid 2>/dev/null || break
    sleep 0.1
  done
  ok=1
  if [ -n "${SWAYSOCK:-}" ] && timeout 5 swaymsg -t get_outputs | grep -m1 -o "\"name\": \"HEADLESS-[0-9]*\""; then ok=0; fi
  [ $ok = 0 ] || { echo "sway did not answer; its log:"; tail -20 /tmp/sway.log; }
  kill $pid 2>/dev/null; timeout 5 tail --pid=$pid -f /dev/null; exit $ok' || fail "headless sway"

echo "--- PAM: the strand service as the test user (pam_unix via unix_chkpwd)"
ls -l /usr/sbin/unix_chkpwd /etc/pam.d/strand
as_tester 'echo strand-test | pamtester -v strand tester authenticate' || fail "the right password was refused"
if as_tester 'echo wrong-password | pamtester -v strand tester authenticate'; then
  fail "a wrong password was accepted"
fi
echo "(the wrong password was refused, as it must be)"
echo "SMOKE PASSED"
