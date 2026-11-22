# ESP32 硬件控制

独立的 ESP-IDF 固件，用于让 Mac mini 通过 ESP32 控制附近硬件。目前提供
极米 NEW Z6X 蓝牙开机，以及外接模块的 433 MHz 固定码窗帘发射路径。
后者的接线、LONGSAM 兼容性边界和升/降/停工具见 [RF433.md](RF433.md)。
硬件固件提供本地唤醒和设备控制，语音转写与智能体运行在 Mr-li 主机。
设备支持 Wi-Fi 本地 HTTP 接入 Home Assistant，也保留 USB MCP 适配器。

### ESP32-S3 本地语音唤醒

S3-CAM 的 INMP441 麦克风沿用 GPIO1（WS）、GPIO2（SCK）、GPIO42（SD）、3V3 和 GND。
构建时将支持“小李”唤醒的中文 MultiNet 模型打包到独立的 `model` 分区；首次升级须同时写入
应用、分区表和模型。旧 S3 构建目录里的生成配置应移走后重新构建，以载入新的分区和 PSRAM 默认值。
已保存的 Wi-Fi 和设备配对仍使用同一 NVS 分区。

在 Mr-li 主机设置 `MR_LI_VOICE_ESP32_CONFIG=hardware/esp32/.local/connection.json` 后，
用 `scripts/run.sh voice-check` 预检，重启已有的 Mr-li bridge 以启用内置监听。说“你好，小李”或“小李”，
等主机用坐姿播报的音色说“我在，请说”，再说指令。板上只在主机确认提示播放完毕后录这一轮，
最多约 8 秒，停顿会提前结束，静音不会生成录音。主机在家中局域网凭现有设备令牌
读取该段音频，本地 ASR 转成文本。录音约 30 秒过期，每轮都需要重新唤醒。
板载灯蓝色表示已识别唤醒词、正在等主机提示，绿色表示正在录音，结束后熄灭。

模型也可以通过内置工具 `teacher_ask_user_question` 提问。主机说完问题后，请等灯变绿就直接回答，无需再次唤醒。此路径需要支持 `on_demand_capture` 的固件。主机使用认证的 `POST /api/voice/listen`，带当前 `boot_id` 和 `wake: 0`。固件返回本次 `capture` 编号，主机只读取编号相符的录音。

主要调用链：`Home Assistant → 家庭 Wi-Fi / HTTP → ESP32 → 蓝牙 → 极米`。

可选调用链：`MCP 客户端 → Mac 上的 mcp_bridge.py → USB → ESP32 → 蓝牙 → 极米`。

## 构建与烧录

使用 ESP-IDF 5.5.x，先在当前终端运行所安装 SDK 的 `export.sh`。
本工程不捆绑 SDK，也不依赖 `xiaoli` 目录。

```sh
./build.sh c3
./build.sh s3
# 确认设备型号后，选择对应版本，指定实际串口：
./build.sh c3 flash /dev/cu.usbmodem实际编号
```

构建结果为 `build/c3/mr_li_hardware.bin` 或 `build/s3/mr_li_hardware.bin`。
烧录命令使用 ESP-IDF 生成的分区、bootloader 和应用地址。

需要 ESP32-C3 或 ESP32-S3 的**原生 USB Serial/JTAG 数据口**；仅外接
CH340/CP210x 的 UART 口不能作为当前固件的控制通道。板子若有两个 USB
口，应使用连接芯片原生 USB 的那个。固件不驱动原板的屏幕、音频等外设。

这是替换原固件的独立程序，不是给小智加插件。首次烧录前请备份原固件；
新项目使用 ESP-IDF 的单应用分区表，可能与设备上的旧分区不同。
后续普通重启和保留 NVS 的升级会保留学习数据；擦除 flash 会删除它。

## Wi-Fi 与 Home Assistant 接入

ESP32 使用 2.4 GHz Wi-Fi，首次配网通过 USB；配好后 USB 只需供电，Mac 和
MCP 适配器不必运行。板子仍应放在投影附近。无需 MQTT broker 或 ESPHome。

