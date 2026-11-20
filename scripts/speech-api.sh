#!/bin/bash
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
unset __PYVENV_LAUNCHER__ PYTHONHOME PYTHONPATH
PYTHON="${MR_LI_SPEECH_PYTHON:-$ROOT/.data/voice/venv/bin/python}"
if [[ ! -x "$PYTHON" && -x "$ROOT/.data/voice-clone/venv/bin/python" ]]; then
  PYTHON="$ROOT/.data/voice-clone/venv/bin/python"
fi
exec "$PYTHON" "$ROOT/scripts/speech_api.py" "$@"
