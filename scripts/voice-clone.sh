#!/bin/bash
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
# macOS command-line Python can pass its launcher to child interpreters, making
# the venv accidentally use the system Python's import path.
unset __PYVENV_LAUNCHER__ PYTHONHOME PYTHONPATH
exec "$ROOT/.data/voice-clone/venv/bin/python" "$ROOT/scripts/voice-clone.py" "$@"
