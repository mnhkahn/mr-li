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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Job {
    Prepare,
    Play,
}

fn scheduled_job(now: DateTime<FixedOffset>) -> Option<(NaiveDate, Job)> {
    let job = match (now.hour(), now.minute(), now.second()) {
        (7, 10..=59, _) => Job::Prepare,
        (8, 0, 0..=4) => Job::Play,
        _ => return None,
    };
    Some((now.date_naive(), job))
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
                Some("preparing") => "今日正在准备日期、天气和新闻音频",
                Some("ready") => "今日音频已就绪，等待 08:00 播放",
                Some("sending") => "今日已开始播报",
                Some("done") => "今日已播报",
                Some("queued") => {
                    match record["inbox_id"]
                        .as_i64()
                        .map(|id| crate::inbox::contains(cfg, id))
                    {
                        Some(Ok(false)) => "今日已从收件箱播报",
                        Some(Ok(true)) => "今日已存入播报收件箱",
                        _ => "今日收件箱状态无法读取",
                    }
                }
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
    let mut jobs = tokio::task::JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        while let Some(result) = jobs.try_join_next() {
            if let Err(error) = result {
                tracing::warn!(%error, "早间新闻任务异常退出");
            }
        }
        let Some((day, job)) = scheduled_job(beijing_now()).filter(|job| Some(*job) != last_started)
        else {
            continue;
        };
        last_started = Some((day, job));
        let state_dir = cfg.data_dir.join("news-broadcast");
        jobs.spawn(async move {
            let flag = match job {
                Job::Prepare => "--prepare",
                Job::Play => "--play",
            };
            tracing::info!(%day, ?job, "启动早间新闻任务");
            let result = tokio::process::Command::new("/usr/bin/python3")
                .args(["scripts/news-broadcast.py", flag, "--state-dir"])
                .arg(state_dir)
                .kill_on_drop(true)
                .status()
                .await;
            match result {
                Ok(status) if status.success() => {}
                Ok(status) => tracing::warn!(%status, ?job, "早间新闻任务执行失败"),
                Err(error) => tracing::warn!(%error, ?job, "无法启动早间新闻任务"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_before_playback_and_does_not_play_late() {
        for (time, expected) in [
            ("07:09:59", None),
            ("07:10:00", Some(Job::Prepare)),
            ("07:59:59", Some(Job::Prepare)),
            ("08:00:00", Some(Job::Play)),
            ("08:00:04", Some(Job::Play)),
            ("08:00:05", None),
        ] {
            let timestamp =
                DateTime::parse_from_rfc3339(&format!("2026-11-23T{time}+08:00")).unwrap();
            assert_eq!(scheduled_job(timestamp).map(|(_, job)| job), expected);
        }
    }
}
