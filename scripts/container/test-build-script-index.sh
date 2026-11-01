#!/usr/bin/env bash
# Tests crates/strand/build.rs on a throwaway repository: run where a tracked
# file was touched, it leaves git's index as it found it, and it still tells a
# clean tree from a dirty one. Its `git status` used to rewrite the index, which
# it also names in `rerun-if-changed`: on a fresh checkout the second cargo
# command then built `strand` again (decisions.md ci-speed-2).
# Run: scripts/container/run.sh scripts/container/test-build-script-index.sh

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

rustc --edition 2024 -o "$TMP/build-script" "$ROOT/crates/strand/build.rs"

cd "$TMP"
git init -q repo
cd repo
git config user.email test@example.invalid
git config user.name test
echo one >tracked
git add tracked
git -c commit.gpgsign=false commit -q -m one
commit=$(git rev-parse HEAD)

# Same content, new mtime: only a refresh would write the index for this.
sleep 1.1
touch tracked
before=$(sha256sum .git/index)
out=$("$TMP/build-script")
after=$(sha256sum .git/index)
if [ "$before" != "$after" ]; then
  echo "FAIL: the build script rewrote git's index, which it watches" >&2
  exit 1
fi
if ! grep -qx "cargo:rustc-env=STRAND_COMMIT=$commit" <<<"$out"; then
  echo "FAIL: a touched file made the tree dirty: $out" >&2
  exit 1
fi

echo two >tracked
out=$("$TMP/build-script")
if ! grep -qx "cargo:rustc-env=STRAND_COMMIT=$commit-dirty" <<<"$out"; then
  echo "FAIL: an edited file did not make the tree dirty: $out" >&2
  exit 1
fi
if ! grep -qx "cargo:rerun-if-changed=.git/index" <<<"$out"; then
  echo "FAIL: the build script stopped watching the index: $out" >&2
  exit 1
fi

echo "test-build-script-index.sh: ok"
