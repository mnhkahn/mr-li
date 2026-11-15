#!/bin/bash
# Configure camera paths without reading secrets aloud or changing Homebridge accounts.
set -euo pipefail
set +x
umask 077
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$ROOT"
[[ -t 0 && -f .env ]] || { echo '请先运行 init.sh，再在交互终端运行本脚本。' >&2; exit 1; }
ask() {
  printf '%s [%s]: ' "$1" "$2"
  IFS= read -r REPLY
  REPLY="${REPLY:-$2}"
}
ask 'Homebridge 当前使用的 config.json 绝对路径' "$HOME/.homebridge-server/config.json"
CAMERA_HOME_CONFIG="$REPLY"
[[ "$CAMERA_HOME_CONFIG" == /* && -f "$CAMERA_HOME_CONFIG" ]] || { echo '配置文件不存在。' >&2; exit 1; }
ask '摄像头设备 ID（米家 did，仅数字）' ''
CAMERA_DEVICE_ID="$REPLY"
[[ "$CAMERA_DEVICE_ID" =~ ^[0-9]{1,24}$ ]] || { echo '设备 ID 必须是 1–24 位数字。' >&2; exit 1; }
ask '兼容当前 macOS 的 FFmpeg 绝对路径' "$HOME/.local/opt/ffmpeg-homebridge-2.1.0/ffmpeg"
CAMERA_FFMPEG="$REPLY"
[[ "$CAMERA_FFMPEG" == /* && -x "$CAMERA_FFMPEG" ]] || { echo 'FFmpeg 路径不可执行。' >&2; exit 1; }
"$CAMERA_FFMPEG" -version >/dev/null 2>&1 || { echo '此 FFmpeg 与系统不兼容。' >&2; exit 1; }
python3 - "$CAMERA_HOME_CONFIG" "$CAMERA_DEVICE_ID" "$CAMERA_FFMPEG" <<'PY'
from pathlib import Path
import json,sys
home,did,ffmpeg=sys.argv[1:]
config=json.loads(Path(home).read_text())
if not any(p.get('platform')=='XiaomiGo2rtc' for p in config.get('platforms',[])):
    raise SystemExit('请先在 Homebridge 安装并配置 homebridge-xiaomi-go2rtc。')
values={'MR_LI_HOMEBRIDGE_CONFIG':home,'MR_LI_CAMERA_DID':did,'MR_LI_FFMPEG_BIN':ffmpeg}
p=Path('.env')
lines=[line for line in p.read_text().splitlines() if line.split('=',1)[0] not in values]
def quote(value):
    return '"'+value.replace('\\','\\\\').replace('"','\\"').replace('$','\\$')+'"'
lines.extend(k+'='+quote(v) for k,v in values.items())
temp=Path('.env.camera-tmp')
with temp.open('x') as f:f.write('\n'.join(lines)+'\n')
temp.chmod(0o600)
temp.replace(p)
print('摄像头配置已写入 .env；未显示或改动现有密钥。')
PY
echo '请在 go2rtc 页面 Add → Xiaomi 连接米家账号，再重启桥接服务。'
echo '飞书指令：/photo 当前照片；/video 默认录 10 秒；/video 20 录 20 秒。'