安装下节的 Python 依赖后，可以尝试复用原固件保存在 `wifi` NVS 命名空间中的
凭据（凭据只在板内复制，不返回给电脑）：

```sh
.venv/bin/python configure_wifi.py --port /dev/cu.usbmodem实际编号 --use-saved-wifi
```

若无旧凭据，将 `{"ssid":"家里的 2.4G SSID","password":"密码"}` 写入
本地私密 JSON 文件，限制权限，再传入 `--credentials-file /绝对路径/wifi.json`。
不要将凭据写入命令参数或提交到 Git。脚本保存新的随机 API 令牌、重启板子并
等待联网；如果联网失败，修正配网后重试。`network.configure` 本身只保存，
调用 `network.restart` 后新配置才生效。`network.status` 不返回密码或令牌。

脚本生成 `.local/connection.json` 与 `.local/ha-secrets.yaml`（权限 600，Git
忽略）。在 HA 配置中启用 `homeassistant.packages: !include_dir_named packages`，
复制 [HA package](home-assistant/mr_li_esp32.yaml) 到 `packages/`，将生成的
三个 secret 合并到 HA 的 `secrets.yaml`，检查配置后重启 HA。
不要覆盖现有的配置或 secrets 文件。

HA 提供 `script.mr_li_xgimi_power_on`、名为“极米蓝牙开机”的按钮和控制器状态
传感器。脚本可由自动化调用；语音识别和打开投影内 APP 属于另一条控制链路，
此处没有配置语音助手。

网络 API 均要求 `Authorization: Bearer <api_token>`：

| 请求 | 行为 |
|---|---|
| `GET /api/status` | 返回 network、xgimi 与 curtain 状态，不暴露唤醒 token 或网络密码 |
| `POST /api/xgimi/power_on`（空 body） | 202 + accepted，异步发送；忙碌时 409 |

极米学习和配网仍仅通过 USB。窗帘已增加认证 HTTP 学习、启用及升/降/停接口，
见 [RF433.md](RF433.md)；实物接线见 [RF433_WIRING.md](RF433_WIRING.md)。mDNS 主机名由芯片 MAC
后六位生成，形如 `mr-li-esp32-8ceff4.local`；如果 HA 主机无法解析 `.local`，
可将两个 URL 改为路由器分配的 IP，并在路由器设置 DHCP 地址保留。
HTTP 接口用于可信家庭局域网，令牌通过 HTTP 传输，不应映射到公网。
已启用 Wi-Fi/BLE 共存；上电先初始化蓝牙控制器但不发送开机广播，再连接 Wi-Fi，
配网轮询保持 USB 串口连接，避免反复打开串口影响启动。断线约每 10 秒尝试重连。
学习数据独立保存在
`xgimi_wake` NVS 命名空间，配网不会覆盖它。

## MCP 接入

在 Mac mini 上为适配器准备独立 Python 环境：

```sh
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
.venv/bin/python mcp_bridge.py --port /dev/cu.usbmodem实际编号
```

最后一条启动 stdio MCP 服务，应由 MCP 客户端启动，不需要单独常驻守护进程。
例如 OpenCode 的本地 MCP 配置（替换路径和串口）：

```json
{
  "mcp": {
    "esp32_hardware": {
      "type": "local",
      "command": [
        "/绝对路径/mr-li/hardware/esp32/.venv/bin/python",
        "/绝对路径/mr-li/hardware/esp32/mcp_bridge.py",
        "--port", "/dev/cu.usbmodem实际编号"
      ],
      "disabled": false
    }
  }
}
```

无需启动 mr-li 桥接服务才能使用这个 MCP。工具名称保留为：

| 工具 | 参数 | 用途 |
|---|---|---|
| `self.xgimi.status` | `{}` | 查看配置和后台操作状态 |
| `self.xgimi.learn` | `{}` | 扫描原遥控器开机广播，最长 20 秒并保存 |
| `self.xgimi.power_on` | `{}` | 发送保存的蓝牙唤醒信号约 3 秒 |
| `self.xgimi.configure` | `{"token":"自己的十六进制数据"}` | 手工保存已抓取的唤醒数据 |

