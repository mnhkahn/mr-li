use crate::{
    config::Config,
    feishu::Feishu,
    models,
    opencode::OpenCode,
    store::{Job, Store},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::task::JoinHandle;

pub struct Bridge {
    pub cfg: Arc<Config>,
    pub store: Arc<Store>,
    pub feishu: Arc<Feishu>,
    pub oc: Arc<OpenCode>,
}
const HELP: &str = "我是李老师。直接发文字即可执行任务。\n/help 帮助\n/models 选择免费模型（点击卡片切换）\n也可以直接说“切换模型”\n/status 当前会话和模型\n/new 新会话（保留旧会话）\n/stop 停止当前任务并取消本会话排队任务\n/approve <请求ID> 仅批准这一次\n/deny <请求ID> 拒绝权限或提问\n/answer <请求ID> <JSON二维数组> 回答提问，例如 [[\"选项A\"],[\"补充说明\"]]\n群聊中请 @李老师。";
fn in_thread(j: &Job) -> bool {
    serde_json::from_str::<Value>(&j.key)
        .ok()
        .is_some_and(|v| v[3].as_str().is_some_and(|s| !s.is_empty()))
}
fn is_command(j: &Job) -> bool {
    j.text.starts_with('/')
}
pub fn chunks(text: &str) -> Vec<String> {
    // Conservative bound in UTF-8 bytes, leaving room for JSON escaping and Feishu limits.
    let mut result = Vec::new();
    let mut s = String::new();
    for c in text.chars() {
        if s.len() + c.len_utf8() > 6000 {
            result.push(std::mem::take(&mut s));
        }
        s.push(c);
    }
    if !s.is_empty() {
        result.push(s)
    }
    result
}
impl Bridge {
    fn selected_model(&self, key: &str) -> Result<String> {
        Ok(self
            .store
            .model(&models::scope(key)?)?
            .or_else(|| self.cfg.model.clone())
            .unwrap_or_else(|| "opencode/glm-5.3-flash-free".into()))
    }
    pub async fn recover(&self) -> Result<()> {
        for j in self.store.jobs("running")? {
            if let Some(sid) = self.store.session(&j.key)? {
                self.oc.abort(&sid).await?;
            }
            self.store.finish(&j.id,"服务曾中断，已请求停止当时的任务。为避免重复副作用，没有自动重跑；请检查结果后重新发送任务。")?;
        }
        Ok(())
    }
    async fn execute(&self, j: &Job) -> Result<()> {
        // Freeze the choice when execution starts; later card clicks affect queued work only.
        let model = self.selected_model(&j.key)?;
        let sid = if let Some(sid) = self.store.session(&j.key)? {
            // A missing session must not silently discard the conversation history.
            self.oc
                .get(&format!("/api/session/{sid}"))
                .await
                .context("会话不可用，请用 /new 创建新会话")?;
            sid
        } else {
            let sid = self.oc.create(Some(&model)).await?;
            self.store.set_session(&j.key, &sid)?;
            sid
        };
        let text = self.oc.prompt(&sid, &j.text, Some(&model)).await?;
        self.store.finish(&j.id, &text)?;
        Ok(())
    }
    async fn command(&self, j: &Job, active: Option<&Job>) -> Result<String> {
        let (cmd, args) = j
            .text
            .split_once(char::is_whitespace)
            .unwrap_or((&j.text, ""));
        let args = args.trim();
        let sid = self.store.session(&j.key)?;
        match cmd {
            "/help" => Ok(HELP.into()),
            "/status" => {
                let pending = self
                    .store
                    .jobs("queued")?
                    .iter()
                    .filter(|q| q.key == j.key && !is_command(q))
                    .count();
                Ok(format!(
                    "会话：{}\n模型：{}\n执行中：{}\n排队：{}",
                    sid.as_deref().unwrap_or("尚未创建"),
                    self.selected_model(&j.key)?,
                    active.is_some_and(|a| a.key == j.key),
                    pending
                ))
            }
            "/new" => {
                ensure!(
                    !active.is_some_and(|a| a.key == j.key),
                    "有任务正在执行，请先 /stop"
                );
                ensure!(
                    !self
                        .store
                        .jobs("queued")?
                        .iter()
                        .any(|q| q.key == j.key && !is_command(q)),
                    "有任务排队，请先 /stop"
                );
                self.store.reset(&j.key)?;
                Ok("下条消息将创建新会话，旧会话仍保存在 OpenCode。".into())
            }
            "/stop" => {
                if let Some(sid) = sid {
                    self.oc.abort(&sid).await?;
                }
                for q in self.store.jobs("queued")? {
                    if q.key == j.key && !is_command(&q) {
                        self.store.finish(&q.id, "任务已由 /stop 取消。")?;
                    }
                }
                Ok("已请求停止当前会话任务，并取消排队任务。".into())
            }
            "/approve" | "/deny" | "/answer" => {
                let sid = sid.context("尚无会话")?;
                let (id, answer) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
                ensure!(
                    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                    "请提供有效请求 ID"
                );
                let permissions = self
                    .oc
                    .get(&format!("/api/session/{sid}/permission"))
                    .await?;
                let questions = self.oc.get(&format!("/api/session/{sid}/form")).await?;
                let owns = |v: &Value| {
                    v.as_array()
                        .is_some_and(|a| a.iter().any(|r| r["id"] == id && r["sessionID"] == sid))
                };
                if owns(&permissions) {
                    ensure!(cmd != "/answer", "权限请求请用 /approve 或 /deny");
                    self.oc
                        .post(
                            &format!("/api/session/{sid}/permission/{id}/reply"),
                            json!({"decision":if cmd=="/approve"{"once"}else{"reject"}}),
                        )
                        .await?;
                } else if owns(&questions) {
                    ensure!(cmd != "/approve", "提问请用 /answer 或 /deny");
                    if cmd == "/deny" {
                        self.oc
                            .delete(&format!("/api/session/{sid}/form/{id}"))
                            .await?;
                    } else {
                        let form = questions
                            .as_array()
                            .and_then(|forms| forms.iter().find(|f| f["id"] == id))
                            .context("表单已失效")?;
                        let answers = crate::opencode::form_answers(
                            form,
                            serde_json::from_str(answer.trim()).context("请输入 JSON 答案")?,
                        )?;
                        self.oc
                            .post(
                                &format!("/api/session/{sid}/form/{id}/reply"),
                                json!({"answer":answers}),
                            )
                            .await?;
                    }
                } else {
                    anyhow::bail!("请求不存在、已处理，或不属于当前用户和会话")
                }
                Ok("已提交。".into())
            }
            _ => Ok(HELP.into()),
        }
    }
    async fn handle_command(&self, j: &Job, active: Option<&Job>) -> Result<()> {
        let mut words = j.text.split_whitespace();
        let cmd = words.next().unwrap_or("");
        if matches!(cmd, "/models" | "/model") {
            let available = models::free_models(&self.oc.catalog().await?);
            ensure!(
                !available.is_empty(),
                "暂未找到可用的 OpenCode 免费模型，请稍后重试"
            );
            let token = uuid::Uuid::new_v4().simple().to_string();
            let selected = self.selected_model(&j.key)?;
            let card = models::picker(&available, &selected, &token, false);
            self.store.register_model_card(&token, &j.key)?;
            self.store
                .finish_output(&j.id, "model_card", &json!({"token":token,"card":card}))?;
        } else if cmd == "/_model-select" {
            ensure!(j.id.starts_with("card:"), "请在模型选择卡片中点选模型");
            let token = words.next().context("卡片标识缺失")?;
            let selected = words.next().context("请选择模型")?;
            let card = self
                .store
                .model_card(token)?
                .context("卡片已过期，请重新打开模型选择")?;
            ensure!(
                card.origin_key == j.key && card.message_id == j.message_id,
                "卡片不属于当前会话"
            );
            let available = models::free_models(&self.oc.catalog().await?);
            ensure!(
                available.iter().any(|m| m.id == selected),
                "此模型已下线或不再免费，请重新打开模型选择"
            );
            let updated = models::picker(&available, selected, token, true);
            self.store.switch_model_and_finish(
                &j.id,
                &models::scope(&j.key)?,
                selected,
                &updated,
            )?;
        } else {
            let text = self.command(j, active).await?;
            self.store.finish(&j.id, &text)?;
        }
        Ok(())
    }
    async fn notify_pending(&self, j: &Job, seen: &mut HashSet<String>) -> Result<()> {
        let Some(sid) = self.store.session(&j.key)? else {
            return Ok(());
        };
        for kind in ["permission", "question"] {
            let path = if kind == "permission" {
                format!("/api/session/{sid}/permission")
            } else {
                format!("/api/session/{sid}/form")
            };
            let list = self.oc.get(&path).await?;
            for p in list.as_array().context("OpenCode 待处理请求格式不兼容")? {
                if p["sessionID"] != sid {
                    continue;
                }
                let id = p["id"].as_str().context("待处理请求缺少 ID")?;
                if seen.contains(id) {
                    continue;
                }
                let detail = if kind == "permission" {
                    format!(
                        "需要授权：{}\n范围：{}\n/approve {id}\n/deny {id}",
                        p["action"], p["resources"]
                    )
                } else {
                    format!(
                        "需要补充信息：{}\n/answer {id} [[\"答案\"]]\n/deny {id}",
                        p["fields"]
                    )
                };
                let detail = detail.chars().take(5000).collect::<String>();
                for (i, part) in chunks(&detail).iter().enumerate() {
                    self.feishu
                        .reply(
                            &j.message_id,
                            part,
                            &uuid::Uuid::new_v5(
                                &uuid::Uuid::NAMESPACE_OID,
                                format!("notice:{}:{id}:{i}", j.id).as_bytes(),
                            )
                            .to_string(),
                            in_thread(j),
                        )
                        .await?;
                }
                seen.insert(id.into());
                return Ok(());
            }
        }
        Ok(())
    }
    async fn deliver(&self) -> Result<()> {
        // One durable result per tick; failed sends remain in the outbox for retry.
        if let Some(j) = self.store.jobs("replying")?.first() {
            let kind = self.store.output_kind(&j.id)?;
            if kind == "model_card" || kind == "model_card_update" {
                let mut payload: Value = serde_json::from_str(&j.result)?;
                let current = self.selected_model(&j.key)?;
                if kind == "model_card" {
                    payload["card"] = models::refresh_selection(&payload["card"], &current)?;
                } else {
                    payload = models::refresh_selection(&payload, &current)?;
                }
                let uuid = uuid::Uuid::new_v5(
                    &uuid::Uuid::NAMESPACE_OID,
                    format!("result:{}:card", j.id).as_bytes(),
                )
                .to_string();
                let result = if kind == "model_card" {
                    match self
                        .feishu
                        .reply_card(&j.message_id, &payload["card"], &uuid, in_thread(j))
                        .await
                    {
                        Ok(message) => self.store.attach_model_card(
                            payload["token"].as_str().context("卡片标识缺失")?,
                            &message,
                        ),
                        Err(e) => Err(e),
                    }
                } else {
                    self.feishu.update_card(&j.message_id, &payload).await
                };
                if let Err(e) = result {
                    self.store.retry_delivery(&j.id)?;
                    return Err(e);
                }
                self.store.sent(&j.id, 1)?;
                self.store.status(&j.id, "done")?;
                return Ok(());
            }
            if let Some((i, part)) = chunks(&j.result).iter().enumerate().nth(j.sent) {
                let uuid = uuid::Uuid::new_v5(
                    &uuid::Uuid::NAMESPACE_OID,
                    format!("result:{}:{i}", j.id).as_bytes(),
                );
                if let Err(e) = self
                    .feishu
                    .reply(&j.message_id, part, &uuid.to_string(), in_thread(j))
                    .await
                {
                    self.store.retry_delivery(&j.id)?;
                    return Err(e);
                }
                self.store.sent(&j.id, i + 1)?;
                return Ok(());
            }
            self.store.status(&j.id, "done")?;
        }
        Ok(())
    }
    pub async fn run(self: Arc<Self>) -> Result<()> {
        self.recover().await?;
        let mut active: Option<(Job, JoinHandle<Result<()>>)> = None;
        let mut seen = HashSet::new();
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut count = 0u64;
        loop {
            tick.tick().await;
            count += 1;
            std::fs::write(
                self.cfg.data_dir.join("heartbeat"),
                format!(
                    "{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_secs()
                ),
            )?;
            if active.as_ref().is_some_and(|(_, h)| h.is_finished()) {
                let (j, h) = active.take().unwrap();
                let result = h.await.context("任务执行器异常退出").and_then(|r| r);
                if let Err(e) = result {
                    // Keep the job running in SQLite. Recovery must confirm abort before any new task.
                    if e.downcast_ref::<crate::opencode::AbortUnconfirmed>()
                        .is_some()
                    {
                        return Err(e);
                    }
                    tracing::error!(error=%e,"任务失败");
                    self.store
                        .finish(&j.id, &format!("任务失败：{e}。可用 /status 查看会话。"))?;
                }
                seen.clear();
            }
            let queued = self.store.jobs("queued")?;
            // Commands stay responsive while the sole model task waits for an approval.
            for j in queued.iter().filter(|j| is_command(j)).take(1) {
                self.store.status(&j.id, "running")?;
                if let Err(e) = self
                    .handle_command(j, active.as_ref().map(|(j, _)| j))
                    .await
                {
                    self.store.finish(&j.id, &format!("操作失败：{e}"))?;
                }
            }
            if active.is_none() {
                // Reload: /stop may have cancelled queued work in this tick.
                if let Some(j) = self
                    .store
                    .jobs("queued")?
                    .into_iter()
                    .find(|j| !is_command(j))
                {
                    self.store.status(&j.id, "running")?;
                    let b = self.clone();
                    let job = j.clone();
                    active = Some((j, tokio::spawn(async move { b.execute(&job).await })));
                }
            }
            if let Some((j, _)) = &active
                && let Err(e) = self.notify_pending(j, &mut seen).await
            {
                tracing::warn!(error=%e,"待处理请求同步失败");
            }
            if let Err(e) = self.deliver().await {
                tracing::warn!(error=%e,"飞书结果暂未送达，将重试");
            }
            if count.is_multiple_of(1800) {
                self.store.prune()?;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn approval_cannot_cross_session_boundary() -> Result<()> {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || {
            for payload in [
                json!([{"id":"per_foreign","sessionID":"ses_foreign"}]),
                json!([]),
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut buf = [0u8; 8192];
                let n = socket.read(&mut buf).unwrap();
                assert!(std::str::from_utf8(&buf[..n]).unwrap().starts_with("GET "));
                let body = payload.to_string();
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
        });
        let mut cfg = Config::fixture();
        cfg.opencode_url = format!("http://{address}");
        let cfg = Arc::new(cfg);
        let store = Arc::new(Store::open(std::path::Path::new(":memory:"))?);
        store.set_session("owner-key", "ses_owner")?;
        let b = Bridge {
            oc: Arc::new(OpenCode::new(cfg.clone())?),
            feishu: Arc::new(Feishu::new(cfg.clone())?),
            cfg,
            store,
        };
        let j = Job {
            id: "om_cmd".into(),
            key: "owner-key".into(),
            message_id: "om_cmd".into(),
            text: "/approve per_foreign".into(),
            result: String::new(),
            sent: 0,
        };
        assert!(
            b.command(&j, None)
                .await
                .unwrap_err()
                .to_string()
                .contains("不属于")
        );
        server.join().unwrap();
        Ok(())
    }
    #[test]
    fn utf8_chunks_are_bounded_and_lossless() {
        let s = "你好🦀\n\"".repeat(4000);
        let parts = chunks(&s);
        assert!(parts.iter().all(|p| p.len() <= 6000));
        assert_eq!(parts.concat(), s);
    }
}
