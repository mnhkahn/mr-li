#!/bin/bash
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
exec "$ROOT/.data/voice-clone/venv/bin/python" "$ROOT/scripts/voice-clone.py" "$@"
