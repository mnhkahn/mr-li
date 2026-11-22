//! Slash command views. Never return provider credentials or another conversation's data.
use crate::reply::{Block, Layout, Reply, Tone};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const HELP: &str = "李老师命令：\n/help 帮助\n/proxy [status|list|countries|set|test|auto] 查询、按国家测速和切换代理\n/model 同时选择 Agent 和免费模型\n/status 当前会话、模型、执行与排队状态\n/skills 查看可用技能\n/mcp 查看外部工具连接状态\n/ha devices [筛选] 查看 HA 设备表格\n/ha commands <实体或设备ID> 查看可用操作\n/ha control <实体ID> <domain.service> [JSON参数] 控制设备\n/music [play|pause|next|prev|search|list|status] Apple Music 播放和搜歌\n/channel 查看消息入口和平板在线状态\n/tasks 当前会话最近 10 项任务\n/task <任务ID> 查看本会话任务详情\n/cron list 查看早间新闻播报和当前聊天的坐姿监测\n/cron run posture 立即检测一次坐姿\n/memory 查看长期记忆\n/inbox [页码] 查看播报收件箱\n/reminder list 查看本聊天的提醒\n/reminder add <时间> <内容> 创建一次性提醒\n/reminder del <ID> 删除提醒\n/baby <内容> 给宝宝发消息，显示在平板作业首页\n/baby list 查看消息送达状态\n/baby cancel <ID> 取消尚未送达的消息\n/notify <内容> 在作业首页显示通知\n/notify list 查看通知和送达状态\n/notify cancel <ID> 取消尚未送达的通知\n/speak <文字> 在 Mac mini 喇叭播报（最多 500 字）\n/speak stop 停止本会话播报并取消待播报文字\n/voice [status|tingting|clone] 查看或切换 Mac mini 播报音色\n/posture 单次坐姿检测\n/posture start [秒数] 平板作业计时期间检查，默认 60 秒；每 10 分钟图文汇总\n/posture status 最近检测结果\n/posture stop 停止定时监测\n/reframe 本地定位人体并调整学习摄像头构图\n/photo 摄像头照片\n/video [秒数] 摄像头录像，默认 10 秒，最长 30 秒\n/new 新会话，旧会话保留\n/stop 停止当前会话任务、语音和定时监测\n/approve <ID> 批准一次权限请求\n/deny <ID> 拒绝权限或提问\n/answer <ID> <JSON二维数组> 回答提问，例如 [[\"选项A\"],[\"补充说明\"]]\n群聊中请 @李老师。";

pub const HA_DEVICES_USAGE: &str = "/ha devices [筛选]";

pub fn title(command: &str) -> &'static str {
    match command.split_whitespace().next().unwrap_or("") {
        "/status" => "会话状态",
        "/channel" => "消息入口与平板",
        "/proxy" => "网络代理",
        "/music" => "音乐",
        "/skills" => "可用技能",
        "/mcp" => "外部工具",
        "/tasks" => "最近任务",
        "/task" => "任务详情",
        "/cron" => "定时任务",
        "/memory" => "长期记忆",
        "/inbox" => "播报收件箱",
        "/reminder" => "提醒",
        "/baby" | "/notify" => "平板通知",
        "/voice" | "/speak" => "语音播报",
        "/ha" => "Home Assistant",
        "/posture" => "坐姿监测",
        "/photo" | "/video" | "/reframe" => "学习摄像头",
        "/new" | "/stop" => "会话操作",
        "/approve" | "/deny" | "/answer" => "请求处理",
        _ => "李老师命令",
    }
}

fn card(title: &str, text: String, blocks: Vec<Block>) -> Reply {
    Reply {
        feishu_reply: 1,
        text,
        layout: Layout::Card,
        title: title.into(),
        tone: Tone::Neutral,
        blocks,
        actions: Vec::new(),
        select: None,
        form: None,
        media: Vec::new(),
    }
}

// Escape values from command output before passing them to Lark Markdown.
fn literal(text: &str) -> String {
    let mut output = String::new();
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '`' | '*' | '_' | '[' | ']' | '~' | '>' | '#' | '$' | '|'
        ) {
            output.push('\\');
        }
        output.push(c);
    }
    output
}

