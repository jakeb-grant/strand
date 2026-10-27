#!/usr/bin/env bash
# Builds the live examples (docs/live-examples.md) into target/wasm/pkg/:
# crates/strand-web for the browser (the release wasm with simd128 and
# fearless_simd's multiply-add fused, below; wasm-bindgen's ES-module
# glue; wasm-opt -Oz), the two fonts the docs examples are
# drawn with, the icon theme files they draw (found by running them
# natively in this image, whose Adwaita and hicolor are CI's), with
# their licences, and sizes.json: each file raw, gzip and brotli. Then
# it fails when the module is over its size budget. It runs
# scripts/wasm/lint.sh first. Run inside
# scripts/wasm/run.sh (GitHub's runner runs it as it is):
#   scripts/wasm/run.sh bash scripts/wasm/build.sh
# scripts/wasm/site.sh then adds the build to a built site.
#
# The budget is a tripwire, not a cap (docs/live-examples.md, "Size"):
# WASM_BROTLI_BUDGET bytes of brotli -q 11 (default 2,500,000 against
# 2.27 MB measured), so a change that adds a large dependency to the
# render path is seen. Raise it, with a reason in docs/decisions.md,
# when the growth is worth it.
#
# WASM_RUST_OPT sets the release opt-level (default "s"; 3 is the
# workspace's own); WASM_OPT sets wasm-opt's level (-Oz; none skips it).
# The defaults are the smallest build measured as fast as opt-level 3
# (docs/live-examples.md, "Size").
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target/wasm}
OUT=target/wasm/pkg
budget=${WASM_BROTLI_BUDGET:-2500000}
rust_opt=${WASM_RUST_OPT:-s}
# First the lint no native build runs: no call to std's clock, which
# panics in the browser (scripts/wasm/lint.sh).
bash scripts/wasm/lint.sh
rm -rf "$OUT"
mkdir -p "$OUT"
# vello_cpu's SIMD layer, fearless_simd, multiplies and adds in one
# rounding natively (AVX2, NEON); its wasm backend rounds twice, which
# moves a scaled shape's edge by a step of alpha. The module is built
# with fearless_simd patched to fuse them in software, so it draws eval's
# frames bit for bit (scripts/wasm/exact-fma.py; docs/live-examples.md,
# "Pixel parity"). The patch is a `[patch.crates-io]` given on the
# command line, which cargo records in Cargo.lock; the lock is put back
# as it was when the build ends.
registry_src=$(cargo metadata --format-version 1 --locked 2>/dev/null \
  | python3 -I -c 'import json, sys; print(next(p["manifest_path"] for p in json.load(sys.stdin)["packages"] if p["name"] == "fearless_simd"))')
python3 -I scripts/wasm/exact-fma.py "$(dirname "$registry_src")" "$CARGO_TARGET_DIR/fearless_simd-exact"
lock=$(mktemp)
cp Cargo.lock "$lock"
trap 'cp "$lock" Cargo.lock; rm -f "$lock"' EXIT
(
  if [ "$rust_opt" != 3 ]; then
    export CARGO_PROFILE_RELEASE_OPT_LEVEL="$rust_opt"
  fi
  # A set RUSTFLAGS (CI's `-D warnings`) would take the place of the
  # target's flags, simd128 among them, and fearless_simd would build its
  # scalar fallback, which the patch does not fuse: so it joins them.
  export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="${RUSTFLAGS:-} ${CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS:-} -C target-feature=+simd128"
  unset RUSTFLAGS
  cargo build --release --target wasm32-unknown-unknown -p strand-web --lib \
    --config "patch.crates-io.fearless_simd.path='$CARGO_TARGET_DIR/fearless_simd-exact'"
)
cp "$lock" Cargo.lock
wasm-bindgen --target web --no-typescript --out-dir "$OUT" "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/strand_web.wasm"
opt=${WASM_OPT:--Oz}
if [ "$opt" != none ]; then
  wasm-opt "$opt" --enable-simd --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
    --enable-mutable-globals --enable-reference-types --enable-multivalue \
    -o "$OUT/strand_web_bg.opt.wasm" "$OUT/strand_web_bg.wasm"
  mv "$OUT/strand_web_bg.opt.wasm" "$OUT/strand_web_bg.wasm"
fi

# The examples as their figures carry them, and the icon files they
# draw, found natively (the same lookup the web host makes in them).
cargo run -q -p strand-site -- live > "$OUT/examples.json"
cargo run -q -p strand-web --bin strand-web-icons -- "$OUT/examples.json" "$OUT/icons"
python3 -I - "$OUT/icons" "$OUT/icons.json" <<'PY'
import base64, json, sys
from pathlib import Path
root, out = Path(sys.argv[1]), Path(sys.argv[2])
names = json.loads((root / "icons.json").read_text())
Path(out).write_text(json.dumps({n: base64.b64encode((root / n).read_bytes()).decode() for n in names}, sort_keys=True))
PY
rm -rf "$OUT/icons" "$OUT/examples.json"
cp crates/strand-text/assets/LiberationSans-Regular.ttf crates/strand-text/assets/LiberationSans-Bold.ttf "$OUT/"
cp crates/strand-text/assets/OFL.txt "$OUT/LiberationSans-OFL.txt"
if [ -f /usr/share/doc/adwaita-icon-theme/copyright ]; then
  cp /usr/share/doc/adwaita-icon-theme/copyright "$OUT/Adwaita-COPYING.txt"
fi

python3 -I - "$OUT" "$budget" <<'PY'
import gzip, json, subprocess, sys
from pathlib import Path
out, budget = Path(sys.argv[1]), int(sys.argv[2])
sizes = {}
for f in sorted(out.iterdir()):
    if f.suffix == ".txt" or f.name == "sizes.json":
        continue
    raw = f.read_bytes()
    br = subprocess.run(["brotli", "-q", "11", "-c", str(f)], capture_output=True, check=True).stdout
    sizes[f.name] = {"raw": len(raw), "gzip": len(gzip.compress(raw, 9)), "brotli": len(br)}
    s = sizes[f.name]
    print(f"{f.name:32} raw {s['raw']:9}  gzip {s['gzip']:9}  brotli {s['brotli']:9}")
(out / "sizes.json").write_text(json.dumps(sizes, indent=1, sort_keys=True) + "\n")
br = sizes["strand_web_bg.wasm"]["brotli"]
total = sum(s["brotli"] for s in sizes.values())
print(f"live examples: the module is {br} bytes brotli (budget {budget}); {total} bytes brotli in all")
if br > budget:
    print(f"live examples: the module is {br} bytes brotli, over its budget of {budget}: "
          "see what grew (docs/live-examples.md, \"Size\"); raise WASM_BROTLI_BUDGET only with a reason "
          "in docs/decisions.md", file=sys.stderr)
    sys.exit(1)
PY
