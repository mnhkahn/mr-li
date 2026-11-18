//! Durable notification publishing through Homework's existing self.notify.send tool.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Mutex};

pub struct Notices(Mutex<Connection>);
impl Notices {
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path.parent().context("通知目录缺失")?)?;
        let db = Connection::open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
   CREATE TABLE IF NOT EXISTS notices(id TEXT PRIMARY KEY,owner TEXT NOT NULL,source TEXT UNIQUE NOT NULL,device TEXT NOT NULL,body TEXT NOT NULL,created INTEGER NOT NULL,expires INTEGER NOT NULL,status TEXT NOT NULL DEFAULT 'pending',due INTEGER NOT NULL DEFAULT 0,error TEXT NOT NULL DEFAULT '');
   CREATE INDEX IF NOT EXISTS notices_pending ON notices(status,due,expires);
   CREATE INDEX IF NOT EXISTS notices_owner ON notices(owner,created DESC);")?;
        Ok(Self(Mutex::new(db)))
    }
    pub fn recover(&self) -> Result<()> {
        self.0.lock().unwrap().execute("UPDATE notices SET status='uncertain',error='服务重启，送达状态未知，未自动重发' WHERE status='sending'",[])?;
        Ok(())
    }
    pub fn publish(
        &self,
        owner: &str,
        source: &str,
        device: &str,
        body: &str,
        now: i64,
    ) -> Result<String> {
        ensure!(
            !body.trim().is_empty() && body.chars().count() <= 2000,
            "通知内容需为 1–2000 字"
        );
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        if let Some(id) = tx
            .query_row("SELECT id FROM notices WHERE source=?", [source], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
        {
            return Ok(id);
        }
        let id = format!(
            "notice-{}",
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, source.as_bytes()).simple()
        );
        let expires = ((now + 8 * 3600) / 86400 + 1) * 86400 - 8 * 3600;
        tx.execute(
            "UPDATE notices SET status='replaced' WHERE device=? AND status='pending'",
            [device],
        )?;
        tx.execute("INSERT INTO notices(id,owner,source,device,body,created,expires) VALUES(?,?,?,?,?,?,?)",params![id,owner,source,device,body,now,expires])?;
        tx.commit()?;
        Ok(id)
    }
    pub fn cancel(&self, owner: &str, id: &str) -> Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        let status = tx
            .query_row(
                "SELECT status FROM notices WHERE owner=? AND id=?",
                params![owner, id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .context("通知不存在，或不属于当前用户和聊天")?;
        ensure!(
            matches!(
                status.as_str(),
                "pending" | "cancelled" | "expired" | "replaced"
            ),
            "通知已发送或正在发送，不能远程撤回；可发送新通知替换，首页通知当天结束后自动消失"
        );
        if status == "pending" {
            tx.execute("UPDATE notices SET status='cancelled' WHERE id=?", [id])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn pending(&self, now: i64) -> Result<Vec<Value>> {
        let db = self.0.lock().unwrap();
        db.execute(
            "UPDATE notices SET status='expired' WHERE status='pending' AND expires<=?",
            [now],
        )?;
        let mut q=db.prepare("SELECT id,device,body FROM notices WHERE status='pending' AND due<=? ORDER BY created,rowid LIMIT 8")?;
        Ok(q.query_map([now],|r|Ok(json!({"id":r.get::<_,String>(0)?,"device":r.get::<_,String>(1)?,"body":r.get::<_,String>(2)?})))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn claim(&self, id: &str, now: i64) -> Result<bool> {
        Ok(self.0.lock().unwrap().execute("UPDATE notices SET status='sending',error='' WHERE id=? AND status='pending' AND expires>?",params![id,now])?>0)
    }
    pub fn finish(&self, id: &str, delivered: bool, error: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE notices SET status=?,error=? WHERE id=? AND status='sending'",
            params![if delivered { "delivered" } else { "uncertain" }, error, id],
        )?;
        Ok(())
    }
    pub fn retry(&self, device: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE notices SET due=0 WHERE device=? AND status='pending'",
            [device],
        )?;
        Ok(())
    }
    pub fn defer(&self, id: &str, error: &str, now: i64) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE notices SET due=?,error=? WHERE id=? AND status='pending'",
            params![now + 30, error, id],
        )?;
        Ok(())
    }
    pub fn list(&self, owner: &str, now: i64) -> Result<String> {
        let db = self.0.lock().unwrap();
        let mut q=db.prepare("SELECT id,body,expires,status,device,error FROM notices WHERE owner=? ORDER BY created DESC,rowid DESC LIMIT 10")?;
        let rows = q
            .query_map([owner], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.is_empty() {
            return Ok("本聊天暂无你的通知。".into());
        }
        let mut text = "本聊天你的最近通知（首页仅显示最新一条，当天结束后消失）：".to_owned();
        for (id, body, expires, state, device, error) in rows {
            let status = if expires <= now && matches!(state.as_str(), "pending" | "delivered") {
                "已过期"
            } else {
                match state.as_str() {
                    "pending" => "待送达",
                    "sending" => "发送中",
                    "delivered" => "已送达",
                    "cancelled" => "已取消",
                    "replaced" => "已被新通知替换",
                    "expired" => "已过期",
                    _ => "送达未确认",
                }
            };
            text.push_str(&format!(
                "\n{} · {}\n{}\n设备：{}{}",
                status,
                crate::commands::preview(&body, 80),
                id,
                device,
                if !error.is_empty() {
                    format!(" · {}", crate::commands::preview(&error, 80))
                } else {
                    String::new()
                }
            ));
        }
        Ok(text)
    }
}
pub struct Publish {
    pub device: Option<String>,
    pub body: String,
}
pub fn parse_send(args: &str) -> Result<Publish> {
    let mut rest = args.trim();
    if let Some(after) = rest.strip_prefix("send ") {
        rest = after.trim_start();
    }
    let mut device = None;
    if let Some(tail) = rest.strip_prefix("--device ") {
        let (id, body) = tail
            .trim_start()
            .split_once(char::is_whitespace)
            .context("用法：/notify send --device <设备ID> <内容>")?;
        device = Some(id.into());
        rest = body.trim_start();
    }
    ensure!(
        !rest.is_empty() && rest.chars().count() <= 2000,
        "通知内容需为 1–2000 字"
    );
    Ok(Publish {
        device,
        body: rest.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Sandbox(std::path::PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mr-li-notices-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn offline_queue_is_durable_idempotent_and_expires() {
        let dir = Sandbox::new();
        let path = dir.0.join("notices.sqlite3");
        let store = Notices::open(&path).unwrap();
        let time = 1795000000;
        let id = store
            .publish("owner", "request", "tablet", "明天带练习册", time)
            .unwrap();
        assert_eq!(
            store
                .publish("owner", "request", "tablet", "明天带练习册", time)
                .unwrap(),
            id
        );
        store.defer(&id, "平板离线", time).unwrap();
        assert!(store.pending(time).unwrap().is_empty());
        drop(store);
        let store = Notices::open(&path).unwrap();
        store.recover().unwrap();
        store.retry("tablet").unwrap();
        assert_eq!(store.pending(time).unwrap()[0]["id"], id);
        assert!(store.pending(time + 86400).unwrap().is_empty());
        assert!(
            store
                .list("owner", time + 86400)
                .unwrap()
                .contains("已过期")
        );
    }
    #[test]
    fn replacement_cancellation_and_owner_checks_are_safe() {
        let dir = Sandbox::new();
        let store = Notices::open(&dir.0.join("db")).unwrap();
        let time = 1795000000;
        let old = store
            .publish("owner", "old", "tablet", "旧通知", time)
            .unwrap();
        let id = store
            .publish("owner", "new", "tablet", "新通知", time)
            .unwrap();
        let pending = store.pending(time).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["id"], id);
        assert!(
            store
                .list("owner", time)
                .unwrap()
                .contains("已被新通知替换")
        );
        assert!(!store.list("other", time).unwrap().contains("新通知"));
        assert!(store.cancel("other", &id).is_err());
        store.cancel("owner", &old).unwrap();
        store.cancel("owner", &id).unwrap();
        assert!(!store.claim(&id, time).unwrap());
        assert!(store.pending(time).unwrap().is_empty());
    }
    #[test]
    fn sending_and_delivered_notices_cannot_be_falsely_retracted_or_resent() {
        let dir = Sandbox::new();
        let path = dir.0.join("db");
        let store = Notices::open(&path).unwrap();
        let time = 1795000000;
        let id = store
            .publish("owner", "one", "tablet", "通知", time)
            .unwrap();
        assert!(store.claim(&id, time).unwrap());
        assert!(store.cancel("owner", &id).is_err());
        drop(store);
        let store = Notices::open(&path).unwrap();
        store.recover().unwrap();
        assert!(store.pending(time).unwrap().is_empty());
        assert!(store.list("owner", time).unwrap().contains("送达未确认"));
        let id = store
            .publish("owner", "two", "tablet", "第二条", time)
            .unwrap();
        assert!(store.claim(&id, time).unwrap());
        store.finish(&id, true, "").unwrap();
        assert!(store.cancel("owner", &id).is_err());
        assert!(store.list("owner", time).unwrap().contains("已送达"));
    }
    #[test]
    fn command_parser_preserves_text_and_supports_a_specific_tablet() {
        let parsed = parse_send("send --device tablet-1 明天带书\n记得水杯").unwrap();
        assert_eq!(parsed.device.as_deref(), Some("tablet-1"));
        assert_eq!(parsed.body, "明天带书\n记得水杯");
        assert_eq!(parse_send("普通通知").unwrap().body, "普通通知");
        assert!(parse_send("").is_err());
        assert!(parse_send(&"字".repeat(2001)).is_err());
    }
}
