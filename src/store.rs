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
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let c = Connection::open(path)?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS sessions (key TEXT PRIMARY KEY, id TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS jobs (id TEXT PRIMARY KEY, key TEXT NOT NULL, message_id TEXT NOT NULL,
          text TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued', result TEXT NOT NULL DEFAULT '',
          sent INTEGER NOT NULL DEFAULT 0, created INTEGER NOT NULL DEFAULT (unixepoch()));
          CREATE TABLE IF NOT EXISTS delivery_retry (id TEXT PRIMARY KEY, attempts INTEGER NOT NULL, due INTEGER NOT NULL);
          CREATE INDEX IF NOT EXISTS job_status ON jobs(status, created);")?;
        Ok(Self(Mutex::new(c)))
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
            "UPDATE jobs SET status='replying',result=?2 WHERE id=?1",
            params![id, text],
        )?;
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
