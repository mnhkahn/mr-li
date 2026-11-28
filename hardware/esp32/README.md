# ESP32 硬件接入

智能音箱的 ESP32 固件、接线图、配网工具、USB MCP 适配器和 Home Assistant 包在 ESP32 Toolbox 维护。从那里构建或更新板子的固件。 [固件与接线](https://github.com/mnhkahn/esp32-toolbox/tree/main/firmware/smart-speaker)

Mr-li 主机通过局域网 HTTP 连接板子。原位置 `hardware/esp32/.local/` 可继续保存本机的连接文件。新接入也可以把连接文件放在 `.data/esp32/connection.json`，在 Mr-li 的 `.env` 中把 `MR_LI_VOICE_ESP32_CONFIG` 设为该文件的绝对路径，再从仓库根目录运行 `scripts/run.sh voice-check`。

升级时选择原板型的固件并保留 NVS。已配置设备的 token 和地址可继续沿用。使用 USB MCP 时，把 MCP 客户端里的脚本和 Python 路径改为 Toolbox 的 `firmware/smart-speaker` 下的对应文件。 [已有设备接入](https://github.com/mnhkahn/esp32-toolbox/tree/main/firmware/smart-speaker#从原-mr-li-接入)
