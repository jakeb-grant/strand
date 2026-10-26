#!/usr/bin/env bash
# media.sh [name ...]: renders the site's animated examples into
# site/media/ from their scenarios in eval/scenarios/docs/ (all of them
# when none is named). Each scenario runs twice in the container's eval,
# light and with the system in dark mode, every frame kept; then
# `strand-site media` writes its frame strips and timing. Run it after
# changing a scenario: the site's build refuses media rendered from an
# older scenario.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
RUN="$ROOT/scripts/container/run.sh"
OUT=target/site-media
names=("$@")
if [ ${#names[@]} -eq 0 ]; then
  for f in eval/scenarios/docs/*.scn; do names+=("$(basename "$f" .scn)"); done
fi
mkdir -p "$OUT/scn"
for n in "${names[@]}"; do
  scn=eval/scenarios/docs/$n.scn
  id=$(sed -n 's/^id *= *"\(.*\)"/\1/p' "$scn" | head -1)
  [ -n "$id" ] || { echo "$scn: no id" >&2; exit 1; }
  rm -rf "${OUT:?}/$n"
  # The dark run: the same scenario with the system in dark mode, set in
  # its [services] table (one is added when it has none).
  if grep -q '^\[services\]' "$scn"; then
    awk '{ print } /^\[services\]$/ { print "\"system.dark\" = true" }' "$scn" > "$OUT/scn/$n-dark.scn"
  else
    awk '!done && /^=== / { print "[services]\n\"system.dark\" = true\n"; done = 1 } { print }' \
      "$scn" > "$OUT/scn/$n-dark.scn"
  fi
  "$RUN" eval run "$scn" --frames all --out "$OUT/$n/light" >/dev/null
  "$RUN" eval run "$OUT/scn/$n-dark.scn" --frames all --out "$OUT/$n/dark" >/dev/null
  "$RUN" cargo run -q -p strand-site -- media "$n" "$OUT/$n/light/$id" "$OUT/$n/dark/$id"
  echo "media: $n"
done
