#!/usr/bin/env bash
# Tests image.sh, publish-image.sh and what run.sh hands docker, against a
# stand-in docker that logs its calls. run.sh runs from a copy of its files, so
# what it writes stays out of the checkout, whose target/container-names may be
# what this very container's passwd is mounted from. The runner's env reaches
# this script through run.sh, so each case sets GITHUB_ACTIONS itself.
# Run: scripts/container/run.sh scripts/container/test-image.sh
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
unset GITHUB_ACTIONS

mkdir "$DIR/bin"
cat >"$DIR/bin/docker" <<'STUB'
#!/usr/bin/env bash
# LOCAL: the images present, one per line. REGISTRY: the tags the registry
# has. MANIFEST_ERROR: what the registry answers for an absent tag. FLAKY: one
# line per pull that fails first, for another reason. RACE: a build also makes
# the tag appear, as another run's push would while this one builds.
echo "docker $*" >>"$DIR/calls"
case "$1 ${2:-}" in
  "image inspect") grep -qxF "$3" "$DIR/local" || exit 1 ;;
  "pull "*)
    if [ -s "$DIR/flaky" ]; then sed -i 1d "$DIR/flaky"; cat "$DIR/flaky-says" >&2; exit 1; fi
    if grep -qxF "$2" "$DIR/registry"; then echo "$2" >>"$DIR/local"; else cat "$DIR/manifest-error" >&2; exit 1; fi ;;
  "tag "*) echo "$3" >>"$DIR/local" ;;
  "manifest inspect")
    if grep -qxF "$3" "$DIR/registry"; then echo "{}"; else cat "$DIR/manifest-error" >&2; exit 1; fi ;;
  "build "*)
    if [ -s "$DIR/race" ]; then cat "$DIR/race" >>"$DIR/registry"; fi
    while [ "$#" -gt 0 ]; do [ "$1" = -t ] && echo "$2" >>"$DIR/local"; shift; done ;;
  "push "*) echo "$2" >>"$DIR/registry" ;;
  "ps "*) ;;
esac
exit 0
STUB
printf '#!/usr/bin/env bash\necho "sleep $*" >>"$DIR/calls"\n' >"$DIR/bin/sleep"
chmod +x "$DIR/bin/docker" "$DIR/bin/sleep"
export DIR PATH="$DIR/bin:$PATH"

hash=$(sha256sum "$HERE/Dockerfile" | cut -c1-12)
LOCAL=strand-ci:$hash
REF=ghcr.io/jakeb-grant/strand-ci:$hash

bad=0
ok() { echo "ok: $1"; }
no() { echo "not ok: $1"; sed 's/^/     /' "$DIR/calls"; bad=1; }
reset() {
  : >"$DIR/calls"; : >"$DIR/local"; : >"$DIR/registry"; : >"$DIR/flaky"; : >"$DIR/race"
  echo "net/http: TLS handshake timeout" >"$DIR/flaky-says"
  echo "manifest unknown" >"$DIR/manifest-error"
}
called() { grep -q "$1" "$DIR/calls"; }

[ "$("$HERE/image.sh" --ref)" = "$REF" ] && ok "--ref names the hash tag" || no "--ref"
matrix_hash=$(sha256sum "$HERE/Dockerfile.matrix" | cut -c1-12)
MATRIX_REF=ghcr.io/jakeb-grant/strand-matrix:$matrix_hash
[ "$(STRAND_IMAGE_FILE=Dockerfile.matrix "$HERE/image.sh" --ref)" = "$MATRIX_REF" ] && ok "the matrix's image has its own name and hash" || no "matrix --ref"
STRAND_IMAGE_FILE=Dockerfile.other "$HERE/image.sh" --ref >/dev/null 2>&1 && no "an unknown file named an image" || ok "an unknown file names no image"

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

