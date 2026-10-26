#!/usr/bin/env bash
# media.sh [--check] [name ...]: renders the site's animated examples
# into site/media/ from their scenarios in eval/scenarios/docs/ (all of
# them when none is named). Each scenario and its dark twin
# (eval/scenarios/docs/dark/<name>.scn, the same scenario with the system
# in dark mode, written here by `strand-site twin`) run through eval's
# gate, every frame kept, so each is held to its approved reference
# recording (eval/refs/<id>.ref); then `strand-site media` writes the
# frames, their timing, their pixel hashes and the references' seals.
# Run it after changing a scenario or approving a new recording of one:
# the site's build refuses media rendered from other scenario text or
# against another recording than the one approved.
#
# --check renders nothing into site/: it runs the committed scenarios and
# twins the same way and fails when the renderer now draws other pixels
# or ticks than the committed media hold (CI's site job).
#
# Every command runs through scripts/container/run.sh; SITE_DIRECT=1 runs
# them as they are (GitHub's runner, or inside the container).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
check=0
if [ "${1:-}" = --check ]; then
  check=1
  shift
fi
if [ "${SITE_DIRECT:-0}" = 1 ]; then
  site() { cargo run -q -p strand-site -- "$@"; }
  evalrun() { bash scripts/eval/eval.sh "$@"; }
else
  site() { "$ROOT/scripts/container/run.sh" cargo run -q -p strand-site -- "$@"; }
  evalrun() { "$ROOT/scripts/container/run.sh" eval "$@"; }
fi
OUT=target/site-media
names=("$@")
if [ ${#names[@]} -eq 0 ]; then
  for f in eval/scenarios/docs/*.scn; do names+=("$(basename "$f" .scn)"); done
fi
mkdir -p "$OUT"
for n in "${names[@]}"; do
  scn=eval/scenarios/docs/$n.scn
  twin=eval/scenarios/docs/dark/$n.scn
  id=$(sed -n 's/^id *= *"\(.*\)"/\1/p' "$scn" | head -1)
  [ -n "$id" ] || { echo "$scn: no id" >&2; exit 1; }
  [ "$check" = 1 ] || site twin "$n"
  rm -rf "${OUT:?}/$n"
  if ! evalrun check --gate "$scn" "$twin" --frames all --out "$OUT/$n" >"$OUT/$n.log" 2>&1; then
    cat "$OUT/$n.log" >&2
    echo "media: $n: a run left its approved reference recording, or has none." >&2
    echo "media: look at it ($OUT/$n/index.html); once it is right, approve it" >&2
    echo "media: (scripts/container/run.sh eval approve $OUT/$n/<id> --note \"why\") and run this again." >&2
    exit 1
  fi
  if [ "$check" = 1 ]; then
    site media --check "$n" "$OUT/$n/$id" "$OUT/$n/$id-dark"
    echo "media: $n matches its committed media"
  else
    site media "$n" "$OUT/$n/$id" "$OUT/$n/$id-dark"
    echo "media: $n"
  fi
done
