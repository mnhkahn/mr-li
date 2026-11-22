//! Slash command views. Never return provider credentials or another conversation's data.
use anyhow::{Context, Result};
use serde_json::Value;

pub const HELP: &str = "李老师命令：\n/help 帮助\n/proxy [status|list|countries|set|test|auto] 查询、按国家测速和切换代理\n/model 同时选择 Agent 和免费模型\n/status 当前会话、模型、执行与排队状态\n/skills 查看可用技能\n/mcp 查看外部工具连接状态\n/ha devices [筛选] 查看 HA 设备、类型和状态\n/ha commands <实体或设备ID> 查看可用操作\n/ha control <实体ID> <domain.service> [JSON参数] 控制设备\n/channel 查看消息入口和平板在线状态\n/tasks 当前会话最近 10 项任务\n/task <任务ID> 查看本会话任务详情\n/cron list 查看当前聊天的定时任务\n/cron run posture 立即检测一次坐姿\n/reminder list 查看本聊天的提醒\n/reminder add <时间> <内容> 创建一次性提醒\n/reminder del <ID> 删除提醒\n/baby <内容> 给宝宝发消息，显示在平板作业首页\n/baby list 查看消息送达状态\n/baby cancel <ID> 取消尚未送达的消息\n/notify <内容> 在作业首页显示通知\n/notify list 查看通知和送达状态\n/notify cancel <ID> 取消尚未送达的通知\n/speak <文字> 在 Mac mini 喇叭播报（最多 500 字）\n/speak stop 停止本会话播报并取消待播报文字\n/posture 单次坐姿检测\n/posture start [秒数] 平板作业计时期间检查，默认 60 秒；每 10 分钟图文汇总\n/posture status 最近检测结果\n/posture stop 停止定时监测\n/reframe 本地定位人体并调整学习摄像头构图\n/photo 摄像头照片\n/video [秒数] 摄像头录像，默认 10 秒，最长 30 秒\n/new 新会话，旧会话保留\n/stop 停止当前会话任务、语音和定时监测\n/approve <ID> 批准一次权限请求\n/deny <ID> 拒绝权限或提问\n/answer <ID> <JSON二维数组> 回答提问，例如 [[\"选项A\"],[\"补充说明\"]]\n群聊中请 @李老师。";

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
