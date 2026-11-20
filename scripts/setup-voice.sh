#!/bin/bash
set -euo pipefail
umask 077
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$ROOT"
PYTHON="${MR_LI_SETUP_PYTHON:-python3}"
"$PYTHON" -m venv .data/voice/venv
SHERPA_VERSION=1.12.38
# Newer wheels link CoreML symbols unavailable on the Intel deployment Mac.
if [[ "$(uname -s)" == Darwin && "$(sw_vers -productVersion | cut -d. -f1)" -lt 15 ]]; then
  SHERPA_VERSION=1.10.46
fi
.data/voice/venv/bin/python -m pip install "sherpa-onnx==$SHERPA_VERSION" 'sounddevice==0.5.3' 'numpy>=1.23,<3'
bash scripts/build-voice-mic.sh
exec .data/voice/venv/bin/python scripts/setup-voice-models.py "$@"