学习步骤：投影接电但关机、ESP32 靠近原装遥控器；调用 `learn` 后查询
`status`，等 `initializing` 变成 `learning` 再按原遥控器电源键。
`learned` 表示已保存。遥控器可能同时把投影打开，这是正常的。
不要同时按其他极米遥控器，以免学习错设备。

手工配置接受 15 或 16 字节数据（30/32 个十六进制字符），不含厂商编号
`4600`。不内置网上其他投影的 token；16 字节格式在唤醒前递增并保存首字节
计数器，15 字节格式保留完整地址。

学习、开机均异步返回。`accepted` 表示任务已接收；`transmitted` 表示广播
按时结束，**不表示投影已经开机**，`physical_effect_confirmed` 始终为 false。
需现场确认或另查投影网络状态。串口断连/超时不自动重发命令，以免重复操作。
同一个 USB 设备同时只允许一个 MCP 进程控制；`tools/list` 不要求设备在线。

## 串口协议与测试

USB 上发送一行 JSON（不是 MCP）：

```json
{"id":"唯一编号","method":"xgimi.status","params":{}}
```

设备返回 `MRLI_HW_RESULT ` 前缀加一行 JSON，含相同 `id`、`result` 或
`error`。适配器处理启动日志、分段消息和旧响应；stdout 只输出 MCP JSON-RPC。

```sh
python3 -m unittest discover -s tests -v
c++ -std=c++17 -Wall -Wextra -Werror -fsanitize=address,undefined \
  -I main tests/xgimi_wake_protocol_test.cc -o /tmp/mrli-xgimi-test
/tmp/mrli-xgimi-test
```

软件检查不代替 USB 设备实测、遥控器学习和实际开机验证。
协议参考：[XGIMI 唤醒研究](https://github.com/manymuch/Xgimi-4-Home-Assistant/issues/38)。

## 已完成的设备验证

2026-11-20 已在一块 ESP32-S3（16 MB Flash、8 MB PSRAM）上完成：
原 Flash 完整备份、烧录与镜像校验、真实 USB/stdio MCP 通信、极米原装
遥控器信号学习和 NVS 保存、通过 `self.xgimi.power_on` 发射。投影接回电源
后，用户现场确认蓝牙开机成功。固件的 `physical_effect_confirmed` 仍保持
false；人工验证记录不等于固件具有电机/投影状态传感能力。

本机接入配置保存在 `.data/esp32-hardware-mcp.json`（相对 mr-li 根目录），
原 Flash 备份及验证记录位于 `artifacts/esp32-backups/`，均不提交到 Git。
这些配置已准备好，尚未自动修改其他 MCP 客户端配置。
433 MHz 已继续实现接收学习与控制；固件已烧录到当前 S3，真实遥控码和电机动作需接线后验证。

2026-11-20 Wi-Fi/HA 接入验证：S3 固件烧录并校验，重启后自动加入家中
2.4 GHz Wi-Fi，NVS 学习数据保留；上电初始化蓝牙但不发送唤醒广播。
HA package 已部署到 Mac mini，配置检查通过，极米投影面板已增加“蓝牙开机”
按钮和控制器状态。HA 按钮调用已观察到 ESP32 蓝牙任务进入发送并完成；
缺失/错误 Bearer token 返回 401。投影本轮保持开机，没有执行关机后再开机测试。

接收学习固件提供 GPIO21 TX、GPIO47 RX 的当前 S3 接线配置，三次独立按键验证、
超时退出和每键 NVS 保存。原始波形最多512个脉宽，未学齐/未显式启用时不发射。
普通 SDK 日志关闭以免混入 USB JSON，保留 USB panic 诊断。HA 的“硬件控制 → 窗帘”
页面提供三键学习、固定码启用开关和升降停；此处不包含电机位置反馈。
