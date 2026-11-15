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
mkdir -p "$ROOT/.data/pose-models"
check_model() { [[ -f "$1" && "$(shasum -a 256 "$1" | awk '{print $1}')" == "$2" ]]; }
install_model() {
  local model_name="$1" model_sha="$2" archive_sha="$3" model_url="$4"
  local dest="$ROOT/.data/pose-models/$model_name"
  if check_model "$dest" "$model_sha"; then return; fi
  RUNTIME_TEMP="$(mktemp -d "$ROOT/.data/pose-download.XXXXXX")"
  if check_model "$ROOT/models/$model_name" "$model_sha"; then
    cp "$ROOT/models/$model_name" "$RUNTIME_TEMP/model.onnx"
  else
    curl -fL --connect-timeout 10 --max-time 300 "$model_url" -o "$RUNTIME_TEMP/model.zip"
    check_model "$RUNTIME_TEMP/model.zip" "$archive_sha" || { echo '模型压缩包校验失败。' >&2; exit 1; }
    python3 - "$RUNTIME_TEMP/model.zip" "$RUNTIME_TEMP/model.onnx" <<'PYMODEL'
import zipfile,sys
with zipfile.ZipFile(sys.argv[1]) as z:
    names=[n for n in z.namelist() if n.endswith('/end2end.onnx') or n=='end2end.onnx']
    assert len(names)==1,'模型文件数量不兼容'
    with open(sys.argv[2],'wb') as out:out.write(z.read(names[0]))
PYMODEL
  fi
  check_model "$RUNTIME_TEMP/model.onnx" "$model_sha" || { echo '模型文件校验失败。' >&2; exit 1; }
  mv "$RUNTIME_TEMP/model.onnx" "$dest"
  /bin/rm -r -- "$RUNTIME_TEMP"
  RUNTIME_TEMP=""
}
install_model rtmpose-m-halpe26.onnx 26f3a19e61304a600dfb82d1001d41d24343b89fc70a33ffc84657e0b0bf2ecf 55b81170e236040b59fc792ad0a8315301ac4c079a3bdb1095d838aad3088d18 https://download.openmmlab.com/mmpose/v1/projects/rtmposev1/onnx_sdk/rtmpose-m_simcc-body7_pt-body7-halpe26_700e-256x192-4d3e73dd_20230605.zip
install_model yolox-tiny-person.onnx ceb11c07298f95c50d7c5abeb906d03340c85f23aa79e3e66966e7fb6c307250 36e09ca555916253fa1b5d51bec01e48e98e1f0af966c51ce3e844d8c8fc4cfc https://download.openmmlab.com/mmpose/v1/projects/rtmposev1/onnx_sdk/yolox_tiny_8xb8-300e_humanart-6f3252f9.zip
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
echo 'YOLOX 人体检测、RTMPose-M 姿态模型和本地程序已准备好。飞书使用 /posture 做单次背面姿态检查。'
echo 'MR_LI_POSE_ROI 默认留空，整图自动找人；可选 x,y,width,height 限制候选人区域。'
