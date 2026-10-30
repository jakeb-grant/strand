#!/usr/bin/env bash
# ci.sh: GitHub's `package` job on this machine, from the host: the
# release archive built in the dev container (scripts/container/run.sh,
# with a pinned cargo-about installed into target/container/tools the
# first time) as the edge build of HEAD, then the installer against it
# in fresh containers on the host's Docker (smoke.sh). The archive stays
# in target/container/dist.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
ABOUT_VERSION=$(sed -n 's/^ *CARGO_ABOUT_VERSION: //p' .github/workflows/ci.yml)
TOOLS=target/container/tools
if ! [ -x "$TOOLS/bin/cargo-about" ] ||
  ! scripts/container/run.sh "$TOOLS/bin/cargo-about" --version | grep -q " $ABOUT_VERSION\$"; then
  scripts/container/run.sh cargo install --locked --root "$TOOLS" cargo-about \
    --version "$ABOUT_VERSION" --features cli
fi
id=edge-$(git rev-parse --short=12 HEAD)
scripts/container/run.sh bash -c "PATH=\$PWD/$TOOLS/bin:\$PATH scripts/release/package.sh $id target/container/dist"
scripts/release/test-publish.sh
scripts/release/smoke.sh target/container/dist
