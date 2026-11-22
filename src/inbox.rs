use crate::{config::Config, models, speech};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::Deserialize;
use std::{path::PathBuf, time::Duration};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Open,
    Sleep,
    DoNotDisturb,
    Unavailable,
}

impl Focus {
    pub fn allows_speech(self) -> bool {
        self == Self::Open
    }

    fn label(self) -> &'static str {
        match self {
            Self::Open => "允许播报",
            Self::Sleep => "Mac 睡眠专注模式，播报暂存",
            Self::DoNotDisturb => "Mac 勿扰专注模式，播报暂存",
            Self::Unavailable => "无法读取 Mac 专注模式，播报暂存",
        }
    }
}

#[derive(Deserialize)]
struct Assertions {
    data: Vec<AssertionStore>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssertionStore {
    #[serde(default)]
    store_assertion_records: Vec<Assertion>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Assertion {
    assertion_details: AssertionDetails,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssertionDetails {
    assertion_details_mode_identifier: String,
}

fn decode_focus(bytes: &[u8]) -> Result<Focus> {
    let state: Assertions = serde_json::from_slice(bytes)?;
    let mut focus = Focus::Open;
    for record in state
        .data
        .into_iter()
        .flat_map(|data| data.store_assertion_records)
    {
        match record
            .assertion_details
            .assertion_details_mode_identifier
            .as_str()
        {
            "com.apple.sleep.sleep-mode" => return Ok(Focus::Sleep),
            "com.apple.donotdisturb.mode.default" => focus = Focus::DoNotDisturb,
            _ => {}
        }
    }
    Ok(focus)
}

pub fn focus(cfg: &Config) -> Focus {
    let Some(path) = &cfg.focus_assertions else {
        return Focus::Open;
    };
    match std::fs::read(path) {
        Ok(bytes) => decode_focus(&bytes).unwrap_or(Focus::Unavailable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Focus::Open,
        Err(_) => Focus::Unavailable,
    }
}

fn path(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("broadcast-inbox.sqlite3")
}

fn connect(cfg: &Config) -> Result<Connection> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let db = Connection::open(path(cfg))?;
    db.busy_timeout(Duration::from_secs(2))?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS broadcasts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            scope TEXT NOT NULL,
            text TEXT NOT NULL,
            queued_at INTEGER NOT NULL DEFAULT (unixepoch())
        );",
    )?;
    Ok(db)
}

pub fn enqueue(cfg: &Config, scope: Option<&str>, text: &str) -> Result<i64> {
    let db = connect(cfg)?;
    db.execute(
        "INSERT INTO broadcasts (scope,text) VALUES (?1,?2)",
        params![scope.unwrap_or_default(), text],
    )?;
    Ok(db.last_insert_rowid())
}

pub fn contains(cfg: &Config, id: i64) -> Result<bool> {
    ensure!(path(cfg).is_file(), "播报收件箱不存在");
    Ok(connect(cfg)?.query_row(
        "SELECT EXISTS(SELECT 1 FROM broadcasts WHERE id=?1)",
        [id],
        |row| row.get(0),
    )?)
}

pub fn command(cfg: &Config, key: &str, args: &str) -> Result<String> {
    let page = if matches!(args, "" | "list") {
        1
    } else {
        args.parse::<u32>().context("用法：/inbox [页码]")?
    };
    ensure!(page > 0, "页码从 1 开始");
    let scope = models::scope(key)?;
    let [_, _, _, thread]: [String; 4] = serde_json::from_str(key)?;
    let mut output = format!("播报收件箱 · {}", focus(cfg).label());
    if !path(cfg).exists() {
        output.push_str("\n暂无待播报内容。");
        return Ok(output);
    }
    let db = connect(cfg)?;
    let total: u32 = db.query_row(
        "SELECT count(*) FROM broadcasts WHERE scope=?1 OR (?2 AND scope='')",
        params![scope, thread.is_empty()],
        |row| row.get(0),
    )?;
    if total == 0 {
        output.push_str("\n暂无待播报内容。");
        return Ok(output);
    }
    let pages = total.div_ceil(10);
    ensure!(page <= pages, "收件箱共 {pages} 页");
    output.push_str(&format!("\n{total} 条待播报 · 第 {page}/{pages} 页"));
    let mut stmt = db.prepare(
        "SELECT strftime('%m-%d %H:%M',queued_at,'unixepoch','+8 hours'),text
         FROM broadcasts WHERE scope=?1 OR (?2 AND scope='')
         ORDER BY id LIMIT 10 OFFSET ?3",
    )?;
    for row in stmt.query_map(
        params![scope, thread.is_empty(), u64::from(page - 1) * 10],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )? {
        let (time, text) = row?;
        output.push_str(&format!("\n- {time} {text}"));
    }
    Ok(output)
}

fn next_batch(cfg: &Config) -> Result<Option<(i64, String)>> {
    let db = connect(cfg)?;
    let mut stmt = db.prepare("SELECT id,text FROM broadcasts ORDER BY id LIMIT 500")?;
    let mut rows = stmt.query([])?;
    let mut last = None;
    let mut text = String::new();
    let mut len = 0;
    while let Some(row) = rows.next()? {
        let content: String = row.get(1)?;
        let count = content.chars().count();
        if len != 0 {
            if len + 1 + count > 500 {
                break;
            }
            text.push('\n');
            len += 1;
        }
        text.push_str(&content);
        len += count;
        last = Some(row.get(0)?);
    }
    Ok(last.map(|id| (id, text)))
}

fn acknowledge(cfg: &Config, last: i64) -> Result<()> {
    connect(cfg)?.execute("DELETE FROM broadcasts WHERE id<=?1", [last])?;
    Ok(())
}

pub async fn maintain(cfg: &Config) {
    let mut last_focus = None;
    loop {
        let state = focus(cfg);
        if last_focus != Some(state) {
            tracing::info!(state = state.label(), "Mac 专注模式与播报收件箱状态");
            last_focus = Some(state);
        }
        let delay = if state.allows_speech() && path(cfg).is_file() {
            match flush(cfg).await {
                Ok(()) => 5,
                Err(error) => {
                    tracing::warn!(%error, "收件箱播报失败，保留待播报内容");
                    60
                }
            }
        } else {
            5
        };
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}

async fn flush(cfg: &Config) -> Result<()> {
    if next_batch(cfg)?.is_none() {
        return Ok(());
    }
    let Ok(lock) =
        tokio::time::timeout(Duration::from_secs(2), speech::audio_lock(cfg, true)).await
    else {
        return Ok(());
    };
    let _lock = lock?;
    if let Some((last, text)) = next_batch(cfg)?
        && speech::play_if_allowed(cfg, &text, Duration::from_secs(360)).await?
    {
        acknowledge(cfg, last)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn write_focus(path: &std::path::Path, mode: &str) -> Result<()> {
        fs::write(
            path,
            json!({"data":[{"storeAssertionRecords":[{"assertionDetails":{
                "assertionDetailsModeIdentifier": mode
            }}]}]})
            .to_string(),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn quiet_mode_queues_broadcasts_and_leaving_it_plays_them_together() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("mr-li-inbox-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir)?;
        let focus_file = dir.join("Assertions.json");
        let player = dir.join("speaker");
        let log = dir.join("audio");
        fs::write(&player, format!("#!/bin/sh\ncat >> '{}'\n", log.display()))?;
        fs::set_permissions(&player, fs::Permissions::from_mode(0o700))?;
        let mut cfg = Config::fixture();
        cfg.data_dir = dir.clone();
        cfg.focus_assertions = Some(focus_file.clone());
        cfg.speech_program = player.to_string_lossy().into();
        let private = json!(["/work", "owner", "private", ""]).to_string();
        let group = json!(["/work", "owner", "group", "thread"]).to_string();
        let scope = models::scope(&private)?;

        write_focus(&focus_file, "com.apple.sleep.sleep-mode")?;
        let speech::Delivery::Queued(first) =
            speech::speak(&cfg, "私人通知。", Some(&scope)).await?
        else {
            panic!("Sleep should queue the notification")
        };
        assert!(speech::ask_aloud(&cfg, "能听见吗？").await.is_err());
        speech::speak_wake(&cfg).await?;
        write_focus(&focus_file, "com.apple.donotdisturb.mode.default")?;
        let speech::Delivery::Queued(second) = speech::speak_news(&cfg, "系统新闻。").await?
        else {
            panic!("Do Not Disturb should queue the news")
        };
        assert!(!log.exists());
        assert!(contains(&cfg, first)? && contains(&cfg, second)?);
        let personal = command(&cfg, &private, "")?;
        assert!(personal.contains("私人通知。") && personal.contains("系统新闻。"));
        let group_view = command(&cfg, &group, "")?;
        assert!(!group_view.contains("私人通知。") && !group_view.contains("系统新闻。"));
        flush(&cfg).await?;
        assert!(!log.exists());

        write_focus(&focus_file, "com.apple.focus.work")?;
        let speaker = std::mem::replace(&mut cfg.speech_program, "/usr/bin/false".into());
        assert!(flush(&cfg).await.is_err());
        assert!(contains(&cfg, first)? && contains(&cfg, second)?);
        cfg.speech_program = speaker;
        flush(&cfg).await?;
        flush(&cfg).await?;
        assert_eq!(fs::read_to_string(log)?, "私人通知。\n系统新闻。");
        assert!(!contains(&cfg, first)? && !contains(&cfg, second)?);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn entering_sleep_stops_playback_and_queues_the_message() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("mr-li-inbox-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir)?;
        let focus_file = dir.join("Assertions.json");
        let player = dir.join("speaker");
        let started = dir.join("started");
        write_focus(&focus_file, "com.apple.focus.work")?;
        fs::write(
            &player,
            format!(
                "#!/bin/sh\ncat >/dev/null\ntouch '{}'\nexec /bin/sleep 30\n",
                started.display()
            ),
        )?;
        fs::set_permissions(&player, fs::Permissions::from_mode(0o700))?;
        let mut cfg = Config::fixture();
        cfg.data_dir = dir.clone();
        cfg.focus_assertions = Some(focus_file.clone());
        cfg.speech_program = player.to_string_lossy().into();
        let running = tokio::spawn({
            let cfg = cfg.clone();
            async move { speech::speak(&cfg, "测试消息", None).await }
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        write_focus(&focus_file, "com.apple.sleep.sleep-mode")?;
        let speech::Delivery::Queued(id) =
            tokio::time::timeout(Duration::from_secs(3), running).await???
        else {
            panic!("Entering Sleep should stop and queue the message")
        };
        assert!(contains(&cfg, id)?);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
