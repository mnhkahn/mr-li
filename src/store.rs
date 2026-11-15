use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, sync::Mutex};
#[derive(Clone, Debug)]
pub struct Job {
    pub id: String,
    pub key: String,
    pub message_id: String,
    pub text: String,
    pub result: String,
    pub sent: usize,
}
pub struct Store(Mutex<Connection>);
pub struct ModelCard {
    pub origin_key: String,
    pub message_id: String,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let c = Connection::open(path)?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS sessions (key TEXT PRIMARY KEY, id TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS jobs (id TEXT PRIMARY KEY, key TEXT NOT NULL, message_id TEXT NOT NULL,
          text TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued', result TEXT NOT NULL DEFAULT '',
          sent INTEGER NOT NULL DEFAULT 0, created INTEGER NOT NULL DEFAULT (unixepoch()));
          CREATE TABLE IF NOT EXISTS delivery_retry (id TEXT PRIMARY KEY, attempts INTEGER NOT NULL, due INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS selected_models (scope TEXT PRIMARY KEY, model TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS model_cards (token TEXT PRIMARY KEY, origin_key TEXT NOT NULL,
          message_id TEXT NOT NULL DEFAULT '', expires INTEGER NOT NULL DEFAULT (unixepoch()+1800));
          CREATE TABLE IF NOT EXISTS runtime_meta (key TEXT PRIMARY KEY,value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS legacy_sessions (key TEXT NOT NULL,id TEXT NOT NULL,archived_at INTEGER DEFAULT (unixepoch()),PRIMARY KEY(key,id));
          CREATE INDEX IF NOT EXISTS job_status ON jobs(status, created);")?;
        let has_kind = c
            .prepare("PRAGMA table_info(jobs)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|s| s == "output_kind");
        if !has_kind {
            c.execute(
                "ALTER TABLE jobs ADD COLUMN output_kind TEXT NOT NULL DEFAULT 'text'",
                [],
            )?;
        }
        Ok(Self(Mutex::new(c)))
    }
    pub fn use_v2_sessions(&self) -> Result<()> {
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let current: Option<String> = tx
            .query_row(
                "SELECT value FROM runtime_meta WHERE key='backend'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if current.as_deref() != Some("v2.3") {
            tx.execute(
                "INSERT OR IGNORE INTO legacy_sessions(key,id) SELECT key,id FROM sessions",
                [],
            )?;
            tx.execute("DELETE FROM sessions", [])?;
            tx.execute("INSERT INTO runtime_meta VALUES ('backend','v2.3') ON CONFLICT(key) DO UPDATE SET value=excluded.value",[])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn insert(&self, j: &Job) -> Result<bool> {
        let c = self.0.lock().unwrap();
        let exists: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1)",
            [&j.id],
            |r| r.get(0),
        )?;
        if exists {
            return Ok(false);
        }
        let pending: i64 = c.query_row(
            "SELECT count(*) FROM jobs WHERE status != 'done'",
            [],
            |r| r.get(0),
        )?;
        anyhow::ensure!(pending < 500, "任务队列已满，等待飞书重试");
        Ok(c.execute(
            "INSERT OR IGNORE INTO jobs(id,key,message_id,text) VALUES (?1,?2,?3,?4)",
            params![j.id, j.key, j.message_id, j.text],
        )? > 0)
    }
    pub fn jobs(&self, status: &str) -> Result<Vec<Job>> {
        let c = self.0.lock().unwrap();
        let mut s = c.prepare("SELECT id,key,message_id,text,result,sent FROM jobs WHERE status=?1 AND (?1 != 'replying' OR NOT EXISTS (SELECT 1 FROM delivery_retry WHERE delivery_retry.id=jobs.id AND due>unixepoch())) ORDER BY rowid LIMIT 500")?;
        let rows = s.query_map([status], |r| {
            Ok(Job {
                id: r.get(0)?,
                key: r.get(1)?,
                message_id: r.get(2)?,
                text: r.get(3)?,
                result: r.get(4)?,
                sent: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn status(&self, id: &str, status: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("UPDATE jobs SET status=?2 WHERE id=?1", params![id, status])?;
        Ok(())
    }
    pub fn finish(&self, id: &str, text: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE jobs SET status='replying',result=?2,output_kind='text' WHERE id=?1",
            params![id, text],
        )?;
        Ok(())
    }
    pub fn finish_output(&self, id: &str, kind: &str, value: &serde_json::Value) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE jobs SET status='replying',result=?2,output_kind=?3 WHERE id=?1",
            params![id, value.to_string(), kind],
        )?;
        Ok(())
    }
    pub fn output_kind(&self, id: &str) -> Result<String> {
        Ok(self.0.lock().unwrap().query_row(
            "SELECT output_kind FROM jobs WHERE id=?1",
            [id],
            |r| r.get(0),
        )?)
    }
    pub fn model(&self, scope: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT model FROM selected_models WHERE scope=?1",
                [scope],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn register_model_card(&self, token: &str, key: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT INTO model_cards(token,origin_key) VALUES (?1,?2)",
            params![token, key],
        )?;
        Ok(())
    }
    pub fn model_card(&self, token: &str) -> Result<Option<ModelCard>> {
        Ok(self.0.lock().unwrap().query_row("SELECT origin_key,message_id FROM model_cards WHERE token=?1 AND expires>unixepoch()",
            [token],|r|Ok(ModelCard{origin_key:r.get(0)?,message_id:r.get(1)?})).optional()?)
    }
    pub fn attach_model_card(&self, token: &str, message: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE model_cards SET message_id=?2 WHERE token=?1",
            params![token, message],
        )?;
        Ok(())
    }
    pub fn switch_model_and_finish(
        &self,
        id: &str,
        scope: &str,
        model: &str,
        card: &serde_json::Value,
    ) -> Result<()> {
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("INSERT INTO selected_models(scope,model) VALUES (?1,?2) ON CONFLICT(scope) DO UPDATE SET model=excluded.model",params![scope,model])?;
        tx.execute("UPDATE jobs SET status='replying',result=?2,output_kind='model_card_update' WHERE id=?1",params![id,card.to_string()])?;
        tx.commit()?;
        Ok(())
    }
    pub fn sent(&self, id: &str, sent: usize) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("UPDATE jobs SET sent=?2 WHERE id=?1", params![id, sent])?;
        Ok(())
    }
    pub fn retry_delivery(&self, id: &str) -> Result<()> {
        self.0.lock().unwrap().execute("INSERT INTO delivery_retry VALUES (?1,1,unixepoch()+5) ON CONFLICT(id) DO UPDATE SET attempts=attempts+1,due=unixepoch()+min(300,5*(1 << min(attempts,6)))", [id])?;
        Ok(())
    }
    pub fn session(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .query_row("SELECT id FROM sessions WHERE key=?1", [key], |r| r.get(0))
            .optional()?)
    }
    pub fn set_session(&self, key: &str, id: &str) -> Result<()> {
        self.0.lock().unwrap().execute("INSERT INTO sessions(key,id) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET id=excluded.id",params![key,id])?;
        Ok(())
    }
    pub fn reset(&self, key: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("DELETE FROM sessions WHERE key=?1", [key])?;
        Ok(())
    }
    pub fn prune(&self) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("DELETE FROM model_cards WHERE expires<unixepoch()", [])?;
        self.0.lock().unwrap().execute(
            "DELETE FROM jobs WHERE status='done' AND created < unixepoch()-2592000",
            [],
        )?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn v1_session_maps_are_archived_once_without_losing_model_choices() -> Result<()> {
        let s = Store::open(Path::new(":memory:"))?;
        s.set_session("key", "ses_v1")?;
        s.0.lock().unwrap().execute(
            "INSERT INTO selected_models VALUES ('scope','opencode/free')",
            [],
        )?;
        s.use_v2_sessions()?;
        assert!(s.session("key")?.is_none());
        assert_eq!(s.model("scope")?.as_deref(), Some("opencode/free"));
        s.set_session("key", "ses_v2")?;
        s.use_v2_sessions()?;
        assert_eq!(s.session("key")?.as_deref(), Some("ses_v2"));
        assert_eq!(
            s.0.lock().unwrap().query_row(
                "SELECT id FROM legacy_sessions WHERE key='key'",
                [],
                |r| r.get::<_, String>(0)
            )?,
            "ses_v1"
        );
        Ok(())
    }
    #[test]
    fn model_selection_and_card_outbox_survive_restart() -> Result<()> {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&dir)?;
        let path = dir.join("old.db");
        {
            let c = Connection::open(&path)?;
            c.execute_batch("CREATE TABLE jobs(id TEXT PRIMARY KEY,key TEXT,message_id TEXT,text TEXT,status TEXT DEFAULT 'queued',result TEXT DEFAULT '',sent INTEGER DEFAULT 0,created INTEGER DEFAULT (unixepoch())); INSERT INTO jobs(id,key,message_id,text) VALUES('old','key','om_old','hello');")?;
        }
        {
            let s = Store::open(&path)?;
            assert_eq!(s.output_kind("old")?, "text");
            s.register_model_card("nonce", "original-key")?;
            s.attach_model_card("nonce", "om_card")?;
            s.switch_model_and_finish(
                "old",
                "scope",
                "opencode/free",
                &serde_json::json!({"selected":"free"}),
            )?;
        }
        {
            let s = Store::open(&path)?;
            assert_eq!(s.model("scope")?.as_deref(), Some("opencode/free"));
            assert_eq!(s.model_card("nonce")?.unwrap().message_id, "om_card");
            assert_eq!(s.output_kind("old")?, "model_card_update");
            assert_eq!(s.jobs("replying")?.len(), 1);
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn durable_dedup_and_outbox() -> Result<()> {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&dir)?;
        let path = dir.join("test.db");
        let j = Job {
            id: "om_1".into(),
            key: "k".into(),
            message_id: "om_1".into(),
            text: "hello".into(),
            result: String::new(),
            sent: 0,
        };
        {
            let s = Store::open(&path)?;
            assert!(s.insert(&j)?);
            assert!(!s.insert(&j)?);
            s.set_session("k", "ses_1")?;
            s.finish(&j.id, "result")?;
            s.sent(&j.id, 1)?;
        }
        {
            let s = Store::open(&path)?;
            assert_eq!(s.session("k")?.as_deref(), Some("ses_1"));
            let jobs = s.jobs("replying")?;
            assert_eq!(jobs[0].sent, 1);
            assert_eq!(jobs[0].result, "result");
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
