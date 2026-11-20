#!/bin/bash
# Reproduce the camera timeout fix without changing the machine's Go toolchain.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
GO_BIN="${GO_BIN:-go}"
OUT="${1:-$ROOT/artifacts/camera-relay/go2rtc-mr-li}"
case "$OUT" in /*) ;; *) OUT="$PWD/$OUT" ;; esac
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
git -c advice.detachedHead=false clone --quiet --depth 1 --branch v1.9.14 \
  https://github.com/AlexxIT/go2rtc.git "$WORK/source"
cd "$WORK/source"
test "$(git rev-parse HEAD)" = b5948cfb25404cc5cb37b166ecaa2dca20b11d4b
git apply --check "$ROOT/resource/go2rtc/mr-li.patch"
git apply "$ROOT/resource/go2rtc/mr-li.patch"
"$GO_BIN" version
"$GO_BIN" test ./pkg/xiaomi/miss/... -count=1 -timeout=40s
mkdir -p "$(dirname -- "$OUT")"
CGO_ENABLED=0 GOOS=darwin GOARCH=amd64 "$GO_BIN" build -trimpath \
  -ldflags '-s -w -buildid=' -o "$OUT" .
shasum -a 256 "$OUT"