pub fn presentation(command: &str, result: &str) -> Option<Reply> {
    if result.starts_with("李老师命令：\n") {
        let headings = ["会话与工具", "作业与提醒", "设备与播报", "权限与提问"];
        let mut groups: [Vec<String>; 4] = Default::default();
        for line in result.lines().skip(1).filter(|line| line.starts_with('/')) {
            let (name, description) = line.split_once(' ').unwrap_or((line, ""));
            let group = match name {
                "/channel" | "/cron" | "/reminder" | "/baby" | "/notify" | "/posture" => 1,
                "/ha" | "/music" | "/voice" | "/speak" | "/inbox" | "/photo" | "/video"
                | "/reframe" => 2,
                "/approve" | "/deny" | "/answer" => 3,
                _ => 0,
            };
            groups[group].push(format!("`{name}` {}", literal(description)));
        }
        return Some(card(
            "李老师命令",
            "展开分类查看用法。群聊中请 @李老师。".into(),
            headings
                .into_iter()
                .zip(groups)
                .map(|(title, lines)| Block::Details {
                    title: title.into(),
                    text: lines.join("\n"),
                })
                .collect(),
        ));
    }
    if !result.contains('\n') {
        return None;
    }
    if command.trim() == "/status"
        && let Some(items) = result
            .lines()
            .map(|line| {
                let (label, value) = line.split_once('：')?;
                let value = match value {
                    "true" => "是",
                    "false" => "否",
                    value => value,
                };
                Some(crate::reply::FieldValue {
                    label: label.into(),
                    value: literal(value),
                })
            })
            .collect::<Option<Vec<_>>>()
    {
        return Some(card(
            title(command),
            "当前聊天的会话与执行状态".into(),
            vec![Block::Fields { items }],
        ));
    }
    let (first, rest) = result.split_once('\n')?;
    let (summary, body) = if (1..=120).contains(&first.trim().chars().count()) {
        (literal(first), rest)
    } else {
        (title(command).into(), result)
    };
    let body = literal(body.trim_end());
    let blocks = crate::bridge::split_text(&body, 4000);
    if blocks.len() > 4 {
        return None;
    }
    let fold = blocks.len() > 1 || body.len() > 1800;
    let count = blocks.len();
    Some(card(
        title(command),
        summary,
        blocks
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                if fold {
                    Block::Details {
                        title: if count == 1 {
                            "查看详情".into()
                        } else {
                            format!("详情 {}/{count}", index + 1)
                        },
                        text,
                    }
                } else {
                    Block::Markdown { text }
                }
            })
            .collect(),
    ))
}

pub fn ha_result(action: &str, value: &Value) -> Result<String> {
    if action == "control" {
        let entity = value["entity_id"].as_str().context("HA 实体标识缺失")?;
        let command = value["command"].as_str().context("HA 操作标识缺失")?;
        let mut text = format!("Home Assistant 已接受操作。\n实体：{entity}\n操作：{command}");
        if let Some(before) = value["before"]["state"].as_str() {
            text.push_str(&format!("\n执行前：{before}"));
        }
        if let Some(state) = value["observed_state"]["state"].as_str() {
            text.push_str(&format!("\n即时回读：{state}（HA 状态）"));
        }
        if let Some(error) = value["state_readback_error"].as_str() {
            text.push_str(&format!("\n{error}"));
        }
        if let Some(response) = value.get("service_response") {
            text.push_str(&format!(
                "\n服务返回：\n{}",
                serde_json::to_string_pretty(response)?
            ));
        }
        return Ok(text);
    }
    let entities = value["entities"]
        .as_array()
        .context("HA 操作清单格式无效")?;
    let mut text = format!("{} 个实体的可用操作", entities.len());
    for entry in entities {
        let entity = entry["entity"]["entity_id"]
            .as_str()
            .context("HA 实体标识缺失")?;
        text.push_str(&format!("\n\n{entity}"));
        let operations = entry["commands"].as_object().context("HA 操作格式无效")?;
        if operations.is_empty() {
            text.push_str("\n暂无可用操作。");
        }
        for (command, service) in operations {
            text.push_str(&format!("\n• {command}"));
            for (name, field) in service["fields"].as_object().into_iter().flatten() {
                let required = if field["required"] == true {
                    "必填"
                } else {
                    "选填"
                };
                text.push_str(&format!("\n  {name}（{required}）"));
                if let Some(description) = field["description"].as_str().filter(|s| !s.is_empty()) {
                    text.push_str(&format!(" · {description}"));
                }
                if let Some(example) = field.get("example") {
                    text.push_str(&format!(" · 示例 {example}"));
                }
                if let Some(default) = field.get("default") {
                    text.push_str(&format!(" · 默认 {default}"));
                }
                if let Some(selector) = field.get("selector") {
                    text.push_str(&format!(" · 参数限制 {selector}"));
                }
            }
        }
    }
    Ok(text)
}

const HA_PAGE_SIZE: usize = 5;

