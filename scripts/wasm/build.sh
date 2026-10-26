#!/usr/bin/env bash
# Builds crates/strand-web for the browser (docs/wasm-spike.md): the
# release wasm, wasm-bindgen's ES-module glue, wasm-opt (-Oz by default;
# WASM_OPT=-O3, or none to skip), and the bundled fonts,
# into site/assets/wasm/ (gitignored), then prints the sizes raw, gzip
# and brotli. Run inside scripts/wasm/run.sh:
#   scripts/wasm/run.sh bash scripts/wasm/build.sh
# WASM_RUST_OPT sets the release opt-level for this build (default "s";
# 3 is the workspace's own). WASM_SIMD=1 (the default; 0 turns it off)
# builds with the simd128 target feature, so fearless_simd, under
# vello_cpu, uses wasm SIMD in place of its scalar fallback. The
# defaults are the smallest build measured as fast as opt-level 3
# (docs/wasm-spike.md, "Size and speed").
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
OUT=site/assets/wasm
rust_opt=${WASM_RUST_OPT:-s}
if [ "$rust_opt" != 3 ]; then
  export CARGO_PROFILE_RELEASE_OPT_LEVEL="$rust_opt"
fi
if [ "${WASM_SIMD:-1}" = 1 ]; then
  export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="${CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS:-} -C target-feature=+simd128"
fi
cargo build --release --target wasm32-unknown-unknown -p strand-web
WASM=target/wasm/wasm32-unknown-unknown/release/strand_web.wasm
rm -rf "$OUT"
mkdir -p "$OUT"
wasm-bindgen --target web --no-typescript --out-dir "$OUT" "$WASM"
opt=${WASM_OPT:--Oz}
if [ "$opt" != none ]; then
  wasm-opt "$opt" --enable-simd --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
    --enable-mutable-globals --enable-reference-types --enable-multivalue \
    -o "$OUT/strand_web_bg.opt.wasm" "$OUT/strand_web_bg.wasm"
  mv "$OUT/strand_web_bg.opt.wasm" "$OUT/strand_web_bg.wasm"
fi
python3 -I scripts/wasm/examples.py "$OUT/examples.json" eval/scenarios/docs/*.scn
cp crates/strand-text/assets/LiberationSans-Regular.ttf crates/strand-text/assets/LiberationSans-Bold.ttf crates/strand-text/assets/OFL.txt "$OUT/"
for f in "$OUT"/strand_web_bg.wasm "$OUT"/strand_web.js "$OUT"/LiberationSans-Regular.ttf "$OUT"/LiberationSans-Bold.ttf; do
  raw=$(stat -c %s "$f")
  gz=$(gzip -9 -c "$f" | wc -c)
  br=$(brotli -q 11 -c "$f" | wc -c)
  printf '%-40s raw %9d  gzip %9d  brotli %9d\n' "$(basename "$f")" "$raw" "$gz" "$br"
done
