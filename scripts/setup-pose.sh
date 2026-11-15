#!/bin/bash
# Optional local pose experiment; no account or model-provider key is needed.
set -euo pipefail
umask 077
TEMP_MODEL=""
RUNTIME_TEMP=""
cleanup() {
  if [[ -n "$TEMP_MODEL" && -f "$TEMP_MODEL" ]]; then /bin/rm -- "$TEMP_MODEL"; fi
  if [[ -n "$RUNTIME_TEMP" && -d "$RUNTIME_TEMP" ]]; then /bin/rm -r -- "$RUNTIME_TEMP"; fi
}
trap cleanup EXIT
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$ROOT"
DEST="$ROOT/.data/pose-models/movenet-thunder.onnx"
EXPECTED=3dca9f6e5f8a64dc9935a5be06fd8bf81bf01e696c9c05c6f2a650e0a401b763
mkdir -p "$(dirname "$DEST")"
check() { [[ -f "$1" && "$(shasum -a 256 "$1" | awk '{print $1}')" == "$EXPECTED" ]]; }
if ! check "$DEST"; then
  TEMP_MODEL="$(mktemp "$(dirname "$DEST")/.movenet.XXXXXX")"
  if check "$ROOT/models/movenet-thunder.onnx"; then
    cp "$ROOT/models/movenet-thunder.onnx" "$TEMP_MODEL"
  else
    curl -fL --connect-timeout 10 --max-time 300 'https://huggingface.co/Xenova/movenet-singlepose-thunder/resolve/bf87836467d0c9988f3d76b96b2fe9f4262b0394/onnx/model.onnx?download=true' -o "$TEMP_MODEL"
  fi
  check "$TEMP_MODEL" || { echo '模型校验失败，不替换已有模型。' >&2; exit 1; }
  mv "$TEMP_MODEL" "$DEST"
fi
# This deployment is Intel macOS 12; use a CPU runtime built for macOS 11+.
[[ "$(uname -s)-$(uname -m)" == Darwin-x86_64 ]] || { echo '当前姿态试验包仅提供 Intel macOS 运行库。' >&2; exit 1; }
RUNTIME="$ROOT/bin/lib/libonnxruntime.1.18.0.dylib"
RUNTIME_SHA=48f4efe527e79f5315d1bdf5accb4b17fcb6f4b49b8ee4410f876619d28b7cf4
if [[ ! -f "$RUNTIME" || "$(shasum -a 256 "$RUNTIME" | awk '{print $1}')" != "$RUNTIME_SHA" ]]; then
  RUNTIME_TEMP="$(mktemp -d "$ROOT/.data/ort-install.XXXXXX")"
  curl -fL --connect-timeout 10 --max-time 300 https://github.com/microsoft/onnxruntime/releases/download/v1.18.0/onnxruntime-osx-x86_64-1.18.0.tgz -o "$RUNTIME_TEMP/runtime.tgz"
  [[ "$(shasum -a 256 "$RUNTIME_TEMP/runtime.tgz" | awk '{print $1}')" == 3af96893675b295e5e0eb886f470de585089f92f9950158d042fbc02b44ed101 ]] || { echo '运行库下载校验失败。' >&2; exit 1; }
  python3 - "$RUNTIME_TEMP/runtime.tgz" "$RUNTIME_TEMP/runtime.dylib" <<'PYORT'
import tarfile,sys
with tarfile.open(sys.argv[1]) as t:
    m=t.getmember('onnxruntime-osx-x86_64-1.18.0/lib/libonnxruntime.1.18.0.dylib')
    with t.extractfile(m) as src,open(sys.argv[2],'wb') as out:out.write(src.read())
    from pathlib import Path
    for name in ['LICENSE','ThirdPartyNotices.txt']:
        m=t.getmember('onnxruntime-osx-x86_64-1.18.0/'+name)
        with t.extractfile(m) as src:
            Path(sys.argv[2]).parent.joinpath('ONNXRUNTIME-'+name).write_bytes(src.read())
PYORT
  [[ "$(shasum -a 256 "$RUNTIME_TEMP/runtime.dylib" | awk '{print $1}')" == "$RUNTIME_SHA" ]] || { echo '运行库文件校验失败。' >&2; exit 1; }
  mkdir -p "$(dirname "$RUNTIME")"
  mv "$RUNTIME_TEMP/runtime.dylib" "$RUNTIME"
  cp "$RUNTIME_TEMP/ONNXRUNTIME-"* "$(dirname "$RUNTIME")/"
  /bin/rm -r -- "$RUNTIME_TEMP"
fi
if [[ ! -x bin/mr-li-pose ]]; then
  if [[ -f Cargo.toml ]] && command -v cargo >/dev/null 2>&1; then
    cargo build --locked --release --features pose --bin mr-li-pose
    mkdir -p bin
    cp target/release/mr-li-pose bin/mr-li-pose
  else
    echo '缺少姿态工作程序，请安装包含 bin/mr-li-pose 的对应架构构建包。' >&2
    exit 1
  fi
fi
echo 'MoveNet Thunder 模型和本地程序已准备好。飞书使用 /posture 做单次背面姿态检查。'
echo '可用 MR_LI_POSE_ROI=x,y,width,height 指定书桌区域（归一化 0–1 比例）。'
