#!/usr/bin/env bash
# Tests image.sh, publish-image.sh and what run.sh hands docker, against a
# stand-in docker that logs its calls.
# Run: scripts/container/run.sh scripts/container/test-image.sh
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT

mkdir "$DIR/bin"
cat >"$DIR/bin/docker" <<'STUB'
#!/usr/bin/env bash
# LOCAL: the images present, one per line. REGISTRY: the tags the registry
# has. MANIFEST_ERROR: what `manifest inspect` answers for an absent tag.
echo "docker $*" >>"$DIR/calls"
case "$1 ${2:-}" in
  "image inspect") grep -qxF "$3" "$DIR/local" || exit 1 ;;
  "pull "*)
    if grep -qxF "$2" "$DIR/registry"; then echo "$2" >>"$DIR/local"; else echo "denied" >&2; exit 1; fi ;;
  "tag "*) echo "$3" >>"$DIR/local" ;;
  "manifest inspect")
    if grep -qxF "$3" "$DIR/registry"; then echo "{}"; else cat "$DIR/manifest-error" >&2; exit 1; fi ;;
  "build "*)
    while [ "$#" -gt 0 ]; do [ "$1" = -t ] && echo "$2" >>"$DIR/local"; shift; done ;;
  "push "*) echo "$2" >>"$DIR/registry" ;;
  "ps "*) ;;
esac
exit 0
STUB
chmod +x "$DIR/bin/docker"
export DIR PATH="$DIR/bin:$PATH"

hash=$(sha256sum "$HERE/Dockerfile" | cut -c1-12)
LOCAL=strand-ci:$hash
REF=ghcr.io/jakeb-grant/strand-ci:$hash

bad=0
ok() { echo "ok: $1"; }
no() { echo "not ok: $1"; sed 's/^/     /' "$DIR/calls"; bad=1; }
reset() {
  : >"$DIR/calls"; : >"$DIR/local"; : >"$DIR/registry"
  echo "manifest unknown" >"$DIR/manifest-error"
}
called() { grep -q "$1" "$DIR/calls"; }

[ "$("$HERE/image.sh" --ref)" = "$REF" ] && ok "--ref names the hash tag" || no "--ref"

reset
echo "$REF" >>"$DIR/registry"
out=$("$HERE/image.sh" 2>/dev/null)
{ [ "$out" = "$LOCAL" ] && called "docker pull $REF" && called "docker tag $REF $LOCAL" && ! called "docker build"; } &&
  ok "published and not here: pulled, named locally, nothing built" || no "pull"

: >"$DIR/calls"
out=$("$HERE/image.sh" 2>/dev/null)
{ [ "$out" = "$LOCAL" ] && ! called "docker pull" && ! called "docker build"; } &&
  ok "here already: no pull" || no "local"

reset
out=$("$HERE/image.sh" 2>/dev/null)
{ [ "$out" = "$LOCAL" ] && called "docker pull" && called "docker build -t $LOCAL"; } &&
  ok "not published: built here under the local name" || no "fallback"
called "docker push" && no "image.sh pushed" || ok "image.sh never pushes"

: >"$DIR/calls"
"$HERE/image.sh" >/dev/null 2>&1
{ called "docker pull" && ! called "docker build"; } &&
  ok "a local build: tries the pull again, builds nothing" || no "retry"
echo "$REF" >>"$DIR/registry"
: >"$DIR/calls"
"$HERE/image.sh" >/dev/null 2>&1
called "docker tag $REF $LOCAL" && ok "a local build gives way to the published image" || no "converge"

# publish-image.sh
reset
[ "$("$HERE/publish-image.sh" missing)" = true ] && ok "missing: absent is true" || no "missing true"
echo "$REF" >>"$DIR/registry"
[ "$("$HERE/publish-image.sh" missing)" = false ] && ok "missing: present is false" || no "missing false"

: >"$DIR/calls"
"$HERE/publish-image.sh" publish >/dev/null
{ ! called "docker build" && ! called "docker push"; } &&
  ok "publish: a published tag is left alone" || no "overwrite"

for answer in "manifest unknown" "denied"; do
  reset
  echo "$answer" >"$DIR/manifest-error"
  "$HERE/publish-image.sh" publish >/dev/null 2>&1
  { called "docker build -t $REF" && called "docker push $REF"; } &&
    ok "publish: '$answer' builds and pushes" || no "publish on $answer"
done

