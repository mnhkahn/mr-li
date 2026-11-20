#!/bin/bash
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
unset __PYVENV_LAUNCHER__ PYTHONHOME PYTHONPATH
exec "$ROOT/.data/voice/venv/bin/python" "$ROOT/scripts/local-voice.py" "$@"
