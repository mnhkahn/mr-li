use crate::config::Config;
use chrono::{DateTime, FixedOffset, NaiveDate, Timelike, Utc};
use std::{
    path::Path,
    time::{Duration, SystemTime},
};

fn beijing_now() -> DateTime<FixedOffset> {
    DateTime::<Utc>::from(SystemTime::now())
        .with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap())
}

fn scheduled_day(now: DateTime<FixedOffset>) -> Option<NaiveDate> {
    (now.hour() == 8 && now.minute() == 30).then(|| now.date_naive())
}

pub fn status(cfg: &Config) -> &'static str {
    if !cfg.news_enabled {
        return "未启用";
    }
    if !Path::new("scripts/news-broadcast.py").is_file() {
        return "新闻脚本未安装";
    }
    let today = beijing_now().date_naive();
    let path = cfg
        .data_dir
        .join("news-broadcast")
        .join(format!("{today}.json"));
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(record) => match record["status"].as_str() {
                Some("preparing") => "今日已开始准备",
                Some("sending") => "今日已开始播报",
                Some("done") => "今日已播报",
                Some("uncertain") => "今日播报结果未确认",
                Some("failed") => "今日执行失败",
                _ => "今日状态无法读取",
            },
            Err(_) => "今日状态无法读取",
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "今日暂无执行记录",
        Err(_) => "今日状态无法读取",
    }
}

pub async fn maintain(cfg: &Config) {
    if !cfg.news_enabled {
        std::future::pending::<()>().await;
    }
    let mut last_started = None;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let Some(day) = scheduled_day(beijing_now()).filter(|day| Some(*day) != last_started)
        else {
            continue;
        };
        last_started = Some(day);
        tracing::info!(%day, "启动早间新闻播报");
        let result = tokio::process::Command::new("/usr/bin/python3")
            .args(["scripts/news-broadcast.py", "--scheduled", "--state-dir"])
            .arg(cfg.data_dir.join("news-broadcast"))
            .kill_on_drop(true)
            .status()
            .await;
        match result {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::warn!(%status, "早间新闻播报执行失败"),
            Err(error) => tracing::warn!(%error, "无法启动早间新闻播报"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_runs_during_the_beijing_scheduled_minute() {
        for (time, due) in [
            ("08:29:59", false),
            ("08:30:00", true),
            ("08:30:59", true),
            ("08:31:00", false),
        ] {
            let timestamp =
                DateTime::parse_from_rfc3339(&format!("2026-11-23T{time}+08:00")).unwrap();
            assert_eq!(scheduled_day(timestamp).is_some(), due);
        }
    }
}