#[derive(Deserialize, Serialize)]
pub struct HaInventory {
    devices: usize,
    standalone: usize,
    rows: Vec<Vec<String>>,
}

impl HaInventory {
    pub fn pages(&self) -> usize {
        self.rows.len().div_ceil(HA_PAGE_SIZE).max(1)
    }

    pub fn render(
        &self,
        store: &crate::store::Store,
        key: &str,
        token: &str,
        page: usize,
    ) -> Result<crate::reply::Output> {
        let total = self.rows.len();
        let pages = self.pages();
        ensure!((1..=pages).contains(&page), "页码超出范围");
        let summary = if total == 0 {
            "没有匹配的 HA 设备或实体。".into()
        } else {
            format!(
                "{} 台设备 · {} 个独立实体 · {total} 行",
                self.devices, self.standalone
            )
        };
        let render = |text: String, rows: &[Vec<String>]| {
            let blocks = if rows.is_empty() {
                vec![]
            } else {
                vec![Block::Table {
                    columns: ["名称", "区域", "状态", "实体 / 设备 ID"]
                        .map(str::to_owned)
                        .to_vec(),
                    rows: rows.to_vec(),
                }]
            };
            let card = card("Home Assistant 设备", text, blocks);
            crate::reply::render(&card, store, key, "")
        };
        // Use Lark's own pager whenever the entire list fits in one card.
        if page == 1
            && total <= 30
            && let Ok(output) = render(summary.clone(), &self.rows)
        {
            return Ok(output);
        }
        let start = (page - 1) * HA_PAGE_SIZE;
        let end = (start + HA_PAGE_SIZE).min(total);
        let mut output = render(
            format!("{summary} · 第 {page}/{pages} 页"),
            &self.rows[start..end],
        )?;
        let packet = &mut output.packets[0];
        packet.token = token.into();
        let controls = [
            (page.checked_sub(1).filter(|p| *p > 0), "上一页"),
            ((page < pages).then_some(page + 1), "下一页"),
        ];
        let columns: Vec<_> = controls.into_iter().filter_map(|(target, label)| {
            let target = target?;
            Some(json!({"tag":"column","width":"weighted","weight":1,"elements":[
                {"tag":"button","text":{"tag":"plain_text","content":label},"type":"default","width":"fill",
                "behaviors":[{"type":"callback","value":{"action":"ha_inventory_page","token":token,"page":target}}]}
            ]}))
        }).collect();
        packet.content["body"]["elements"]
            .as_array_mut()
            .context("HA 表格卡片无内容")?
            .push(json!({"tag":"column_set","flex_mode":"none","columns":columns}));
        Ok(output)
    }
}

pub fn ha_devices(value: &Value) -> Result<HaInventory> {
    let devices = value["devices"].as_array().context("HA 设备清单格式无效")?;
    let standalone = value["standalone_entities"]
        .as_array()
        .context("HA 独立实体清单格式无效")?;
    let mut entries = Vec::new();
    for device in devices {
        let entities = device["entities"]
            .as_array()
            .context("HA 实体清单格式无效")?;
        if entities.is_empty() {
            entries.push((
                device["name"].as_str().unwrap_or("未命名设备"),
                device["area"].as_str().unwrap_or(""),
                "无实体",
                device["device_id"].as_str().unwrap_or(""),
            ));
        }
        entries.extend(entities.iter().map(|entity| ha_entity_row(entity, device)));
    }
    entries.extend(
        standalone
            .iter()
            .map(|entity| ha_entity_row(entity, &Value::Null)),
    );
    entries.sort_by_key(|&(name, area, _, id)| (area, name, id));
    let rows = entries
        .into_iter()
        .map(|(name, area, state, id)| {
            vec![
                preview(name, 44),
                preview(area, 24),
                preview(state, 28),
                preview(id, 299),
            ]
        })
        .collect();
    Ok(HaInventory {
        devices: devices.len(),
        standalone: standalone.len(),
        rows,
    })
}

fn ha_entity_row<'a>(entity: &'a Value, device: &'a Value) -> (&'a str, &'a str, &'a str, &'a str) {
    let id = entity["entity_id"].as_str().unwrap_or("");
    (
        entity["attributes"]["friendly_name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| device["name"].as_str())
            .unwrap_or(id),
        entity["area"].as_str().unwrap_or(""),
        entity["state"].as_str().unwrap_or("未知"),
        id,
    )
}

