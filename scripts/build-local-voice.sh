#!/bin/bash
# Build a CPU-only ZipVoice worker compatible with the Intel macOS12 deployment.
# Public models and the private reference audio/text are installed separately.
set -euo pipefail
umask 077
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
STATE="$ROOT/.data/local-tts"
[[ "$(uname -s)-$(uname -m)" == Darwin-x86_64 ]] || { echo 'This compatibility build targets Intel macOS.' >&2; exit 1; }
command -v cmake >/dev/null
mkdir -p "$STATE/source" "$STATE/runtime" "$STATE/bin" "$STATE/lib"
SOURCE_ARCHIVE="$STATE/source.ready.tar.gz"
if [[ ! -f "$STATE/source/CMakeLists.txt" ]]; then
  if [[ ! -f "$SOURCE_ARCHIVE" ]]; then
    curl -fL --connect-timeout 10 --max-time 300 https://github.com/k2-fsa/sherpa-onnx/archive/refs/tags/v1.13.8.tar.gz -o "$SOURCE_ARCHIVE"
  fi
  [[ "$(shasum -a 256 "$SOURCE_ARCHIVE" | awk '{print $1}')" == b0374cc56dbc186d442ae73d5de743bb092470b640c4c50ce7b029044c0c4fa8 ]] || { echo 'Source archive checksum mismatch.' >&2; exit 1; }
  tar -xzf "$SOURCE_ARCHIVE" --strip-components=1 -C "$STATE/source"
fi
if [[ ! -f "$STATE/runtime/include/onnxruntime_cxx_api.h" ]]; then
  ORT_ARCHIVE="${MR_LI_ORT_ARCHIVE:-$ROOT/.data/pose-models/onnxruntime-osx-x86_64-1.18.0.tgz}"
  [[ -f "$ORT_ARCHIVE" ]] || { echo 'Set MR_LI_ORT_ARCHIVE to the official onnxruntime-osx-x86_64-1.18.0.tgz archive used by setup-pose.sh.' >&2; exit 1; }
  [[ "$(shasum -a 256 "$ORT_ARCHIVE" | awk '{print $1}')" == 3af96893675b295e5e0eb886f470de585089f92f9950158d042fbc02b44ed101 ]] || { echo 'ONNX Runtime archive checksum mismatch.' >&2; exit 1; }
  tar -xzf "$ORT_ARCHIVE" --strip-components=1 -C "$STATE/runtime"
fi
export SHERPA_ONNXRUNTIME_INCLUDE_DIR="$STATE/runtime/include"
export SHERPA_ONNXRUNTIME_LIB_DIR="$STATE/runtime/lib"
export MACOSX_DEPLOYMENT_TARGET=12.0
cmake -S "$STATE/source" -B "$STATE/build" \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_DEPLOYMENT_TARGET=12.0 \
  -DBUILD_SHARED_LIBS=ON -DSHERPA_ONNX_ENABLE_PYTHON=OFF \
  -DSHERPA_ONNX_ENABLE_PORTAUDIO=OFF -DSHERPA_ONNX_ENABLE_WEBSOCKET=OFF \
  -DSHERPA_ONNX_ENABLE_SPEAKER_DIARIZATION=OFF -DSHERPA_ONNX_BUILD_C_API_EXAMPLES=OFF \
  -DSHERPA_ONNX_ENABLE_TESTS=OFF -DCMAKE_POLICY_VERSION_MINIMUM=3.5
cmake --build "$STATE/build" --target sherpa-onnx-cxx-api -j "${MR_LI_BUILD_JOBS:-4}"
cp -L "$STATE/build/lib/"*.dylib "$STATE/lib/"
cp -L "$STATE/runtime/lib/"*.dylib "$STATE/lib/"
clang++ -std=c++17 -O2 -mmacosx-version-min=12.0 -I "$STATE/source" \
  "$ROOT/scripts/local-voice.cc" -L "$STATE/lib" \
  -lsherpa-onnx-cxx-api -lsherpa-onnx-c-api \
  -Wl,-rpath,@executable_path/../lib -o "$STATE/bin/mr-li-local-voice"
python3 - "$STATE" <<'PY'
from pathlib import Path
import re,subprocess,sys
root=Path(sys.argv[1]); libs=list((root/'lib').glob('*.dylib'))
names={p.name for p in libs}
for p in libs+[root/'bin/mr-li-local-voice']:
    if p.suffix=='.dylib':
        subprocess.run(['install_name_tool','-id','@rpath/'+p.name,str(p)],check=True)
    deps=subprocess.check_output(['otool','-L',str(p)],text=True).splitlines()[1:]
    for line in deps:
        dep=line.strip().split(' (')[0]; name=Path(dep).name
        if name in names and dep!='@rpath/'+name:
            subprocess.run(['install_name_tool','-change',dep,'@rpath/'+name,str(p)],check=True)
    rpath='@loader_path' if p.suffix=='.dylib' else '@executable_path/../lib'
    info=subprocess.check_output(['otool','-l',str(p)],text=True)
    if rpath not in re.findall(r'path (.*?) \(offset',info):
        subprocess.run(['install_name_tool','-add_rpath',rpath,str(p)],check=True)
    subprocess.run(['codesign','--force','--sign','-',str(p)],check=True,capture_output=True)
PY
touch "$STATE/bin/mr-li-local-voice.lock-v2"
mkdir -p "$STATE/licenses"
cp "$STATE/source/LICENSE" "$STATE/licenses/sherpa-onnx-LICENSE"
cp "$ROOT/.data/pose-runtime/lib/ONNXRUNTIME-"* "$STATE/licenses/" 2>/dev/null || true
echo "Built: $STATE/bin/mr-li-local-voice (CPU, deployment target macOS12)"
