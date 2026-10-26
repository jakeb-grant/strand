#!/usr/bin/env bash
# site.sh [SITE]: adds the live examples' build (target/wasm/pkg, from
# scripts/wasm/build.sh) to a built site (target/site by default, from
# `strand-site build`): its files under wasm/, the module and its glue
# named by their content's hash (so a server may cache them for good;
# docs/live-examples.md, "Serving"),
# and wasm/manifest.js written again naming them, which is what makes
# each page offer its examples live (docs/live-examples.md). The site
# builds and works without this step; run it again after each
# `strand-site build`, which writes the manifest back to none.
# Needs only bash, python3 and sha256sum.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
SITE=${1:-target/site}
PKG=target/wasm/pkg
[ -f "$SITE/index.html" ] || { echo "site.sh: no $SITE/index.html: run strand-site build" >&2; exit 2; }
[ -f "$PKG/strand_web_bg.wasm" ] || { echo "site.sh: no $PKG: run scripts/wasm/build.sh" >&2; exit 2; }
OUT=$SITE/wasm
rm -rf "$OUT"
mkdir -p "$OUT"
named() {
  local f=$1 stem=$2 ext=$3
  local h
  h=$(sha256sum "$PKG/$f" | cut -c1-16)
  cp "$PKG/$f" "$OUT/$stem.$h.$ext"
  echo "$stem.$h.$ext"
}
glue=$(named strand_web.js strand_web js)
module=$(named strand_web_bg.wasm strand_web_bg wasm)
icons=$(named icons.json icons json)
cp "$PKG"/LiberationSans-Regular.ttf "$PKG"/LiberationSans-Bold.ttf "$PKG"/*.txt "$OUT/"
python3 -I - "$PKG/sizes.json" "$OUT/manifest.js" "$glue" "$module" "$icons" <<'PY'
import json, sys
sizes_path, out, glue, module, icons = sys.argv[1:]
sizes = json.load(open(sizes_path))
fonts = ["LiberationSans-Regular.ttf", "LiberationSans-Bold.ttf"]
download = sum(sizes[f]["brotli"] for f in ["strand_web.js", "strand_web_bg.wasm", "icons.json", *fonts])
manifest = {"glue": glue, "module": module, "icons": icons, "fonts": fonts, "download": download}
open(out, "w").write(
    "// The live examples' manifest (docs/live-examples.md), written by scripts/wasm/site.sh:\n"
    "// the WebAssembly build this site has. `download` is what a reader fetches, brotli.\n"
    f"window.STRAND_WASM = {json.dumps(manifest, sort_keys=True)};\n")
print(f"site.sh: the live examples in {out.rsplit('/', 1)[0]} ({download} bytes brotli to download)")
PY
