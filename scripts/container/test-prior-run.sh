#!/usr/bin/env bash
# Tests scripts/container/prior-run.sh against a stand-in `gh` and
# checks-gate.sh on made-up `needs`: a green branch run of the commit proves
# its jobs on main only, a job its path filter emptied there is not proven,
# and every error, doubt or changed cache input proves nothing; the gate
# passes a skip only with that proof. Then ci.yml's jobs are held to both
# scripts' lists.
# Run: scripts/container/run.sh scripts/container/test-prior-run.sh

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
PRIOR=$HERE/prior-run.sh
GATE=$HERE/checks-gate.sh
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
bad=0

ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }

SHA=1111111111111111111111111111111111111111
OLD=2222222222222222222222222222222222222222
mkdir "$DIR/bin"
cat >"$DIR/bin/gh" <<'EOF'
#!/usr/bin/env bash
[ "$1" = api ] || { echo "gh: unexpected: $*" >&2; exit 1; }
case "$2" in
  */actions/workflows/ci.yml/runs\?*) f=runs ;;
  */actions/runs/*/jobs\?*) f=jobs ;;
  */compare/*) f=compare ;;
  *) echo "gh: unexpected: $*" >&2; exit 1 ;;
esac
[ "${GH_FAIL-}" != "$f" ] || exit 1
cat "$STUB/$f.json"
EOF
chmod +x "$DIR/bin/gh"
export STUB=$DIR PATH="$DIR/bin:$PATH" GITHUB_REPOSITORY=o/r

# runs BRANCH CONCLUSION...: one push run of $SHA per pair.
runs() {
  local i=0 sep=""
  {
    echo '{"workflow_runs":['
    while [ $# -gt 0 ]; do
      i=$((i + 1))
      printf '%s{"id":%s,"head_sha":"%s","event":"push","status":"completed","conclusion":"%s","head_branch":"%s","updated_at":"2026-11-10T0%s:00:00Z"}' \
        "$sep" "$i" "$SHA" "$2" "$1" "$i"
      sep=,
      shift 2
    done
    echo ']}'
  } >"$DIR/runs.json"
}
# jobs LOCKSTEP [JOB CONCLUSION]...: every check job a success, lock-vm's
# work step LOCKSTEP, then the overrides.
jobs() {
  local lockstep=$1
  shift
  local -A c=()
  for j in lint test budgets acceptance timing eval site wasm compositors lock-vm package; do c[$j]=success; done
  while [ $# -gt 0 ]; do c[$1]=$2; shift 2; done
  {
    echo '{"jobs":['
    local sep=""
    for j in "${!c[@]}"; do
      printf '%s{"name":"%s","conclusion":"%s","steps":[{"name":"Decide","conclusion":"success"},{"name":"Lock VM scenarios","conclusion":"%s"}]}' "$sep" "$j" "${c[$j]}" "$lockstep"
      sep=,
    done
    echo ']}'
  } >"$DIR/jobs.json"
}
# compare FILE...: a fast-forward changing these.
compare() {
  {
    printf '{"status":"ahead","files":['
    local sep=""
    for f in "$@"; do printf '%s{"filename":"%s"}' "$sep" "$f"; sep=,; done
    echo ']}'
  } >"$DIR/compare.json"
}
# prior REF [EVENT] [BEFORE]: runs it, the outputs in $DIR/out.
prior() {
  : >"$DIR/out"
  GITHUB_OUTPUT=$DIR/out GITHUB_SHA=$SHA GITHUB_REF=$1 GITHUB_EVENT_NAME=${2:-push} BEFORE=${3-$OLD} \
    "$PRIOR" >"$DIR/log" 2>&1 || no "prior-run.sh exited non-zero"
}
# want NAME KEY=VALUE...: each output is that.
want() {
  local name=$1 kv k v got fail=0
  shift
  for kv in "$@"; do
    k=${kv%%=*} v=${kv#*=}
    got=$(sed -n "s/^$k=//p" "$DIR/out")
    [ "$got" = "$v" ] || { fail=1; echo "     $k=$got, wanted $v"; }
  done
  if [ "$fail" = 0 ]; then ok "$name"; else no "$name"; sed 's/^/     /' "$DIR/log"; fi
}
M=refs/heads/main
ALL=(green=true proven_lint=true proven_test=true proven_budgets=true proven_acceptance=true proven_timing=true proven_eval=true proven_site=true proven_compositors=true proven_lock_vm=true)

runs laptop/x success
jobs success
compare crates/a/src/lib.rs docs/x.md
prior $M
want "a green branch run of the commit proves every check job on main" "${ALL[@]}"
prior refs/heads/laptop/y
want "on another branch it is a dry run" green=false proven_lint=false proven_lock_vm=false
grep -q 'dry run' "$DIR/log" && ok "and says what it would do" || no "no dry run printed"
prior refs/tags/v1
want "and on a tag" green=false proven_test=false
prior $M pull_request
want "not on a pull request" green=false proven_test=false
prior $M schedule
want "or a schedule" green=false proven_test=false

jobs skipped
prior $M
want "a lock VM its filter emptied there is not proven" green=true proven_lock_vm=false proven_test=true
jobs success timing skipped compositors cancelled
prior $M
want "nor a job that did not succeed there" green=true proven_timing=false proven_compositors=false proven_eval=true

jobs success
runs main success laptop/x failure
prior $M
want "main's own run or a red one proves nothing" green=false proven_lint=false
runs laptop/x success
GH_FAIL=runs prior $M
want "a failed runs query proves nothing" green=false proven_lint=false
GH_FAIL=jobs prior $M
want "nor a failed jobs query" green=false proven_lint=false
GH_FAIL=compare prior $M
want "nor a failed compare" green=false proven_lint=false
echo 'not json' >"$DIR/jobs.json"
prior $M
want "nor an answer it cannot read" green=false proven_lint=false
jobs success
prior $M push ""
want "nor a push with no previous head" green=false proven_lint=false
prior $M push 0000000000000000000000000000000000000000
want "nor a new branch's zero SHA" green=false proven_lint=false
for f in Cargo.lock crates/x/Cargo.toml .cargo/config.toml scripts/container/Dockerfile .github/workflows/ci.yml .github/actions/setup/action.yml; do
  compare src/a.rs "$f"
  prior $M
  want "a changed cache input ($f) proves nothing" green=false proven_lint=false
done
printf '{"status":"diverged","files":[]}' >"$DIR/compare.json"
prior $M
want "nor a push that is no fast-forward" green=false proven_lint=false

# needs OVERRIDE...: every check job a success, prior a success proving
# nothing, then jq assignments.
gate() {
  local n='{"prior":{"result":"success","outputs":{}}}' j f
  for j in lint test budgets acceptance timing eval site wasm compositors lock-vm package; do
    n=$(jq --arg j "$j" '.[$j] = {"result":"success","outputs":{}}' <<<"$n")
  done
  for f in "$@"; do n=$(jq "$f" <<<"$n"); done
  NEEDS=$n "$GATE" >"$DIR/log" 2>&1
}
expect_gate() {
  local want=$1 name=$2 got=0
  shift 2
  gate "$@" || got=$?
  if [ "$got" = "$want" ]; then ok "$name"; else no "$name: exit $got, wanted $want"; sed 's/^/     /' "$DIR/log"; fi
}
expect_gate 0 "the gate passes every job passed"
expect_gate 0 "and a skip prior proved" '.lint.result="skipped"' '.prior.outputs.proven_lint="true"' \
  '.["lock-vm"].result="skipped"' '.prior.outputs.proven_lock_vm="true"'
expect_gate 0 "and a prior that failed when nothing skipped" '.prior.result="failure"'
expect_gate 1 "not a skip nothing proved" '.timing.result="skipped"'
expect_gate 1 "nor one proved for another job" '.timing.result="skipped"' '.prior.outputs.proven_lint="true"'
expect_gate 1 "nor a proof from a prior that failed" '.lint.result="skipped"' '.prior.outputs.proven_lint="true"' '.prior.result="failure"'
expect_gate 1 "nor a failure, proved or not" '.lint.result="failure"' '.prior.outputs.proven_lint="true"'
expect_gate 1 "nor a cancel" '.eval.result="cancelled"'
expect_gate 1 "nor package or wasm skipped, which nothing proves" '.package.result="skipped"' '.prior.outputs.proven_package="true"'
expect_gate 1 "nor a missing job" 'del(.site)'
NEEDS="" "$GATE" >/dev/null 2>&1 && no "no NEEDS passed" || ok "nor no NEEDS"

# ci.yml: checks needs prior and the gate's jobs, deploy and release need
# checks, and each job prior can prove skips on its output, which prior
# passes on.
CI=$ROOT/.github/workflows/ci.yml
checks=$(sed -n 's/^CHECKS="\(.*\)"$/\1/p' "$GATE")
skippable=$(sed -n 's/^SKIPPABLE="\(.*\)"$/\1/p' "$PRIOR")
[ "$skippable" = "$(sed -n 's/^PROVABLE="\(.*\)"$/\1/p' "$GATE")" ] && ok "the gate takes a proof for prior's jobs alone" ||
  no "the gate's PROVABLE is not prior's SKIPPABLE"
grep -q "^    needs: \[prior, ${checks// /, }\]$" "$CI" && ok "ci.yml's checks needs every gated job" || no "checks' needs are not prior, $checks"
[ "$(grep -c '^    needs: checks$' "$CI")" = 2 ] && ok "deploy and release need checks" || no "deploy and release do not both need checks"
for j in $skippable; do
  k=proven_${j//-/_}
  awk -v j="  $j:" '$0 == j { f = 1; next } f && /^  [a-z]/ { exit } f' "$CI" | grep -q "needs.prior.outputs.$k != 'true'" ||
    no "$j does not skip on $k"
  grep -q "^      $k: \${{ steps.find.outputs.$k }}$" "$CI" || no "prior does not pass $k on"
  [[ " $checks " == *" $j "* ]] || no "$j is not gated"
done
ok "ci.yml's skippable jobs skip on prior's outputs"

[ "$bad" = 0 ] && echo "test-prior-run: all passed"
exit "$bad"
