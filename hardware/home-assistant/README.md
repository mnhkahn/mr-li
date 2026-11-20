# Home Assistant 局域网硬件控制

`custom_components/xgimi_apps` 为极米 NEW Z6X 增加应用列表和启动能力。
直接从投影 HTTP 16741 端口读取已安装应用，通过 UDP 16750 发送启动命令。
投影必须开机且与 HA 同网；ESP32 只承担[蓝牙开机](../esp32/README.md)，不参与 APP 控制。

## 安装

将 `custom_components/xgimi_apps` 复制到 HA 配置目录的 `custom_components/` 下，
重启 HA，然后在“设置 → 设备与服务 → 添加集成”中添加 **XGIMI Apps**，填写投影
IP 地址或主机名。此集成独立于已有的 `xgimi` 遥控器，不覆盖第三方组件。

集成提供“打开应用”select 实体：其下拉列表来自投影，**选择选项即发送启动命令**。
选中状态表示最后请求打开的应用，不是当前前台应用。应用约每 30 秒刷新，开机后也可
调用 `homeassistant.update_entity` 或以下服务主动刷新；每次启动前会重新查询，确认
投影可访问且目标 APP 仍已安装。投影离线时显示不可用，不自动开机、不重发 UDP。

## 可调用服务

`xgimi_apps.list_apps` 必须使用响应模式，返回 `apps`、`projector_online` 和 `cached`。
每个应用含 `name`、`package` 和 `label`。默认返回缓存；`refresh: true` 主动刷新。
`projector_online` 表示上次列表读取的结果；刷新失败时仍保留缓存，并标记 `cached: true`。

```yaml
action: xgimi_apps.list_apps
data:
  refresh: true
response_variable: installed_apps
```

`xgimi_apps.open_app` 接受已安装应用的准确名称、下拉标签或包名。相同名称出现多次时，
请使用包名或带包名的标签。重复打开同一个 APP 可以再次调用服务。

```yaml
action: xgimi_apps.open_app
data:
  app: com.xiaodianshi.tv.yst
response_variable: launch_result
```

启动响应的 `sent: true` 只表示 UDP 已交给系统发出，`launch_confirmed` 始终为 false；
协议没有前台应用确认能力。多个投影时，两个服务均需提供对应配置的 `entry_id`。
不要求 REST 响应的调用方，也可以用 HA 的标准 `select.select_option` 调用下拉实体，
或者直接读取 select 状态属性中的 `apps` 列表。现有 HA API/MCP 调用方因此无需新增
ESP32 专用协议。

## 测试与部署记录

```sh
python3 -m unittest discover -s hardware/home-assistant/tests -v
```

测试覆盖原始/URL 编码 JSON、重复应用名、非法目录与包名，以及 mode 7/type 1 启动
报文，避免误用现有遥控器 mode 5 按键协议。

协议参考：[homebridge-xgimi-tv](https://github.com/fantasytu/homebridge-xgimi-tv) 的应用
输入实现。该协议因机型和固件而异，软件发送结果不能代替投影画面的现场确认。

2026-11-20 已部署到 Mac mini 的原生 HA：创建 XGIMI Apps 配置，实际读取 NEW Z6X
的 16 个 APP；`list_apps` 响应和 select 列表验证一致；通过 `open_app` 发送 B 站电视版
启动命令，用户现场确认“已打开”。极米投影面板增加完整应用下拉列表，以及 B站、腾讯
视频、爱奇艺、优酷、芒果TV、央视频六个快捷按钮。其余遥控按键和 ESP32 蓝牙开机保留。
运行时仍不会将人工确认冒充为协议支持的前台应用反馈。

## Mr-li 统一控制

在 XGIMI Apps 的配置选项中关联已有 `remote` 实体和 ESP32 蓝牙开机 `script`，
集成会提供“统一控制”媒体播放器实体。开机、应用、关机、音量加减统一暴露为标准
`media_player` 操作；只声明已经接好的能力，不声明不支持的绝对音量设置。

| 操作 | HA 命令 | 实际路径 |
|---|---|---|
| 开机 | `media_player.turn_on` | HA 脚本 → Wi-Fi → ESP32 → 蓝牙 |
| 打开应用 | `media_player.select_source`，`source` 为 APP 标签或包名 | 投影局域网应用接口 |
| 关机 | `media_player.turn_off` | 已有 Wi-Fi 遥控器 |
| 音量增加/减小 | `media_player.volume_up` / `media_player.volume_down` | 已有 Wi-Fi 遥控器 |

Mr-li 保留原来的 `ha_devices`、`ha_commands`、`ha_control` 三个工具：动态发现统一实体、
读取它的 `source_list` / `apps` 和可用命令，再通过 `ha_control` 调用。无需替换 Mr-li
二进制或新增一套 ESP32 MCP。HA 面板的开机、关机、APP 快捷键和音量键也可指向此实体。

例如 Mr-li 的 `ha_control` 参数（entity_id 使用实际发现的值）：

```json
{"entity_id":"media_player.ji_mi_tou_ying_ying_yong_tong_yi_kong_zhi","command":"media_player.select_source","data":{"source":"云视听小电视"}}
```

控制网关在投影关机时仍可用，否则无法唤醒。已安装应用列表缓存在 HA 存储中，重启时
先恢复缓存；应用接口约每 30 秒检查一次。电源状态依据实际应用接口可达性，避免旧
遥控器的 RTSP 554 端口关闭时误判关机。选择应用只记录“最后请求”，不冒充前台状态；
音量支持加减，无法读取当前音量或按百分比精确设置。Mr-li 的 accepted/即时回读也不能
代替画面或声音的现场确认。

路由及关机可唤醒回归测试需使用安装了 Home Assistant 的 Python：

```sh
PYTHONPATH=/HA配置目录 /HA虚拟环境/bin/python hardware/home-assistant/tests/test_projector_control.py
```

2026-11-20 统一控制验证：5 项 HA 环境路由回归测试通过；从 Mac mini 正在部署的
`/Users/Bryce/Services/mr-li/bin/mr-li ha-mcp` 实际查询到全部标准命令，并实际调用
音量加、音量减、关机、开机、选择 B站。关机后应用 HTTP 接口离线，蓝牙开机后恢复。
另在投影关机期间重启 HA，统一实体仍为可用的 off，16 个缓存应用和开机能力均保留。
本轮没有读取当前音量或前台包名，声音变化与本轮 B站画面不算自动确认；此前 B站
启动已由用户现场确认成功。
