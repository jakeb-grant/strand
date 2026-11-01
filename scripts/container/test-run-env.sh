#!/usr/bin/env bash
# ci.yml's top env reaches the container: every name there, as run.sh
# passes it on a real docker. The lint job runs it on the runner, where that
# env is set; it needs nothing but the image.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
names=$("$HERE/ci-top-env.sh" "$ROOT/.github/workflows/ci.yml")
[ -n "$names" ] || { echo "no names in ci.yml's top env" >&2; exit 1; }

inside=$("$HERE/run.sh" env)
bad=0
for name in $names; do
  want=${!name-}
  if [ -z "${!name+set}" ]; then
    echo "not ok: $name is not set on the runner"
    bad=1
  elif grep -qxF "$name=$want" <<<"$inside"; then
    echo "ok: $name=$want"
  else
    echo "not ok: $name is '$want' here, $(grep "^$name=" <<<"$inside" || echo unset) in the container"
    bad=1
  fi
done
exit "$bad"
