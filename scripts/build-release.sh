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
rm -rf -- "$DEST"
mkdir -p "$DEST/bin" "$DEST/scripts"
cp target/release/mr-li "$DEST/bin/"
CYEAM_CLI_REVISION="1f049cef59204134161cac1685e3fa6aa7f936bb"
GO_BIN="$(command -v go || true)"
if [[ -z "$GO_BIN" && -x /usr/local/go/bin/go ]]; then
  GO_BIN=/usr/local/go/bin/go
fi
if [[ -z "$GO_BIN" ]]; then
  echo 'Go is required to build the Homework CLI from GitHub.' >&2
  exit 1
fi
CGO_ENABLED=0 GOTOOLCHAIN=go1.25.5 GOBIN="$DEST/bin" "$GO_BIN" install \
  -trimpath -ldflags="-s -w" "github.com/mnhkahn/cyeam-cli/cmd/cyeam@$CYEAM_CLI_REVISION"
mv "$DEST/bin/cyeam" "$DEST/bin/cyeam-homework-status"
cp scripts/init.sh scripts/init-camera.sh scripts/setup-pose.sh scripts/run.sh scripts/service.sh scripts/watchdog-system.sh scripts/boot-start.sh "$DEST/scripts/"
if [[ "${MR_LI_BUILD_POSE:-0}" == 1 ]]; then
  cp target/release/mr-li-pose "$DEST/bin/"
  if [[ -f .data/pose-runtime/lib/libonnxruntime.1.18.0.dylib ]]; then
    mkdir -p "$DEST/bin/lib"
    cp .data/pose-runtime/lib/* "$DEST/bin/lib/"
  fi
  mkdir -p "$DEST/models"
  for obsolete in movenet-lightning.onnx movenet-thunder.onnx rtmpose-s-halpe26.onnx; do
    rm -f -- "$DEST/models/$obsolete"
  done
  for name in rtmpose-m-halpe26.onnx yolox-tiny-person.onnx manifest.json; do
    if [[ -f ".data/pose-models/$name" ]]; then cp ".data/pose-models/$name" "$DEST/models/"; fi
  done
fi
mkdir -p "$DEST/resource"
cp resource/feishu-replies.txt "$DEST/resource/"
cp scripts/smoke-builtin.py "$DEST/scripts/"
cp scripts/smoke-home-assistant.py "$DEST/scripts/"
cp scripts/voice-clone.py scripts/voice-clone.sh "$DEST/scripts/"
cp scripts/voice-input.py scripts/setup-voice.sh scripts/setup-voice-models.py "$DEST/scripts/"
cp scripts/voice-mic-permission.m scripts/build-voice-mic.sh scripts/voice-start.sh "$DEST/scripts/"
cp scripts/speech_api.py scripts/speech-api.sh scripts/import-xiaoli-speech.py "$DEST/scripts/"
cp scripts/local-voice.cc scripts/local-voice.py scripts/local-voice.sh scripts/build-local-voice.sh "$DEST/scripts/"
cp README.md .env.example opencode.example.json "$DEST/"
cp scripts/auto-deploy.py "$DEST/scripts/"
cp scripts/github-release-broadcast.py "$DEST/scripts/"
chmod +x "$DEST/bin/mr-li" "$DEST/scripts/"*.sh
python3 scripts/auto-deploy.py manifest --package "$DEST" --source "$ROOT"
COPYFILE_DISABLE=1 tar -czf "$ROOT/dist/mr-li-$PLATFORM.tar.gz" -C "$ROOT/dist" "mr-li-$PLATFORM"
echo "Built: $ROOT/dist/mr-li-$PLATFORM.tar.gz"
echo 'The binary architecture must match the deployment Mac. No secrets or conversation data are packaged.'
