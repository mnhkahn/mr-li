#!/bin/bash
set -euo pipefail
umask 077
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$HOME/.opencode/bin:/usr/bin:/bin:/usr/sbin:/sbin"
export MR_LI_ENV="${MR_LI_ENV:-$ROOT/.env}"
BIN="${MR_LI_BIN:-$ROOT/bin/mr-li}"
if [[ ! -x "$BIN" ]]; then BIN="$ROOT/target/release/mr-li"; fi
if [[ ! -x "$BIN" ]]; then echo 'Missing binary: run scripts/build-release.sh first.' >&2; exit 1; fi
if [[ "$#" == 0 ]]; then set -- bridge; fi
exec "$BIN" "$@"
