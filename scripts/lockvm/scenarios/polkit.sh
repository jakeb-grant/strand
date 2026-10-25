#!/usr/bin/env bash
# (services-round) Strand's polkit agent against the real polkitd, its
# setuid polkit-agent-helper-1 and pam_unix (strand-services'
# tests/vm_auth.rs, `polkit_agent_answers_polkitd`). The guest has no
# systemd, so this lays out what logind would: cgroup2 with `tester`'s
# session scope (session `1`, which the test joins) and logind's state
# files under /run/systemd, which libsystemd (polkitd's session lookup)
# reads. Then the system bus, two test actions (auth_self, auth_admin),
# two administrators in group `sudo` (admin2, admin3) and polkitd, as
# its D-Bus activation would start it (root).
set -uo pipefail
. "$(dirname "$0")/common.sh"
log=$out/polkit.log
: >"$log"

mountpoint -q /sys/fs/cgroup || mount -t cgroup2 cgroup2 /sys/fs/cgroup || exit 1
scope=/sys/fs/cgroup/user.slice/user-1000.slice/session-1.scope
mkdir -p "$scope"
mkdir -p /run/systemd/sessions /run/systemd/users /run/systemd/seats /run/systemd/machines
cat >/run/systemd/sessions/1 <<'S'
UID=1000
USER=tester
ACTIVE=1
IS_DISPLAY=1
STATE=active
REMOTE=0
TYPE=wayland
CLASS=user
SEAT=seat0
VTNR=1
SCOPE=session-1.scope
SERVICE=strand-lockvm
S
cat >/run/systemd/users/1000 <<'S'
NAME=tester
STATE=active
SESSIONS=1
ONLINE_SESSIONS=1
ACTIVE_SESSIONS=1
DISPLAY=1
S
cat >/run/systemd/seats/seat0 <<'S'
IS_SEAT0=1
CAN_MULTI_SESSION=1
CAN_TTY=1
CAN_GRAPHICAL=1
ACTIVE=1
ACTIVE_UID=1000
SESSIONS=1
UIDS=1000
S

policy=/usr/share/polkit-1/actions/org.strand.test.policy
cat >"$policy" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE policyconfig PUBLIC "-//freedesktop//DTD PolicyKit Policy Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/PolicyKit/1/policyconfig.dtd">
<policyconfig>
  <action id="org.strand.test.self">
    <message>Strand's lock VM wants to test polkit</message>
    <icon_name>dialog-password</icon_name>
    <defaults><allow_any>auth_self</allow_any><allow_inactive>auth_self</allow_inactive><allow_active>auth_self</allow_active></defaults>
  </action>
  <action id="org.strand.test.admin">
    <message>Strand's lock VM wants an administrator</message>
    <defaults><allow_any>auth_admin</allow_any><allow_inactive>auth_admin</allow_inactive><allow_active>auth_admin</allow_active></defaults>
  </action>
</policyconfig>
XML
for u in admin2 admin3; do
  id "$u" >/dev/null 2>&1 || useradd -M -G sudo -s /usr/sbin/nologin "$u"
  echo "$u:strand-$u" | chpasswd
done
getent group sudo | tee -a "$log"

mkdir -p /run/dbus
dbus-uuidgen --ensure
dbus-daemon --system --fork --nopidfile || exit 1
/usr/lib/polkit-1/polkitd --no-debug >>"$log" 2>&1 &
polkitd=$!
up=0
for _ in $(seq 100); do
  if dbus-send --system --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus \
      org.freedesktop.DBus.NameHasOwner string:org.freedesktop.PolicyKit1 2>/dev/null \
      | grep -q 'boolean true'; then
    up=1
    break
  fi
  sleep 0.1
done
[ "$up" = 1 ] || { echo "polkitd never took its name"; cat "$log"; exit 1; }

# This shell joins the session's scope; the test (and the pkcheck it
# starts) inherit it.
echo $$ >"$scope/cgroup.procs"
cat /proc/self/cgroup | tee -a "$log"
as_tester env STRAND_LOCK_VM_POLKIT=1 "$VM_AUTH_TEST" polkit --test-threads=1 --nocapture \
  2>&1 | tee -a "$log"
status=${PIPESTATUS[0]}

kill "$polkitd" 2>/dev/null
pkill -x dbus-daemon 2>/dev/null
echo $$ >/sys/fs/cgroup/cgroup.procs
rm -f "$policy"
if grep -q 'skipping ' "$log"; then
  echo "the polkit test skipped inside the VM"
  exit 1
fi
if grep -q 'running 0 tests' "$log"; then
  echo "the polkit filter matched no test"
  exit 1
fi
exit "$status"
