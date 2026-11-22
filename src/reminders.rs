use crate::store::{Job, Store};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};

const HELP: &str = "/reminder list 查看提醒\n/reminder add <时间> <内容> 创建提醒\n/reminder del <ID> 删除提醒\n例：/reminder add \"2026-12-01 09:00\" 交房租";

pub fn now() -> i64 {
    crate::posture::now_epoch() as i64
}

fn beijing() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).unwrap()
}

fn parse_time(input: &str) -> Result<i64> {
    if let Ok(time) = DateTime::parse_from_rfc3339(input) {
        return Ok(time.timestamp() + i64::from(time.timestamp_subsec_nanos() != 0));
    }
    let time = NaiveDateTime::parse_from_str(input, "%Y-%m-%d %H:%M")
        .context("请使用 RFC3339 时间，或用双引号包住北京时间，如 \"2026-12-01 09:00\"")?;
    Ok(beijing().from_local_datetime(&time).unwrap().timestamp())
}

fn display_time(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .unwrap()
        .with_timezone(&beijing())
        .format("%Y-%m-%d %H:%M:%S 北京时间")
        .to_string()
}

fn add_args(input: &str) -> Option<(&str, &str)> {
    let (time, text) = if let Some(quoted) = input.strip_prefix('"') {
        let (time, tail) = quoted.split_once('"')?;
        if !tail.starts_with(char::is_whitespace) {
            return None;
        }
        (time, tail)
    } else {
        input.split_once(char::is_whitespace)?
    };
    let text = text.trim();
    (!time.is_empty() && !text.is_empty()).then_some((time, text))
}

pub fn command(store: &Store, job: &Job, args: &str) -> Result<String> {
    let scope = crate::models::scope(&job.key)?;
    let (action, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    match action {
        "" | "list" if rest.is_empty() => {
            let reminders = store.list_reminders(&scope)?;
            if reminders.is_empty() {
                return Ok("当前没有提醒。用 /reminder add <时间> <内容> 创建。".into());
            }
            let mut reply = String::from("提醒列表：");
            for reminder in reminders {
                reply.push_str(&format!(
                    "\n- {} · {} · {}\n  {}",
                    reminder.id,
                    display_time(reminder.due),
                    reminder.status,
                    reminder.text,
                ));
            }
            Ok(reply)
        }
        "add" => {
            let (time, text) = add_args(rest).context(HELP)?;
            let due = parse_time(time)?;
            ensure!(due > now(), "提醒时间已过，请用将来的时间");
            ensure!(text.chars().count() <= 500, "提醒内容最多 500 字");
            let id = store.add_reminder(job, &scope, due, text)?;
            Ok(format!("已创建提醒 {id}\n{}\n{text}", display_time(due)))
        }
        "del" | "delete" | "rm" if rest.split_whitespace().count() == 1 => {
            ensure!(store.delete_reminder(&scope, rest)?, "当前聊天没有此提醒");
            Ok(format!("已删除提醒 {rest}。"))
        }
        _ => Ok(HELP.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_beijing_time_and_rfc3339_refer_to_the_same_instant() -> Result<()> {
        let (time, text) = add_args("\"2026-12-01 09:00\"\t交房租\n核对金额").unwrap();
        assert_eq!(text, "交房租\n核对金额");
        assert_eq!(parse_time(time)?, parse_time("2026-12-01T01:00:00Z")?);
        assert!(add_args("\"2026-12-01 09:00\"交房租").is_none());
        assert!(parse_time("2026-02-30 09:00").is_err());
        Ok(())
    }
}
