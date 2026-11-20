#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
PROFILE="${1:-c3}"
case "$PROFILE" in
    c3) TARGET=esp32c3 ;;
    s3) TARGET=esp32s3 ;;
    *) echo "Usage: $0 [c3|s3] [build|flash|monitor] [port]" >&2; exit 2 ;;
esac
ACTION="${2:-build}"
case "$ACTION" in build|flash|monitor) ;; *) echo "Unsupported action" >&2; exit 2 ;; esac
: "${IDF_PATH:?Source ESP-IDF export.sh first (ESP-IDF 5.5.x)}"
ARGS=(-C "$ROOT" -B "$ROOT/build/$PROFILE" -D "IDF_TARGET=$TARGET"
      -D "SDKCONFIG=$ROOT/build/$PROFILE/sdkconfig"
      -D "SDKCONFIG_DEFAULTS=$ROOT/sdkconfig.defaults;$ROOT/profiles/$PROFILE.defaults")
if [ -n "${3:-}" ]; then ARGS+=(-p "$3"); fi
exec python "$IDF_PATH/tools/idf.py" "${ARGS[@]}" "$ACTION"