# On GitHub a build needs the registry's word that the tag is absent.
reset
echo "$REF" >>"$DIR/registry"
printf 'x\nx\n' >"$DIR/flaky"
GITHUB_ACTIONS=true "$HERE/image.sh" >/dev/null 2>&1
{ [ "$?" = 0 ] && called "docker tag $REF $LOCAL" && ! called "docker build"; } &&
  ok "github: a pull failing twice is retried, nothing built" || no "github retry"

reset
echo "$REF" >>"$DIR/registry"
printf 'x\nx\nx\n' >"$DIR/flaky"
if err=$(GITHUB_ACTIONS=true "$HERE/image.sh" 2>&1 >/dev/null); then no "github passed on a failing pull"
elif called "docker build"; then no "github built on a failing pull"
else
  case "$err" in *"::error::"*"TLS handshake"*) ok "github: a pull that keeps failing fails the job and says why" ;; *) no "github error: $err" ;; esac
fi

reset
echo denied >"$DIR/manifest-error"
if GITHUB_ACTIONS=true "$HERE/image.sh" >/dev/null 2>&1; then no "github built on denied"
else ok "github: denied is no proof of absence"; fi

# The containerd image store's wording for an absent tag, as Docker 29 says it.
reset
echo "Error response from daemon: failed to resolve reference \"$REF\": $REF: not found" >"$DIR/manifest-error"
GITHUB_ACTIONS=true "$HERE/image.sh" >/dev/null 2>&1
{ [ "$?" = 0 ] && called "docker build -t $LOCAL"; } &&
  ok "github: containerd's 'not found' is the registry's word too" || no "github containerd"

# Only the reference's own "not found": a host or path that isn't is a failure.
reset
echo "$REF" >>"$DIR/registry"
printf 'x\nx\nx\n' >"$DIR/flaky"
echo "dial tcp: lookup ghcr.io: not found" >"$DIR/flaky-says"
if GITHUB_ACTIONS=true "$HERE/image.sh" >/dev/null 2>&1; then no "github passed on a lookup's not found"
elif called "docker build"; then no "github built on a lookup's not found"
else ok "github: another 'not found' fails the job"; fi

reset
err=$(GITHUB_ACTIONS=true "$HERE/image.sh" 2>&1 >/dev/null)
{ [ "$?" = 0 ] && called "docker build -t $LOCAL" && ! called "sleep"; } &&
  ok "github: a tag the registry says is absent builds, at once" || no "github fork"
case "$err" in *"::warning::"*) ok "github: that build warns" ;; *) no "github warning: $err" ;; esac

: >"$DIR/calls"
echo "$REF" >>"$DIR/registry"
GITHUB_ACTIONS=true "$HERE/image.sh" >/dev/null 2>&1
{ ! called "docker pull" && ! called "docker tag"; } &&
  ok "github: a job's build stays, one image for its steps" || no "github one image"

reset
printf 'x\n' >"$DIR/flaky"
"$HERE/image.sh" >/dev/null 2>&1
{ called "docker build -t $LOCAL" && ! called "sleep"; } &&
  ok "laptop: any failed pull builds, at once, as with no network" || no "laptop no network"

# publish-image.sh
reset
STRAND_IMAGE_FILE=Dockerfile.matrix "$HERE/publish-image.sh" publish >/dev/null
grep -q "docker build -t $MATRIX_REF .*Dockerfile.matrix" "$DIR/calls" && grep -q "docker push $MATRIX_REF" "$DIR/calls" && ok "publish builds the matrix's image from its own file" || no "matrix publish"
reset
out=$(STRAND_IMAGE_FILE=Dockerfile.matrix "$HERE/image.sh" 2>/dev/null)
[ "$out" = "strand-matrix:$matrix_hash" ] && grep -q "docker build -t strand-matrix:$matrix_hash -f .*Dockerfile.matrix" "$DIR/calls" && ok "absent, the matrix's image is built from its own file" || no "matrix build"
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

# Another run on another ref pushes while this one builds.
reset
echo "$REF" >"$DIR/race"
"$HERE/publish-image.sh" publish >/dev/null 2>&1
{ [ "$?" = 0 ] && called "docker build -t $REF" && ! called "docker push"; } &&
  ok "publish: a tag that appeared during the build is left alone" || no "publish race"