reset
echo "net/http: TLS handshake timeout" >"$DIR/manifest-error"
if "$HERE/publish-image.sh" publish >/dev/null 2>&1; then no "publish passed on a registry error"
elif called "docker push"; then no "publish pushed on a registry error"
else ok "publish: a registry error fails and pushes nothing"; fi

# What run.sh hands docker. HOME and the runner's paths are stand-ins; the
# stub's `run` does nothing.
reset
echo "$REF" >>"$DIR/local"
echo "$LOCAL" >>"$DIR/local"
run_args() { : >"$DIR/calls"; "$@" >/dev/null 2>&1; grep '^docker run' "$DIR/calls"; }

laptop=$(run_args env -u GITHUB_ACTIONS -u RUNNER_TEMP -u RUN_SCCACHE -u CI HOME="$DIR/home" "$HERE/run.sh" true)
case "$laptop" in *"-v strand-cargo-registry:/opt/cargo/registry"*) ok "laptop: the named volumes" ;; *) no "laptop volumes" ;; esac
case "$laptop" in *"-v strand-sccache:/opt/sccache"*) ok "laptop: the compile cache" ;; *) no "laptop sccache" ;; esac
case "$laptop" in *"GITHUB_ACTIONS"* | *"CARGO_INCREMENTAL"*) no "laptop passes CI's env" ;; *) ok "laptop: none of CI's env" ;; esac

top=$("$HERE/ci-top-env.sh" "$ROOT/.github/workflows/ci.yml")
[ "$(wc -l <<<"$top")" -ge 10 ] && ok "ci.yml's top env reads" || no "top env: $top"
set -a
for name in $top; do printf -v "$name" '%s' x; done
set +a
runner=$(run_args env GITHUB_ACTIONS=true CI=true RUNNER_TEMP="$DIR/temp" GITHUB_STEP_SUMMARY="$DIR/temp/s" RUST_BACKTRACE=short HOME="$DIR/home" "$HERE/run.sh" true)
for name in $top CI GITHUB_ACTIONS GITHUB_STEP_SUMMARY RUNNER_TEMP RUST_BACKTRACE; do
  case "$runner" in *"-e $name "* | *"-e $name="*) ;; *) no "runner: $name not passed"; ;; esac
done
ok "runner: the top env and the step summary's names are passed"
case "$runner" in *"-v $DIR/home/.cargo/registry:/opt/cargo/registry"*"-v $DIR/home/.cargo/git:/opt/cargo/git"*) ok "runner: ~/.cargo, for rust-cache" ;; *) no "runner cargo: $runner" ;; esac
case "$runner" in *"strand-cargo"*) no "runner: a named volume" ;; *) ok "runner: no named volume" ;; esac
case "$runner" in *"-v $DIR/temp:$DIR/temp"*) ok "runner: RUNNER_TEMP at its own path" ;; *) no "runner temp" ;; esac
case "$runner" in *"sccache"*) no "runner: sccache on" ;; *) ok "runner: no compile cache" ;; esac
case "$runner$laptop" in *"/dev/dri"*) no "a device passed unasked" ;; *) ok "no device passed unasked" ;; esac

# A uid the image doesn't name gets a line of its own; 1000 needs none.
mkdir "$DIR/fake"
for tool in id; do
  cat >"$DIR/fake/$tool" <<'FAKE'
#!/usr/bin/env bash
case "$1" in -u) echo "${FAKE_UID}" ;; -g) echo 123 ;; -un) echo runner ;; -gn) echo docker ;; esac
FAKE
  chmod +x "$DIR/fake/$tool"
done
named=$(run_args env PATH="$DIR/fake:$PATH" FAKE_UID=1001 HOME="$DIR/home" "$HERE/run.sh" true)
file=$ROOT/target/container-names/$hash/passwd
case "$named" in *"--user 1001:123"*"$file:/etc/passwd:ro"*"/group:/etc/group:ro"*) ok "uid 1001: its names mounted" ;; *) no "names: $named" ;; esac
grep -q '^runner:x:1001:123::/tmp/home:' "$file" && ok "uid 1001: named in passwd" || no "passwd line"
rm -rf "$ROOT/target/container-names/$hash"
named=$(run_args env PATH="$DIR/fake:$PATH" FAKE_UID=1000 HOME="$DIR/home" "$HERE/run.sh" true)
case "$named" in *"/etc/passwd"*) no "uid 1000 got a passwd" ;; *) ok "uid 1000: the image's own names" ;; esac

[ "$bad" = 0 ] && echo "test-image: all passed"
exit "$bad"