pub fn skills(value: &Value) -> Result<String> {
    let mut skills: Vec<_> = value
        .as_array()
        .context("技能目录格式不兼容")?
        .iter()
        .collect();
    skills.sort_by_key(|skill| skill["name"].as_str().unwrap_or(""));
    if skills.is_empty() {
        return Ok("当前未加载技能。".into());
    }
    let mut text = format!("可用技能（{} 项）：", skills.len());
    for skill in skills.iter().take(50) {
        text.push_str(&format!(
            "\n• {}：{}",
            skill["name"].as_str().unwrap_or("未命名"),
            preview(skill["description"].as_str().unwrap_or(""), 100)
        ));
    }
    if skills.len() > 50 {
        text.push_str("\n仅显示前 50 项。");
    }
    Ok(text)
}
pub fn mcp(value: &Value) -> Result<String> {
    let servers = value.as_array().context("MCP 状态格式不兼容")?;
    if servers.is_empty() {
        return Ok("当前未配置 MCP 服务。".into());
    }
    let mut text = "MCP 服务：".to_owned();
    for server in servers {
        let state = server["status"]["status"]
            .as_str()
            .or_else(|| server["status"].as_str())
            .unwrap_or("未知");
        text.push_str(&format!(
            "\n• {}：{}",
            server["name"].as_str().unwrap_or("未命名"),
            state
        ));
    }
    Ok(text)
}
pub fn preview(text: &str, limit: usize) -> String {
    let mut result: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(limit)
        .collect();
    if text.chars().count() > limit {
        result.push('…');
    }
    result
}
pub fn task_state(value: &Value) -> &str {
    match value.as_str().unwrap_or("") {
        "queued" => "排队中",
        "running" => "执行中",
        "replying" => "等待发送",
        "done" => "已结束",
        _ => "未知",
    }
}
pub fn task_list(tasks: &[Value]) -> String {
    if tasks.is_empty() {
        return "当前会话暂无任务记录。".into();
    }
    let mut text = "本会话最近任务：".to_owned();
    for task in tasks {
        text.push_str(&format!(
            "\n{} · {}\n{}",
            task_state(&task["status"]),
            preview(task["text"].as_str().unwrap_or(""), 80),
            task["id"].as_str().unwrap_or("")
        ));
    }
    text.push_str("\n/task <ID> 查看详情");
    text
}
pub fn task(value: &Value) -> String {
    let at = chrono::DateTime::from_timestamp(value["created"].as_i64().unwrap_or(0), 0)
        .map(|time| {
            time.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
                .format("%m-%d %H:%M（北京时间）")
                .to_string()
        })
        .unwrap_or_else(|| "未知".into());
    let result = if value["kind"] == "text" {
        preview(value["result"].as_str().unwrap_or(""), 600)
    } else if value["kind"] == "rich_reply"
        && value["text"]
            .as_str()
            .is_some_and(|text| text.starts_with('/'))
    {
        serde_json::from_str::<crate::reply::Output>(value["result"].as_str().unwrap_or(""))
            .map(|output| preview(&output.fallback, 600))
            .unwrap_or_else(|_| "结构化结果".into())
    } else if value["kind"] == "file_attachment" {
        serde_json::from_str::<Value>(value["result"].as_str().unwrap_or(""))
            .map(|file| {
                format!(
                    "文件附件：{}（{}）",
                    file["file_name"].as_str().unwrap_or("文件"),
                    if value["status"] == "done" {
                        "已发送"
                    } else {
                        "已上传，等待发送"
                    }
                )
            })
            .unwrap_or_else(|_| "文件附件".into())
    } else if value["kind"] == "camera_media" {
        "图文或媒体结果".into()
    } else {
        "结构化结果".into()
    };
    format!(
        "任务：{}\n状态：{}\n创建：{}\n内容：{}\n结果：{}",
        value["id"].as_str().unwrap_or(""),
        task_state(&value["status"]),
        at,
        preview(value["text"].as_str().unwrap_or(""), 200),
        if result.is_empty() {
            "尚无结果"
        } else {
            &result
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn summaries_do_not_dump_private_tool_config_or_skill_bodies() {
        let result=skills(&json!([{"name":"weather","description":"查询天气","content":"private-skill-body","path":"private-path"}])).unwrap();
        assert!(result.contains("weather"));
        assert!(!result.contains("private"));
        let result=mcp(&json!([{"name":"homework","status":{"status":"connected","token":"private-token"},"config":{"secret":"private-secret"}}])).unwrap();
        assert!(result.contains("connected"));
        assert!(!result.contains("private"));
    }
    #[test]
    fn finished_tasks_are_not_claimed_to_be_successful() {
        assert_eq!(task_state(&json!("done")), "已结束");
        assert_eq!(preview("你好\n世界", 3), "你好 …");
    }
}
