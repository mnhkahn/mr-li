#!/bin/bash
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$ROOT"
if [[ "$(uname -s)" == Darwin ]]; then
  HELPER="$ROOT/.data/voice/bin/mr-li-microphone-access"
  [[ -x "$HELPER" ]] || { echo '缺少麦克风授权程序，请运行 scripts/build-voice-mic.sh。' >&2; exit 1; }
  "$HELPER"
fi
exec "$ROOT/scripts/run.sh" voice
