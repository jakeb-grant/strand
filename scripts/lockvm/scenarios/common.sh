# Sourced by the scenarios (in the guest, as root): the binaries
# build.sh recorded, and `as_tester CMD...` to run a command as the test
# user with the lock tests' environment.
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
out=$root/target/lockvm
if [ ! -f "$out/bins.env" ]; then
  echo "no $out/bins.env: run scripts/lockvm/scenarios/build.sh first (through run.sh)"
  exit 1
fi
# shellcheck source=/dev/null
. "$out/bins.env"
[ "$(hostname)" = lockvm ] || { echo "not in the lock VM guest: refusing to run"; exit 1; }

as_tester() {
  runuser -u tester -- env -i \
    PATH=/usr/local/bin:/usr/bin:/bin HOME=/home/tester LANG=C.UTF-8 \
    XDG_RUNTIME_DIR=/run/user/1000 STRAND_LOCK_VM=1 STRAND_REQUIRE_SWAY=1 \
    RUST_BACKTRACE=1 "$@"
}