# image-publish's shared group: a branch or pull request's run alone. The
# group of any other run holds that run alone, so no other can cancel it.
group=$(sed -n '/^  image-publish:/,/^  [a-z-]*:$/{/^ *group:/p}' "$ROOT/.github/workflows/ci.yml")
case "$group" in
  *"needs.image.outputs.publish == 'true' && ((github.event_name == 'push' && github.ref_type == 'branch' && github.ref != 'refs/heads/main') || github.event_name == 'pull_request') && format('ci-image-publish-{0}', "*"format('ci-image-none-{0}', github.run_id)"*)
    ok "image-publish shares a group only on a branch or pull request" ;;
  *) no "image-publish's group: $group" ;;
esac

reset
echo "net/http: TLS handshake timeout" >"$DIR/manifest-error"
if "$HERE/publish-image.sh" publish >/dev/null 2>&1; then no "publish passed on a registry error"
elif called "docker push"; then no "publish pushed on a registry error"
else ok "publish: a registry error fails and pushes nothing"; fi

# What run.sh hands docker. HOME and the runner's paths are stand-ins; the
# stub's `run` does nothing.
tree=$DIR/tree
mkdir -p "$tree/scripts/container"
cp "$HERE/run.sh" "$HERE/image.sh" "$HERE/Dockerfile" "$tree/scripts/container/"
RUN=$tree/scripts/container/run.sh
reset
echo "$REF" >>"$DIR/local"
echo "$LOCAL" >>"$DIR/local"
run_args() { : >"$DIR/calls"; "$@" >/dev/null 2>&1; grep '^docker run' "$DIR/calls"; }

laptop=$(run_args env -u GITHUB_ACTIONS -u RUNNER_TEMP -u RUN_SCCACHE -u CI HOME="$DIR/home" "$RUN" true)
case "$laptop" in *"-v strand-cargo-registry:/opt/cargo/registry"*) ok "laptop: the named volumes" ;; *) no "laptop volumes" ;; esac
case "$laptop" in *"-v strand-sccache:/opt/sccache"*) ok "laptop: the compile cache" ;; *) no "laptop sccache" ;; esac
case "$laptop" in *"GITHUB_ACTIONS"* | *"CARGO_INCREMENTAL"*) no "laptop passes CI's env" ;; *) ok "laptop: none of CI's env" ;; esac

top=$("$HERE/ci-top-env.sh" "$ROOT/.github/workflows/ci.yml")
[ "$(wc -l <<<"$top")" -ge 10 ] && ok "ci.yml's top env reads" || no "top env: $top"
set -a
for name in $top; do printf -v "$name" '%s' x; done
set +a
runner=$(run_args env GITHUB_ACTIONS=true CI=true RUNNER_TEMP="$DIR/temp" GITHUB_STEP_SUMMARY="$DIR/temp/s" RUST_BACKTRACE=short HOME="$DIR/home" "$RUN" true)
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
# The runs above leave the copy's names for the real uid, if it is not 1000.
rm -rf "$tree/target/container-names"
named=$(run_args env PATH="$DIR/fake:$PATH" FAKE_UID=1001 HOME="$DIR/home" "$RUN" true)
file=$tree/target/container-names/$hash/passwd
case "$named" in *"--user 1001:123"*"$file:/etc/passwd:ro"*"/group:/etc/group:ro"*) ok "uid 1001: its names mounted" ;; *) no "names: $named" ;; esac
grep -q '^runner:x:1001:123::/tmp/home:' "$file" && ok "uid 1001: named in passwd" || no "passwd line"
rm -rf "$tree/target/container-names"
named=$(run_args env PATH="$DIR/fake:$PATH" FAKE_UID=1000 HOME="$DIR/home" "$RUN" true)
case "$named" in *"/etc/passwd"*) no "uid 1000 got a passwd" ;; *) ok "uid 1000: the image's own names" ;; esac

[ "$bad" = 0 ] && echo "test-image: all passed"
exit "$bad"
