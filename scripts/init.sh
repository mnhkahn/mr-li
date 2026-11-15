#!/bin/bash
# Interactive setup; never source .env as shell code or print secret values.
set -euo pipefail
set +x
umask 077
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$HOME/.opencode/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"
MODE="${1:-}"
if [[ "$MODE" == --help ]]; then
  echo 'Usage: ./scripts/init.sh [--configure-only]'
  echo 'Default: check/install build tools, build if needed, configure Feishu secrets and a free model, offer service installation.'
  echo '--configure-only: configure local files only; no downloads, builds, model login, or service installation.'
  exit 0
fi
[[ -z "$MODE" || "$MODE" == --configure-only ]] || { echo 'Unknown option; use --help.' >&2; exit 2; }
[[ "$(uname -s)" == Darwin ]] || { echo '此初始化向导用于 macOS。' >&2; exit 1; }
[[ "$EUID" != 0 ]] || { echo '请以部署用户运行，不要 sudo 整个向导。安装系统服务时会单独使用 sudo。' >&2; exit 1; }
[[ -t 0 ]] || { echo '请在交互终端中运行此向导。' >&2; exit 1; }
TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/mr-li-init.XXXXXX")"
STAGED=""
cleanup() {
  if [[ -n "$STAGED" && -f "$STAGED" ]]; then /bin/rm -- "$STAGED"; fi
  if [[ -d "$TEMP_DIR" ]]; then /bin/rm -r -- "$TEMP_DIR"; fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM
ask() {
  local prompt="$1" default="${2:-}"
  printf '%s' "$prompt"
  if [[ -n "$default" ]]; then printf ' [%s]' "$default"; fi
  printf ': '
  IFS= read -r REPLY || exit 1
  if [[ -z "$REPLY" ]]; then REPLY="$default"; fi
}
confirm() {
  ask "$1 (y/N)"
  [[ "$REPLY" == y || "$REPLY" == Y ]]
}
secret() {
  printf '%s: ' "$1"
  IFS= read -r -s REPLY || { printf '\n'; exit 1; }
  printf '\n'
}
write_var() {
  local value="$2"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//\$/\\\$}"
  printf '%s="%s"\n' "$1" "$value"
}
rust_ok() {
  command -v rustc >/dev/null 2>&1 && command -v cargo >/dev/null 2>&1 || return 1
  local ver major minor
  ver="$(rustc --version)"; ver="${ver#rustc }"; ver="${ver%% *}"
  major="${ver%%.*}"; ver="${ver#*.}"; minor="${ver%%.*}"
  [[ "$major" =~ ^[0-9]+$ && "$minor" =~ ^[0-9]+$ ]] && ((major>1 || (major==1 && minor>=88)))
}
build_if_needed() {
  local binary=""
  if [[ -x "$ROOT/bin/mr-li" ]]; then binary="$ROOT/bin/mr-li";
  elif [[ -x "$ROOT/target/release/mr-li" ]]; then binary="$ROOT/target/release/mr-li"; fi
  if [[ -n "$binary" && "$(/usr/bin/file -b "$binary")" == *"$(uname -m)"* ]]; then
    echo '找到当前架构的桥接二进制，部署机无需安装 Rust 或编译工具。'
    return
  fi
  [[ -f Cargo.toml ]] || { echo '缺少匹配架构的二进制和源码。请下载对应架构构建包，或使用完整源码目录。' >&2; exit 1; }
  if ! /usr/bin/xcode-select -p >/dev/null 2>&1 || ! /usr/bin/xcrun --find clang >/dev/null 2>&1; then
    echo '源码编译需要 Apple Command Line Tools。'
    if confirm '现在打开系统安装窗口？安装完成后重新运行本向导'; then /usr/bin/xcode-select --install; fi
    exit 1
  fi
  if ! rust_ok; then
    if ! confirm '需要 Rust 1.88+。是否通过官方 rustup 安装/准备 stable 工具链？'; then exit 1; fi
    if command -v rustup >/dev/null 2>&1; then
      rustup toolchain install stable --profile minimal
      export RUSTUP_TOOLCHAIN=stable
    else
      curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$TEMP_DIR/rustup-init.sh"
      /bin/sh "$TEMP_DIR/rustup-init.sh" -y --profile minimal --no-modify-path --default-toolchain stable
    fi
    rust_ok || { echo 'Rust 检查失败，请检查工具链配置。' >&2; exit 1; }
  fi
  echo '正在编译本机架构的桥接二进制（首次需要下载 crates）……'
  cargo build --locked --release
}
echo '李老师初始化向导：在目标 Mac mini 的最终安装目录运行。'
if [[ "$MODE" != --configure-only ]]; then build_if_needed; fi
WRITE_ENV=true
if [[ -e .env ]]; then
  echo '已有 .env，不会读取或显示其中的密钥。'
  if ! confirm '是否重新输入并替换现有 .env？选择 N 保留原配置'; then WRITE_ENV=false; fi
fi
if [[ "$WRITE_ENV" == true ]]; then
  while :; do
    ask '应用区域：feishu=国内飞书，lark=国际版' feishu; DOMAIN="$REPLY"
    [[ "$DOMAIN" == feishu || "$DOMAIN" == lark ]] && break
    echo '请输入 feishu 或 lark。'
  done
  ask '飞书 App ID' cli_aa9b39cf2f789eef; APP_ID="$REPLY"
  while :; do secret '飞书 App Secret（隐藏输入）'; APP_SECRET="$REPLY"; [[ -n "$APP_SECRET" ]] && break; echo 'App Secret 不能为空。'; done
  while :; do
    ask '允许使用的用户 open_id，多个用英文逗号分隔（ou_ 开头）'; ALLOWED="$REPLY"
    ALLOWED="${ALLOWED// /}"
    [[ "$ALLOWED" =~ ^ou_[a-zA-Z0-9_-]+(,ou_[a-zA-Z0-9_-]+)*$ ]] && break
    echo '请填写有效的 open_id 白名单，不能留空。'
  done
  ask '限定工作群 chat_id（oc_ 开头，可留空）'; ALLOWED_CHAT="$REPLY"
  [[ -z "$ALLOWED_CHAT" || "$ALLOWED_CHAT" =~ ^oc_[a-zA-Z0-9_-]+$ ]] || { echo '工作群 chat_id 格式错误。' >&2; exit 1; }
  secret 'OpenCode 本地服务密码（隐藏输入，回车自动生成）'; PASSWORD="$REPLY"
  if [[ -z "$PASSWORD" ]]; then PASSWORD="$(/usr/bin/openssl rand -hex 32)"; echo '已生成随机密码，将仅写入 .env。'; fi
  while :; do
    ask 'Agent 任务工作目录（绝对路径）' "$HOME/TeacherLiWorkspace"; WORKSPACE="$REPLY"
    [[ "$WORKSPACE" == /* ]] && break; echo '请填写绝对路径。'
  done
  mkdir -p "$WORKSPACE"
  WORKSPACE="$(cd -- "$WORKSPACE" && pwd -P)"
  [[ "$WORKSPACE" != "$ROOT" ]] || { echo '任务目录请与服务安装目录分开。' >&2; exit 1; }
  while :; do
    ask 'OpenCode 免费模型 provider/model' opencode/glm-5.3-flash-free; MODEL="$REPLY"
    [[ "$MODEL" == opencode/?* ]] && break; echo '请输入 opencode/免费模型ID。'
  done
  FOUND_OC="$(command -v opencode || true)"
  if [[ -x "$HOME/.local/bin/opencode" ]]; then FOUND_OC="$HOME/.local/bin/opencode"; fi
  while :; do
    ask 'OpenCode V2.3.x 启动器绝对路径' "$FOUND_OC"; OPENCODE_BIN="$REPLY"
    [[ -n "$OPENCODE_BIN" ]] || OPENCODE_BIN="$HOME/.local/bin/opencode"
    if [[ "$MODE" == --configure-only ]]; then break; fi
    if [[ -x "$OPENCODE_BIN" ]] && [[ "$("$OPENCODE_BIN" --version 2>/dev/null || true)" =~ (^|[[:space:]])v?2\.3\. ]]; then break; fi
    echo '需要可运行的 OpenCode V2.3.x。macOS 12 请先准备兼容启动器；向导不会替换你的兼容内核，也不会安装 V1。'
  done
  [[ "$OPENCODE_BIN" == /* ]] || { echo 'OpenCode 路径必须是绝对路径。' >&2; exit 1; }
  {
    write_var FEISHU_DOMAIN "$DOMAIN"
    write_var FEISHU_APP_ID "$APP_ID"
    write_var FEISHU_APP_SECRET "$APP_SECRET"
    write_var FEISHU_ALLOWED_OPEN_IDS "$ALLOWED"
    write_var FEISHU_ALLOWED_CHAT_IDS "$ALLOWED_CHAT"
    write_var OPENCODE_URL http://127.0.0.1:4096
    write_var OPENCODE_SERVER_USERNAME opencode
    write_var OPENCODE_SERVER_PASSWORD "$PASSWORD"
    write_var OPENCODE_BIN "$OPENCODE_BIN"
    write_var OPENCODE_MODEL "$MODEL"
    write_var OPENCODE_AGENT build
    write_var MR_LI_FREE_ONLY true
    write_var MR_LI_DIRECT_NETWORK true
    write_var MR_LI_WORKSPACE "$WORKSPACE"
    write_var MR_LI_DATA_DIR .data
    write_var MR_LI_TASK_TIMEOUT_SECONDS 1800
  } > "$TEMP_DIR/env"
  chmod 600 "$TEMP_DIR/env"
  # Stage beside the destination, so the final rename is atomic even across filesystems.
  STAGED="$(mktemp "$ROOT/.env.init.XXXXXX")"
  /bin/cat "$TEMP_DIR/env" > "$STAGED"
  chmod 600 "$STAGED"
  /bin/mv -f "$STAGED" "$ROOT/.env"
  unset APP_SECRET PASSWORD
  if [[ ! -e "$WORKSPACE/opencode.json" && ! -e "$WORKSPACE/opencode.jsonc" ]]; then
    cp "$ROOT/opencode.example.json" "$WORKSPACE/opencode.json"
    echo '已写入工作目录权限配置；工具执行会请求飞书授权。'
  else echo '工作目录已有 OpenCode 配置，予以保留。'; fi
  echo '已生成 .env（权限 600），未显示密钥。'
fi
if [[ "$MODE" == --configure-only ]]; then echo '仅配置模式完成，未下载、编译、登录模型或安装服务。'; exit 0; fi
"$ROOT/scripts/run.sh" check
echo '使用 OpenCode 官方免费模型，无需模型 API Key；免费额度不足时不会切换付费模型。'
echo '飞书后台还需启用机器人、消息权限和 im.message.receive_v1 长连接事件，并发布应用。'
echo '配置完成。下面可安装守护服务，这会开始连接飞书。'
ask '安装守护服务？输入 system=开机运行，user=登录后运行，none=暂不安装' none
case "$REPLY" in
  system) sudo "$ROOT/scripts/service.sh" install system;;
  user) "$ROOT/scripts/service.sh" install user;;
  none|'') echo '未安装常驻服务；可用 scripts/run.sh opencode 和 scripts/run.sh bridge 前台联调。';;
  *) echo '未识别选项，未安装常驻服务。';;
esac
