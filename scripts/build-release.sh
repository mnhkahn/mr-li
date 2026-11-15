#!/bin/bash
set -euo pipefail
umask 077
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$ROOT"
cargo test --locked
cargo build --locked --release
if [[ "${MR_LI_BUILD_POSE:-0}" == 1 ]]; then
  cargo test --locked --features pose --bin mr-li-pose
  cargo build --locked --release --features pose --bin mr-li-pose
fi
PLATFORM="$(uname -s)-$(uname -m)"
DEST="$ROOT/dist/mr-li-$PLATFORM"
mkdir -p "$DEST/bin" "$DEST/scripts"
cp target/release/mr-li "$DEST/bin/"
cp scripts/init.sh scripts/init-camera.sh scripts/setup-pose.sh scripts/run.sh scripts/service.sh scripts/watchdog-system.sh "$DEST/scripts/"
if [[ "${MR_LI_BUILD_POSE:-0}" == 1 ]]; then
  cp target/release/mr-li-pose "$DEST/bin/"
  if [[ -f .data/pose-runtime/lib/libonnxruntime.1.18.0.dylib ]]; then
    mkdir -p "$DEST/bin/lib"
    cp .data/pose-runtime/lib/* "$DEST/bin/lib/"
  fi
  if [[ -f .data/pose-models/movenet-thunder.onnx ]]; then
    mkdir -p "$DEST/models"
    cp .data/pose-models/movenet-thunder.onnx "$DEST/models/"
    cp .data/pose-models/manifest.json "$DEST/models/"
  fi
fi
cp README.md .env.example opencode.example.json "$DEST/"
chmod +x "$DEST/bin/mr-li" "$DEST/scripts/"*.sh
COPYFILE_DISABLE=1 tar -czf "$ROOT/dist/mr-li-$PLATFORM.tar.gz" -C "$ROOT/dist" "mr-li-$PLATFORM"
echo "Built: $ROOT/dist/mr-li-$PLATFORM.tar.gz"
echo 'The binary architecture must match the deployment Mac. No secrets or conversation data are packaged.'
