#!/bin/bash
set -euo pipefail
umask 077
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$ROOT"
cargo test --locked
cargo build --locked --release
PLATFORM="$(uname -s)-$(uname -m)"
DEST="$ROOT/dist/mr-li-$PLATFORM"
mkdir -p "$DEST/bin" "$DEST/scripts"
cp target/release/mr-li "$DEST/bin/"
cp scripts/init.sh scripts/run.sh scripts/service.sh scripts/watchdog-system.sh "$DEST/scripts/"
cp README.md .env.example opencode.example.json "$DEST/"
chmod +x "$DEST/bin/mr-li" "$DEST/scripts/"*.sh
COPYFILE_DISABLE=1 tar -czf "$ROOT/dist/mr-li-$PLATFORM.tar.gz" -C "$ROOT/dist" "mr-li-$PLATFORM"
echo "Built: $ROOT/dist/mr-li-$PLATFORM.tar.gz"
echo 'The binary architecture must match the deployment Mac. No secrets or conversation data are packaged.'
