#!/usr/bin/env bash
# Spike only (docs/decisions.md, m4-gpu-spike): edits crates/strand in
# place for one measurement build. Revert with
#   git checkout -- crates/strand Cargo.lock && rm crates/strand/src/gpu_spike.rs
# Usage: apply.sh client_system|gpu
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TOML=$ROOT/crates/strand/Cargo.toml
add() { sed -i "/^serde_json = /a $1" "$TOML"; }
add 'wayland-backend = { version = "0.3.17", features = ["client_system"] }'
[ "$1" = client_system ] && exit 0
add 'wgpu = { version = "30.0.1", default-features = false, features = ["vulkan", "wgsl", "std"] }'
add 'naga = { version = "30.0.1", features = ["wgsl-in"] }'
add 'vello_gpu = { version = "0.3.0", default-features = false, features = ["wgpu", "std"] }'
add 'vello_common = "0.3.0"'
add 'raw-window-handle = "0.6.2"'
add 'pollster = "0.4.0"'
cp "$ROOT/spikes/text-measure/gpu_spike.rs" "$ROOT/crates/strand/src/gpu_spike.rs"
MAIN=$ROOT/crates/strand/src/main.rs
sed -i '0,/^mod /s//mod gpu_spike;\nmod /' "$MAIN"
sed -i 's|^    let args: Vec<String> = std::env::args().skip(1).collect();|&\n    if std::env::var_os("STRAND_GPU_SPIKE").is_some() {\n        if let Err(e) = gpu_spike::run() {\n            eprintln!("gpu spike: {e}");\n        }\n    }|' "$MAIN"
