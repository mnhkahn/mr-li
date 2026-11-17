use crate::posture::Assessment;
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
pub struct FormCard {
    pub origin_key: String,
    pub session: String,
    pub message_id: String,
    pub state: serde_json::Value,
}
pub struct ProcessingReaction {
    pub job_id: String,
    pub message_id: String,
    pub reaction_id: String,
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
          CREATE TABLE IF NOT EXISTS form_cards (token TEXT PRIMARY KEY,origin_key TEXT NOT NULL,session TEXT NOT NULL,message_id TEXT NOT NULL DEFAULT '',state TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS runtime_meta (key TEXT PRIMARY KEY,value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS processing_reactions (job_id TEXT PRIMARY KEY,message_id TEXT NOT NULL,reaction_id TEXT NOT NULL,due INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS posture_watch (scope TEXT PRIMARY KEY,origin_key TEXT NOT NULL,message_id TEXT NOT NULL,interval_seconds INTEGER NOT NULL,enabled INTEGER NOT NULL DEFAULT 1,next_due INTEGER NOT NULL DEFAULT (unixepoch()),last_note TEXT NOT NULL DEFAULT '',last_at INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS posture_report_state (scope TEXT PRIMARY KEY,started INTEGER NOT NULL,next_due INTEGER NOT NULL,bad INTEGER NOT NULL DEFAULT 0,clear INTEGER NOT NULL DEFAULT 0,unknown INTEGER NOT NULL DEFAULT 0,speech_failures INTEGER NOT NULL DEFAULT 0,last_speech_error TEXT NOT NULL DEFAULT '',last_assessment INTEGER NOT NULL DEFAULT 2,away INTEGER NOT NULL DEFAULT 0,observe INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS posture_samples (job_id TEXT PRIMARY KEY,scope TEXT NOT NULL,recorded_at INTEGER NOT NULL,assessment INTEGER NOT NULL,speech_failed INTEGER NOT NULL DEFAULT 0);
          CREATE INDEX IF NOT EXISTS posture_samples_scope ON posture_samples(scope,recorded_at);
          INSERT OR IGNORE INTO posture_report_state(scope,started,next_due) SELECT scope,unixepoch(),unixepoch()+600 FROM posture_watch;
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
        let columns = c
            .prepare("PRAGMA table_info(posture_report_state)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for name in ["away", "observe"] {
            if !columns.iter().any(|c| c == name) {
                c.execute(&format!("ALTER TABLE posture_report_state ADD COLUMN {name} INTEGER NOT NULL DEFAULT 0"),[])?;
            }
        }
        c.execute("UPDATE jobs SET output_kind='posture_report' WHERE id LIKE 'posture-report:%' AND status='replying' AND sent=0 AND output_kind='text'",[])?;
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
    pub fn start_posture_watch(&self, j: &Job, seconds: u64) -> Result<()> {
        self.start_posture_watch_at(j, seconds, crate::posture::now_epoch())
    }
    fn start_posture_watch_at(&self, j: &Job, seconds: u64, now: u64) -> Result<()> {
        anyhow::ensure!((30..=3600).contains(&seconds), "定时间隔支持 30–3600 秒");
        let scope = crate::models::scope(&j.key)?;
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("INSERT INTO posture_watch(scope,origin_key,message_id,interval_seconds) VALUES (?1,?2,?3,?4) ON CONFLICT(scope) DO UPDATE SET origin_key=excluded.origin_key,message_id=excluded.message_id,interval_seconds=excluded.interval_seconds,enabled=1,next_due=?5",params![scope,j.key,j.message_id,seconds,now])?;
        tx.execute(
            "UPDATE posture_watch SET next_due=?2 WHERE scope=?1",
            params![scope, now],
        )?;
        tx.execute("INSERT INTO posture_report_state(scope,started,next_due) VALUES (?1,?2,?3) ON CONFLICT(scope) DO UPDATE SET started=excluded.started,next_due=excluded.next_due,bad=0,clear=0,unknown=0,away=0,observe=0,speech_failures=0,last_speech_error='',last_assessment=2",params![scope,now,crate::posture::next_report_at(now)])?;
        tx.execute("DELETE FROM posture_samples WHERE scope=?1", [scope])?;
        tx.commit()?;
        Ok(())
    }
    pub fn posture_enabled(&self, key: &str) -> Result<bool> {
        if !crate::posture::schedule_active() {
            return Ok(false);
        }
        Ok(self.0.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM posture_watch WHERE origin_key=?1 AND enabled=1)",
            [key],
            |r| r.get(0),
        )?)
    }
    pub fn stop_posture_watch(&self, scope: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("UPDATE posture_watch SET enabled=0 WHERE scope=?1", [scope])?;
        for j in self
            .jobs("queued")?
            .into_iter()
            .chain(self.jobs("replying")?)
        {
            if (j.id.starts_with("posture-sample:") || j.id.starts_with("posture-report:"))
                && crate::models::scope(&j.key)? == scope
            {
                self.status(&j.id, "done")?;
            }
        }
        Ok(())
    }
    pub fn posture_watch_status(&self, scope: &str) -> Result<String> {
        let c = self.0.lock().unwrap();
        let row=c.query_row("SELECT enabled,interval_seconds,last_note,last_at FROM posture_watch WHERE scope=?1",[scope],|r|Ok((r.get::<_,bool>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?,r.get::<_,u64>(3)?))).optional()?;
        let Some((enabled, interval, note, at)) = row else {
            return Ok("尚未启动定时姿态试验。用 /posture 查看单次效果，或 /posture start 启用每日 17:00–21:30 每分钟检查，仅异常播报，飞书每 10 分钟汇总。".into());
        };
        let age = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            .saturating_sub(at);
        Ok(format!(
            "定时坐姿检查：{}；每日北京时间 17:00–21:30，每 {interval} 秒检查，仅低头/歪头播报，飞书每 10 分钟汇总\n最近检查：{}\n{}",
            if !enabled {
                "已停用每日计划"
            } else if crate::posture::schedule_active() {
                "运行中"
            } else {
                "计划已启用，等待下次 17:00"
            },
            if at == 0 {
                "尚未检查".into()
            } else {
                format!("{age} 秒前")
            },
            if note.is_empty() {
                "仅低头或歪头时播报；其余静音并记入汇总。"
            } else {
                &note
            }
        ))
    }
    pub fn queue_due_posture(&self) -> Result<()> {
        self.queue_due_posture_at(crate::posture::now_epoch())
    }
    fn queue_due_posture_at(&self, now: u64) -> Result<()> {
        if !crate::posture::schedule_active_at(now) {
            self.0.lock().unwrap().execute("UPDATE jobs SET status='done' WHERE id LIKE 'posture-sample:%' AND status='queued'",[])?;
            return Ok(());
        }
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        // Start a new day's reporting window without counting the overnight pause.
        let (start, _) = crate::posture::schedule_bounds_at(now);
        tx.execute("UPDATE posture_report_state SET started=?1,next_due=?2 WHERE started<?3 AND bad+clear+unknown+away+observe=0",params![now,crate::posture::next_report_at(now),start])?;
        // One camera job at a time, with no backlog when capture is slow or offline.
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE id LIKE 'posture-sample:%' AND status!='done')",
            [],
            |r| r.get(0),
        )?;
        if !pending {
            let row=tx.query_row("SELECT scope,origin_key,message_id,interval_seconds FROM posture_watch WHERE enabled=1 AND next_due<=?1 ORDER BY next_due LIMIT 1",[now],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,u64>(3)?))).optional()?;
            if let Some((scope, key, message, seconds)) = row {
                tx.execute(
                    "INSERT INTO jobs(id,key,message_id,text) VALUES (?1,?2,?3,'/posture')",
                    params![
                        format!("posture-sample:{}", uuid::Uuid::new_v4().simple()),
                        key,
                        message
                    ],
                )?;
                tx.execute(
                    "UPDATE posture_watch SET next_due=?3+?2 WHERE scope=?1",
                    params![scope, seconds, now],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn finish_posture_sample(&self, j: &Job, note: &str) -> Result<()> {
        self.finish_posture_observation(j, note, Assessment::Unknown, None)
    }
    pub fn finish_posture_observation(
        &self,
        j: &Job,
        note: &str,
        assessment: Assessment,
        speech_error: Option<&str>,
    ) -> Result<()> {
        let scope = crate::models::scope(&j.key)?;
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let enabled: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM posture_watch WHERE origin_key=?1 AND enabled=1)",
            [&j.key],
            |r| r.get(0),
        )?;
        let code = match assessment {
            Assessment::NeedsReminder => 0,
            Assessment::NoObviousIssue => 1,
            Assessment::Unknown => 2,
            Assessment::Away => 3,
            Assessment::Observe => 4,
        };
        let fresh = if enabled {
            tx.execute("INSERT OR IGNORE INTO posture_samples(job_id,scope,recorded_at,assessment,speech_failed) VALUES (?1,?2,unixepoch(),?3,?4)",params![j.id,scope,code,speech_error.is_some()])?>0
        } else {
            false
        };
        if fresh {
            tx.execute("UPDATE posture_report_state SET bad=bad+?2,clear=clear+?3,unknown=unknown+?4,speech_failures=speech_failures+?5,last_speech_error=CASE WHEN ?5 THEN ?6 ELSE last_speech_error END,last_assessment=?7,away=away+?8,observe=observe+?9 WHERE scope=?1",params![scope,assessment==Assessment::NeedsReminder,assessment==Assessment::NoObviousIssue,assessment==Assessment::Unknown,speech_error.is_some(),speech_error.unwrap_or(""),match assessment {Assessment::NeedsReminder=>0,Assessment::NoObviousIssue=>1,Assessment::Unknown=>2,Assessment::Away=>3,Assessment::Observe=>4},assessment==Assessment::Away,assessment==Assessment::Observe])?;
            tx.execute("UPDATE posture_watch SET last_note=?2,last_at=unixepoch(),next_due=CASE WHEN next_due<unixepoch() THEN unixepoch()+interval_seconds ELSE next_due END WHERE origin_key=?1 AND enabled=1",params![j.key,note])?;
        }
        tx.execute(
            "UPDATE jobs SET status='done',result=?2,output_kind='text' WHERE id=?1",
            params![j.id, note],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Queue one durable summary per enabled scope every ten minutes, including unknowns.
    /// Capture and speech failure never prevent this independent timer from reporting.
    pub fn queue_due_posture_reports(&self) -> Result<()> {
        self.queue_due_posture_reports_at(crate::posture::now_epoch())
    }
    fn queue_due_posture_reports_at(&self, now: u64) -> Result<()> {
        let active = crate::posture::schedule_active_at(now);
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let row=tx.query_row("SELECT r.scope,w.origin_key,w.message_id,r.last_assessment,r.bad,r.clear,r.unknown,r.away,r.observe,r.speech_failures,r.last_speech_error,max(1,((CASE WHEN ?2 THEN ?1 ELSE min(?1,max(w.last_at,r.started)) END)-r.started+59)/60) FROM posture_report_state r JOIN posture_watch w ON w.scope=r.scope WHERE w.enabled=1 AND r.bad+r.clear+r.unknown+r.away+r.observe>0 AND (r.next_due<=?1 OR NOT ?2) AND NOT EXISTS(SELECT 1 FROM jobs j WHERE j.key=w.origin_key AND j.id LIKE 'posture-report:%' AND j.status!='done') ORDER BY r.next_due LIMIT 1",params![now,active],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,u64>(4)?,r.get::<_,u64>(5)?,r.get::<_,u64>(6)?,r.get::<_,u64>(7)?,r.get::<_,u64>(8)?,r.get::<_,u64>(9)?,r.get::<_,String>(10)?,r.get::<_,u64>(11)?))).optional()?;
        if let Some((
            scope,
            key,
            message,
            last_assessment,
            bad,
            clear,
            unknown,
            away,
            observe,
            speech_failures,
            speech_error,
            minutes,
        )) = row
        {
            let total = bad + clear + unknown + away + observe;
            let mut note = format!(
                "坐姿汇总（过去约 {minutes} 分钟）\n检查 {total} 次：疑似低头或歪头 {bad} 次；未触发提醒 {clear} 次；离座/非坐姿画面 {away} 次；临界待观察 {observe} 次；无法判断 {unknown} 次。\n这些是间隔采样次数，不代表持续时长。\n最近结果：{}",
                if total == 0 {
                    "尚未完成采样"
                } else {
                    match last_assessment {
                        0 => "疑似低头或歪头，请抬头坐正",
                        1 => "未触发低头或歪头提醒",
                        3 => "未检测到学习位内可判断的坐姿目标",
                        4 => "已识别头肩指标，当前处于临界待观察区间",
                        _ => "无法判断坐姿",
                    }
                }
            );
            let mut details = {
                let mut q=tx.prepare("SELECT strftime('%H:%M',recorded_at,'unixepoch','+8 hours'),assessment,speech_failed FROM posture_samples WHERE scope=?1 ORDER BY recorded_at DESC,rowid DESC LIMIT 60")?;
                q.query_map([&scope], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, bool>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
            };
            details.reverse();
            if !details.is_empty() {
                note.push_str("\n\n每分钟记录（北京时间）：");
                if total > details.len() as u64 {
                    note.push_str(&format!(
                        "\n本批共 {total} 条，仅展开最近 {} 条。",
                        details.len()
                    ));
                }
                for (time, assessment, speech_failed) in details {
                    let label = match assessment {
                        0 => "疑似低头或歪头",
                        1 => "未触发提醒",
                        3 => "离座/非坐姿画面",
                        4 => "临界待观察",
                        _ => "无法判断",
                    };
                    note.push_str(&format!(
                        "\n{time} · {label}{}",
                        if speech_failed {
                            "（语音失败）"
                        } else {
                            ""
                        }
                    ));
                }
            }
            if speech_failures > 0 {
                note.push_str(&format!(
                    "\n语音播报失败 {speech_failures} 次，最近原因：{speech_error}"
                ));
            }
            tx.execute("INSERT INTO jobs(id,key,message_id,text,status,result,output_kind) VALUES (?1,?2,?3,'/posture report','replying',?4,'posture_report')",params![format!("posture-report:{}",uuid::Uuid::new_v4().simple()),key,message,note])?;
            tx.execute("UPDATE posture_report_state SET started=?2,next_due=?3,bad=0,clear=0,unknown=0,away=0,observe=0,speech_failures=0,last_speech_error='' WHERE scope=?1",params![scope,now,crate::posture::next_report_at(now)])?;
            tx.execute("DELETE FROM posture_samples WHERE scope=?1", [scope])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn save_processing_reaction(&self, j: &Job, reaction: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT INTO processing_reactions(job_id,message_id,reaction_id) VALUES (?1,?2,?3)",
            params![j.id, j.message_id, reaction],
        )?;
        Ok(())
    }
    pub fn completed_reactions(&self) -> Result<Vec<ProcessingReaction>> {
        let c = self.0.lock().unwrap();
        let mut s = c.prepare("SELECT r.job_id,r.message_id,r.reaction_id FROM processing_reactions r LEFT JOIN jobs j ON j.id=r.job_id WHERE (j.status='done' OR j.id IS NULL) AND r.due<=unixepoch() ORDER BY r.rowid LIMIT 1")?;
        Ok(s.query_map([], |r| {
            Ok(ProcessingReaction {
                job_id: r.get(0)?,
                message_id: r.get(1)?,
                reaction_id: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn processing_reaction_removed(&self, id: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("DELETE FROM processing_reactions WHERE job_id=?1", [id])?;
        Ok(())
    }
    pub fn retry_reaction_cleanup(&self, id: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE processing_reactions SET due=unixepoch()+30 WHERE job_id=?1",
            [id],
        )?;
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
    pub fn save_form_card(
        &self,
        token: &str,
        key: &str,
        session: &str,
        state: &serde_json::Value,
    ) -> Result<()> {
        self.0.lock().unwrap().execute("INSERT INTO form_cards(token,origin_key,session,state) VALUES (?1,?2,?3,?4) ON CONFLICT(token) DO UPDATE SET state=excluded.state",params![token,key,session,state.to_string()])?;
        Ok(())
    }
    pub fn open_form_tokens(&self) -> Result<Vec<String>> {
        let c = self.0.lock().unwrap();
        let mut query =
            c.prepare("SELECT token FROM form_cards WHERE json_extract(state,'$.done') IS NOT 1")?;
        Ok(query
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn form_card(&self, token: &str) -> Result<Option<FormCard>> {
        let row = self
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT origin_key,session,message_id,state FROM form_cards WHERE token=?1",
                [token],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(origin_key, session, message_id, state)| {
            Ok(FormCard {
                origin_key,
                session,
                message_id,
                state: serde_json::from_str(&state)?,
            })
        })
        .transpose()
    }
    pub fn finish_form_card(
        &self,
        job: &str,
        token: &str,
        state: &serde_json::Value,
    ) -> Result<()> {
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE form_cards SET state=?2 WHERE token=?1",
            params![token, state.to_string()],
        )?;
        tx.execute("UPDATE jobs SET status='replying',output_kind='form_card_update',result=?2 WHERE id=?1",params![job,serde_json::json!({"token":token}).to_string()])?;
        tx.commit()?;
        Ok(())
    }
    pub fn attach_form_card(&self, token: &str, message: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE form_cards SET message_id=?2 WHERE token=?1",
            params![token, message],
        )?;
        Ok(())
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
    fn active_time() -> u64 {
        static TIME: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        *TIME.get_or_init(|| {
            let now = crate::posture::now_epoch();
            if crate::posture::schedule_active_at(now) {
                now
            } else {
                crate::posture::next_start_at(now)
            }
        })
    }
    #[test]
    fn daily_watch_stops_at_close_and_resumes_without_empty_night_reports() -> Result<()> {
        let s = Store::open(Path::new(":memory:"))?;
        let j = Job {
            id: "daily-start".into(),
            key: serde_json::json!(["workspace", "owner", "chat", "thread"]).to_string(),
            message_id: "om_start".into(),
            text: "/posture start".into(),
            result: String::new(),
            sent: 0,
        };
        // Keep the simulated next day ahead of the wall-clock timestamp used
        // when finish_posture_sample updates next_due.
        let (start, _) = crate::posture::schedule_bounds_at(active_time());
        let end = start + 16200;
        s.start_posture_watch_at(&j, 60, start - 60)?;
        s.queue_due_posture_at(start - 1)?;
        assert!(s.jobs("queued")?.is_empty());
        s.queue_due_posture_at(start)?;
        let sample = s.jobs("queued")?.remove(0);
        s.finish_posture_sample(&sample, "无法判断，静音")?;
        s.queue_due_posture_reports_at(start + 599)?;
        assert!(s.jobs("replying")?.is_empty());
        s.queue_due_posture_reports_at(end)?;
        let report = s.jobs("replying")?.remove(0);
        assert!(report.result.contains("检查 1 次"));
        s.status(&report.id, "done")?;
        s.queue_due_posture_at(end)?;
        s.queue_due_posture_reports_at(end + 3600)?;
        s.queue_due_posture_at(start + 86400 - 1)?;
        assert!(s.jobs("queued")?.is_empty());
        assert!(s.jobs("replying")?.is_empty());
        s.queue_due_posture_at(start + 86400)?;
        assert_eq!(s.jobs("queued")?.len(), 1);
        let c = s.0.lock().unwrap();
        assert_eq!(
            c.query_row("SELECT enabled FROM posture_watch", [], |r| r
                .get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            c.query_row("SELECT next_due FROM posture_report_state", [], |r| r
                .get::<_, u64>(0))?,
            start + 86400 + 600
        );
        Ok(())
    }
    #[test]
    fn periodic_pose_samples_are_silent_bounded_and_stoppable() -> Result<()> {
        let s = Store::open(Path::new(":memory:"))?;
        let j = Job {
            id: "om_start".into(),
            key: serde_json::json!(["workspace", "ou_owner", "oc_chat", "thread"]).to_string(),
            message_id: "om_start".into(),
            text: "/posture start".into(),
            result: String::new(),
            sent: 0,
        };
        s.start_posture_watch_at(&j, 30, active_time())?;
        s.queue_due_posture_at(active_time())?;
        let samples = s.jobs("queued")?;
        assert_eq!(samples.len(), 1);
        s.queue_due_posture_at(active_time())?;
        assert_eq!(s.jobs("queued")?.len(), 1);
        s.finish_posture_sample(&samples[0], "头肩点不足，无法判断")?;
        assert!(s.jobs("replying")?.is_empty());
        assert!(
            s.posture_watch_status(&crate::models::scope(&j.key)?)?
                .contains("无法判断")
        );
        s.0.lock()
            .unwrap()
            .execute("UPDATE posture_watch SET next_due=0", [])?;
        s.queue_due_posture_at(active_time())?;
        assert_eq!(s.jobs("queued")?.len(), 1);
        s.stop_posture_watch(&crate::models::scope(&j.key)?)?;
        assert!(s.jobs("queued")?.is_empty());
        s.queue_due_posture_at(active_time())?;
        assert!(s.jobs("queued")?.is_empty());
        assert!(s.start_posture_watch(&j, 1).is_err());
        Ok(())
    }
    #[test]
    fn posture_summaries_count_unknowns_and_errors_and_stop_cancels_delivery() -> Result<()> {
        let s = Store::open(Path::new(":memory:"))?;
        let j = Job {
            id: "start".into(),
            key: serde_json::json!(["workspace", "owner", "chat", "thread"]).to_string(),
            message_id: "om_start".into(),
            text: "/posture start".into(),
            result: String::new(),
            sent: 0,
        };
        s.start_posture_watch_at(&j, 60, active_time())?;
        for assessment in [
            Assessment::NeedsReminder,
            Assessment::NoObviousIssue,
            Assessment::Unknown,
            Assessment::Away,
            Assessment::Observe,
        ] {
            s.0.lock()
                .unwrap()
                .execute("UPDATE posture_watch SET next_due=0", [])?;
            s.queue_due_posture_at(active_time())?;
            let a = s.jobs("queued")?.remove(0);
            s.finish_posture_observation(&a, "采样结果", assessment, Some("测试语音失败"))?;
        }
        s.queue_due_posture_reports_at(active_time())?;
        assert!(s.jobs("replying")?.is_empty());
        s.0.lock().unwrap().execute(
            "UPDATE posture_report_state SET next_due=0,started=unixepoch()-600",
            [],
        )?;
        s.queue_due_posture_reports_at(active_time())?;
        let reports = s.jobs("replying")?;
        assert_eq!(reports.len(), 1);
        assert_eq!(s.output_kind(&reports[0].id)?, "posture_report");
        assert!(reports[0].result.contains("检查 5 次"));
        assert!(reports[0].result.contains("无法判断 1 次"));
        assert!(reports[0].result.contains("播报失败 5 次"));
        assert!(reports[0].result.contains("每分钟记录（北京时间）"));
        assert_eq!(reports[0].result.matches(" · ").count(), 5);
        assert!(reports[0].result.contains(" · 未触发提醒"));
        assert!(reports[0].result.contains(" · 无法判断"));
        assert!(reports[0].result.contains("离座/非坐姿画面 1 次"));
        assert!(reports[0].result.contains("临界待观察 1 次"));
        assert_eq!(
            s.0.lock()
                .unwrap()
                .query_row("SELECT count(*) FROM posture_samples", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        s.0.lock()
            .unwrap()
            .execute("UPDATE posture_report_state SET next_due=0", [])?;
        s.queue_due_posture_reports_at(active_time())?;
        assert_eq!(s.jobs("replying")?.len(), 1); // no unbounded failed-send backlog
        s.stop_posture_watch(&crate::models::scope(&j.key)?)?;
        assert!(s.jobs("replying")?.is_empty());
        assert!(!s.posture_enabled(&j.key)?);
        s.queue_due_posture_reports_at(active_time())?;
        assert!(s.jobs("replying")?.is_empty());
        Ok(())
    }
    #[test]
    fn old_posture_schema_gains_separate_presence_and_observation_counts() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("mr-li-migrate-{}.sqlite3", uuid::Uuid::new_v4()));
        let c = Connection::open(&path)?;
        c.execute_batch("CREATE TABLE posture_report_state(scope TEXT PRIMARY KEY,started INTEGER NOT NULL,next_due INTEGER NOT NULL,bad INTEGER NOT NULL DEFAULT 0,clear INTEGER NOT NULL DEFAULT 0,unknown INTEGER NOT NULL DEFAULT 0,speech_failures INTEGER NOT NULL DEFAULT 0,last_speech_error TEXT NOT NULL DEFAULT '',last_assessment INTEGER NOT NULL DEFAULT 2); INSERT INTO posture_report_state(scope,started,next_due,unknown) VALUES ('scope',1,600,3);")?;
        drop(c);
        let s = Store::open(&path)?;
        let row = s.0.lock().unwrap().query_row(
            "SELECT unknown,away,observe,next_due FROM posture_report_state WHERE scope='scope'",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )?;
        assert_eq!(row, (3, 0, 0, 600));
        drop(s);
        std::fs::remove_file(path)?;
        Ok(())
    }
    #[test]
    fn posture_report_window_survives_restart_without_reset_or_catchup() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("mr-li-posture-{}.sqlite3", uuid::Uuid::new_v4()));
        let j = Job {
            id: "start".into(),
            key: serde_json::json!(["workspace", "owner", "chat", "thread"]).to_string(),
            message_id: "om_start".into(),
            text: "/posture start".into(),
            result: String::new(),
            sent: 0,
        };
        let s = Store::open(&path)?;
        s.start_posture_watch_at(&j, 60, active_time())?;
        s.queue_due_posture_at(active_time())?;
        let a = s.jobs("queued")?.remove(0);
        s.0.lock()
            .unwrap()
            .execute("UPDATE posture_watch SET next_due=unixepoch()+45", [])?;
        s.finish_posture_observation(&a, "未触发", Assessment::NoObviousIssue, None)?;
        s.finish_posture_observation(&a, "未触发", Assessment::NoObviousIssue, None)?; // retried completion must not double-count
        let next_sample: i64 = s.0.lock().unwrap().query_row(
            "SELECT next_due-unixepoch() FROM posture_watch",
            [],
            |r| r.get(0),
        )?;
        assert!((40..=45).contains(&next_sample)); // analysis/speech do not add another full minute
        s.0.lock().unwrap().execute(
            "UPDATE posture_report_state SET next_due=unixepoch()-3600",
            [],
        )?;
        drop(s);
        let s = Store::open(&path)?;
        s.queue_due_posture_reports_at(active_time())?;
        let rows = s.jobs("replying")?;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].result.contains("未触发提醒 1 次"));
        s.queue_due_posture_reports_at(active_time())?;
        assert_eq!(s.jobs("replying")?.len(), 1);
        s.status(&rows[0].id, "done")?;
        s.queue_due_posture_reports_at(active_time())?;
        assert!(s.jobs("replying")?.is_empty()); // only one report after an offline gap
        drop(s);
        std::fs::remove_file(path)?;
        Ok(())
    }
    #[test]
    fn processing_reaction_survives_restart_and_waits_for_delivery() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("mr-li-reaction-{}.sqlite3", uuid::Uuid::new_v4()));
        let j = Job {
            id: "job".into(),
            key: "key".into(),
            message_id: "om_task".into(),
            text: "hello".into(),
            result: String::new(),
            sent: 0,
        };
        {
            let s = Store::open(&path)?;
            s.insert(&j)?;
            s.status(&j.id, "running")?;
            s.save_processing_reaction(&j, "reaction_id")?;
            assert!(s.completed_reactions()?.is_empty());
            s.finish(&j.id, "answer")?;
            assert!(s.completed_reactions()?.is_empty());
        }
        {
            let s = Store::open(&path)?;
            assert!(s.completed_reactions()?.is_empty());
            s.status(&j.id, "done")?;
            let r = s.completed_reactions()?.pop().unwrap();
            assert_eq!(r.message_id, "om_task");
            assert_eq!(r.reaction_id, "reaction_id");
            s.retry_reaction_cleanup(&j.id)?;
            assert!(s.completed_reactions()?.is_empty());
            s.processing_reaction_removed(&j.id)?;
            assert_eq!(
                s.0.lock().unwrap().query_row(
                    "SELECT count(*) FROM processing_reactions",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
        }
        std::fs::remove_file(path)?;
        Ok(())
    }
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
