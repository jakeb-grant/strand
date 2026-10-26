#!/usr/bin/env bash
# measure.sh: builds strand-web at each opt-level / wasm-opt / SIMD
# variant into target/wasm-measure/<variant>/ and prints its sizes, then
# benchmarks them side by side, interleaved, in headless Chromium
# (scripts/wasm/bench.py, no network), so a busy machine slows every
# variant alike (docs/wasm-spike.md).
#   WASM_VARIANTS="3:-O3 s:-Oz:simd"   opt-level:wasm-opt[:simd]
#   WASM_BENCH_ONLY=1                   benchmark what is built
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
OUT=target/wasm-measure
mkdir -p "$OUT"
variants=(${WASM_VARIANTS:-"3:none" "3:-O3" "3:-Oz" "s:-Oz" "z:-Oz" "3:-O3:simd" "s:-Oz:simd"})
names=()
for v in "${variants[@]}"; do
  ro=${v%%:*}
  wo=${v#*:}
  simd=0
  case "$wo" in *:simd) simd=1; wo=${wo%:simd} ;; esac
  name="opt$ro-wasmopt${wo#-}$([ $simd = 1 ] && echo -simd || true)"
  names+=("$name")
  [ "${WASM_BENCH_ONLY:-0}" = 1 ] && continue
  WASM_SIMD=$simd WASM_RUST_OPT=$ro WASM_OPT=$wo scripts/wasm/run.sh bash scripts/wasm/build.sh > "$OUT/$name.sizes" 2>&1
  mkdir -p "$OUT/$name"
  cp site/assets/wasm/* "$OUT/$name/"
  echo "== $name"
  grep strand_web_bg "$OUT/$name.sizes"
done
mkdir -p "$OUT/media"
cp site/media/* "$OUT/media/"
DOCKERFILE=$ROOT/scripts/eval/Dockerfile.viewer
IMAGE=strand-viewer-check:$(sha256sum "$DOCKERFILE" | cut -c1-12)
docker run --rm --init --network none --user "$(id -u):$(id -g)" -e HOME=/tmp --shm-size=1g \
  -v "$ROOT/$OUT:/bench" -v "$ROOT/scripts/wasm:/check:ro" \
  "$IMAGE" python3 -I /check/bench.py --dir /bench --rounds "${WASM_ROUNDS:-5}" "${names[@]}" | tee "$OUT/bench.txt"
