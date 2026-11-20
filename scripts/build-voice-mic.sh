#!/bin/bash
set -euo pipefail
umask 077
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
[[ "$(uname -s)" == Darwin ]] || exit 0
BUILD_DIR="$(mktemp -d "${TMPDIR:-/tmp}/mr-li-mic.XXXXXX")"
trap 'rm -rf -- "$BUILD_DIR"' EXIT
mkdir -p "$ROOT/.data/voice/bin"
cat > "$BUILD_DIR/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.cyeam.mr-li.microphone-access</string>
<key>CFBundleName</key><string>李老师麦克风授权</string>
<key>NSMicrophoneUsageDescription</key><string>用于“你好，李老师”语音唤醒和本地语音转文字。</string>
</dict></plist>
PLIST
xcrun clang -fobjc-arc -fblocks -mmacosx-version-min=12.0 \
  -framework Foundation -framework AVFoundation \
  -Wl,-sectcreate,__TEXT,__info_plist,"$BUILD_DIR/Info.plist" \
  "$ROOT/scripts/voice-mic-permission.m" -o "$BUILD_DIR/mr-li-microphone-access"
codesign --force --sign - "$BUILD_DIR/mr-li-microphone-access"
mv "$BUILD_DIR/mr-li-microphone-access" "$ROOT/.data/voice/bin/"
