use crate::{
    camera,
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
const HELP: &str = "我是李老师。直接发文字即可执行任务，操作权限默认自动允许。\n/help 帮助\n/models 选择免费模型（点击卡片切换）\n也可以直接说“切换模型”\n/posture 本地姿态模型试验（关键点图片与说明）\n/posture start [秒数] 每日 17:00–21:30 检查，默认 60 秒，仅异常播报\n/posture status 最近结果\n/posture stop 停止采样\n/photo 获取摄像头当前照片\n/video [秒数] 从现在录像（默认 10 秒，最长 30 秒）\n/status 当前会话和模型\n/new 新会话（保留旧会话）\n/stop 停止当前任务并取消本会话排队任务\n/approve <请求ID> 仅批准这一次\n/deny <请求ID> 拒绝权限或提问\n/answer <请求ID> <JSON二维数组> 回答提问，例如 [[\"选项A\"],[\"补充说明\"]]\n群聊中请 @李老师。";
fn destination(j: &Job) -> Result<String> {
    let values: Vec<String> = serde_json::from_str(&j.key).context("会话标识无效")?;
    ensure!(values.len() == 4, "会话标识无效");
    // Nonempty root marks group input. Send directly to its chat, regardless of the source thread.
    Ok(if !values[3].is_empty() {
        values[2].clone()
    } else {
        j.message_id.clone()
    })
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
    async fn start_processing(&self, j: &Job) {
        match tokio::time::timeout(
            Duration::from_secs(8),
            self.feishu.add_processing_reaction(&j.message_id),
        )
        .await
        {
            Ok(Ok(id)) => {
                if let Err(e) = self.store.save_processing_reaction(j, &id) {
                    tracing::warn!(error=%e,"保存处理表情失败");
                    let _ = self
                        .feishu
                        .remove_processing_reaction(&j.message_id, &id)
                        .await;
                }
            }
            Ok(Err(e)) => tracing::warn!(error=%e,"添加处理表情失败，继续任务"),
            Err(_) => tracing::warn!("添加处理表情超时，继续任务"),
        }
    }
    async fn clear_completed_reactions(&self) -> Result<()> {
        for r in self.store.completed_reactions()? {
            match tokio::time::timeout(
                Duration::from_secs(8),
                self.feishu
                    .remove_processing_reaction(&r.message_id, &r.reaction_id),
            )
            .await
            {
                Ok(Ok(())) => self.store.processing_reaction_removed(&r.job_id)?,
                _ => {
                    self.store.retry_reaction_cleanup(&r.job_id)?;
                    tracing::warn!(job=%r.job_id,"撤除处理表情失败，将重试");
                }
            }
        }
        Ok(())
    }
    fn selected_model(&self, key: &str) -> Result<String> {
        Ok(self
            .store
            .model(&models::scope(key)?)?
            .or_else(|| self.cfg.model.clone())
            .unwrap_or_else(|| "opencode/glm-5.3-flash-free".into()))
    }
    pub async fn recover(&self) -> Result<()> {
        for j in self.store.jobs("running")? {
            if j.id.starts_with("posture-sample:") {
                self.store
                    .finish_posture_sample(&j, "上次采样因服务中断未完成；等待下一次采样。")?;
                continue;
            }
            if !camera::is_capture(&j.text)
                && let Some(sid) = self.store.session(&j.key)?
            {
                self.oc.abort(&sid).await?;
            }
            self.store.finish(&j.id,"服务曾中断，已请求停止当时的任务。为避免重复副作用，没有自动重跑；请检查结果后重新发送任务。")?;
        }
        Ok(())
    }
    async fn capture_camera(&self, j: &Job) -> Result<()> {
        let background = j.id.starts_with("posture-sample:");
        if background && !self.store.posture_enabled(&j.key)? {
            self.store.status(&j.id, "done")?;
            return Ok(());
        }
        if !background {
            self.start_processing(j).await;
        }
        let mut media = camera::capture(&self.cfg, camera::Capture::parse(&j.text)?).await?;
        if background && !self.store.posture_enabled(&j.key)? {
            self.store.status(&j.id, "done")?;
            return Ok(());
        }
        if media.cover.is_none() {
            crate::posture::archive_photo(&self.cfg, &j.id, &media.path).context("照片归档失败")?;
        }
        if background
            && self.store.posture_enabled(&j.key)?
            && let Err(e) = crate::posture::cache_frame(&self.cfg, &j.key, &media.path)
        {
            tracing::warn!(error=%e,"无法保留坐姿汇总图片");
        }
        let mut assessment = crate::posture::Assessment::Unknown;
        let note = if j.text.trim() == "/posture" {
            let observation = crate::posture::analyze(&self.cfg, &media.path).await?;
            media.path = observation.image;
            assessment = observation.assessment;
            Some(observation.note)
        } else {
            None
        };
        if background {
            self.record_posture(j, note.as_deref().context("定时姿态结果缺失")?, assessment)
                .await?;
            return Ok(());
        }
        let video = media.cover.is_some();
        let key = self
            .feishu
            .upload_camera_media(&media.path, video, media.duration_ms)
            .await?;
        let payload = if let Some(cover) = &media.cover {
            let image_key = self.feishu.upload_camera_media(cover, false, 0).await?;
            json!({"msg_type":"media", "content":{"file_key":key,"image_key":image_key}})
        } else {
            json!({"msg_type":"image", "content":{"image_key":key},"note":note})
        };
        // Persist uploaded keys before attempting delivery; retries never recapture video.
        self.store.finish_output(&j.id, "camera_media", &payload)?;
        Ok(())
    }
    async fn record_posture(
        &self,
        j: &Job,
        note: &str,
        assessment: crate::posture::Assessment,
    ) -> Result<()> {
        if !self.store.posture_enabled(&j.key)? {
            self.store.status(&j.id, "done")?;
            return Ok(());
        }
        let error = crate::posture::speak(&self.cfg, assessment)
            .await
            .err()
            .map(|e| e.to_string());
        if let Some(e) = &error {
            tracing::warn!(error=%e,"坐姿语音播报失败");
        }
        let note = if let Some(e) = &error {
            format!("{note}\n语音播报失败：{e}")
        } else {
            note.to_owned()
        };
        self.store
            .finish_posture_observation(j, &note, assessment, error.as_deref())?;
        Ok(())
    }
    async fn execute(&self, j: &Job) -> Result<()> {
        // Freeze the choice when execution starts; later card clicks affect queued work only.
        let model = self.selected_model(&j.key)?;
        self.start_processing(j).await;
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
            "/posture" => {
                let words: Vec<_> = args.split_whitespace().collect();
                match words.as_slice() {
                    ["start"] | ["start",_] => {
                        ensure!(self.cfg.pose_model.is_file() && self.cfg.pose_detector.as_ref().is_none_or(|p|p.is_file()) && std::path::Path::new(&self.cfg.pose_bin).is_file(),"请先安装人体检测/姿态模型和程序");
                        let seconds=words.get(1).map(|s|s.parse::<u64>()).transpose()?.unwrap_or(60);
                        self.store.start_posture_watch(j,seconds)?;
                        Ok(format!("已启用每日北京时间 17:00–21:30 每 {seconds} 秒检查，仅低头或歪头时在 Mac mini 播报，其余静音。21:30 停止检查，次日 17:00 自动恢复；飞书每 10 分钟图文汇总。\n/posture status 查看最近结果\n/posture stop 停止\n/posture 查看单次关键点图片"))
                    }
                    ["stop"] => {self.store.stop_posture_watch(&models::scope(&j.key)?)?;crate::posture::clear_frames(&self.cfg,&j.key)?;Ok("已停止此聊天的定时姿态试验。".into())}
                    ["status"] => self.store.posture_watch_status(&models::scope(&j.key)?),
                    _=>Ok("/posture 单次关键点试验\n/posture start [间隔秒数] 每日 17:00–21:30 检查，默认 60 秒，仅异常播报\n/posture status 最近结果\n/posture stop 停止".into()),
                }
            }
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
                if let Ok(scope) = models::scope(&j.key) {
                    self.store.stop_posture_watch(&scope)?;
                    crate::posture::clear_frames(&self.cfg, &j.key)?;
                }
                if let Some(sid) = sid {
                    self.oc.abort(&sid).await?;
                }
                for q in self.store.jobs("queued")? {
                    if q.key == j.key && (!is_command(&q) || camera::is_capture(&q.text)) {
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
        self.oc.approve_pending(&sid).await?;
        let list = self.oc.get(&format!("/api/session/{sid}/form")).await?;
        for p in list.as_array().context("OpenCode 待处理提问格式不兼容")? {
            if p["sessionID"] != sid {
                continue;
            }
            let id = p["id"].as_str().context("待处理提问缺少 ID")?;
            if seen.contains(id) {
                continue;
            }
            let detail = format!(
                "需要补充信息：{}\n/answer {id} [[\"答案\"]]\n/deny {id}",
                p["fields"]
            );
            let detail = detail.chars().take(5000).collect::<String>();
            for (i, part) in chunks(&detail).iter().enumerate() {
                self.feishu
                    .reply(
                        &destination(j)?,
                        part,
                        &uuid::Uuid::new_v5(
                            &uuid::Uuid::NAMESPACE_OID,
                            format!("notice:{}:{id}:{i}", j.id).as_bytes(),
                        )
                        .to_string(),
                        false,
                    )
                    .await?;
            }
            seen.insert(id.into());
            return Ok(());
        }
        Ok(())
    }
    async fn prepare_posture_report(&self, j: &Job) -> Result<()> {
        let mut payload = serde_json::from_str::<Value>(&j.result)
            .ok()
            .filter(|p| p["note"].is_string())
            .unwrap_or_else(|| json!({"note":j.result}));
        if payload["snapshot"].is_null() {
            let Some((path, at)) = crate::posture::freeze_report_frame(&self.cfg, &j.key, &j.id)?
            else {
                self.store.finish(
                    &j.id,
                    &format!(
                        "{}\n本批未取得近 10 分钟内的图片，先发送文字记录。",
                        payload["note"].as_str().context("汇总文字缺失")?
                    ),
                )?;
                return Ok(());
            };
            payload["snapshot"] = json!(path);
            payload["captured_at"] = json!(at);
            // Freeze before upload: retries and restarts use the same image and original report.
            self.store
                .finish_output(&j.id, "posture_report", &payload)?;
        }
        let path =
            std::path::PathBuf::from(payload["snapshot"].as_str().context("汇总图片路径缺失")?);
        ensure!(
            path == crate::posture::report_frame_path(&self.cfg, &j.key, &j.id)?,
            "汇总图片路径不匹配"
        );
        let key = self.feishu.upload_camera_media(&path, false, 0).await?;
        let at = payload["captured_at"]
            .as_u64()
            .context("汇总图片时间缺失")?;
        let seconds = (at + 8 * 3600) % 86400;
        let note = format!(
            "{}\n附图：最近一次监控画面（北京时间 {:02}:{:02}）。",
            payload["note"].as_str().context("汇总文字缺失")?,
            seconds / 3600,
            seconds % 3600 / 60
        );
        self.store.finish_output(
            &j.id,
            "camera_media",
            &json!({"msg_type":"image","content":{"image_key":key},"note":note}),
        )?;
        let _ = std::fs::remove_file(path);
        Ok(())
    }
    async fn deliver(&self) -> Result<()> {
        // One durable result per tick; failed sends remain in the outbox for retry.
        if let Some(j) = self.store.jobs("replying")?.first() {
            let kind = self.store.output_kind(&j.id)?;
            if kind == "posture_report" {
                if let Err(e) = self.prepare_posture_report(j).await {
                    self.store.retry_delivery(&j.id)?;
                    return Err(e);
                }
                return Ok(());
            }
            if kind == "camera_media" {
                if j.sent == 0 {
                    let payload: Value = serde_json::from_str(&j.result)?;
                    let uuid = uuid::Uuid::new_v5(
                        &uuid::Uuid::NAMESPACE_OID,
                        format!("result:{}:camera", j.id).as_bytes(),
                    )
                    .to_string();
                    if let Err(e) = self
                        .feishu
                        .reply_camera_media(&destination(j)?, &payload, &uuid, false)
                        .await
                    {
                        self.store.retry_delivery(&j.id)?;
                        return Err(e);
                    }
                    self.store.sent(&j.id, 1)?;
                }
                let payload: Value = serde_json::from_str(&j.result)?;
                if j.sent <= 1
                    && let Some(note) = payload["note"].as_str()
                {
                    let uuid = uuid::Uuid::new_v5(
                        &uuid::Uuid::NAMESPACE_OID,
                        format!("result:{}:posture-note", j.id).as_bytes(),
                    )
                    .to_string();
                    if let Err(e) = self
                        .feishu
                        .reply(&destination(j)?, note, &uuid, false)
                        .await
                    {
                        self.store.retry_delivery(&j.id)?;
                        return Err(e);
                    }
                    self.store.sent(&j.id, 2)?;
                }
                self.store.status(&j.id, "done")?;
                return Ok(());
            }
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
                        .reply_card(&destination(j)?, &payload["card"], &uuid, false)
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
                    .reply(&destination(j)?, part, &uuid.to_string(), false)
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
        let mut camera_active: Option<(Job, JoinHandle<Result<()>>)> = None;
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
            if !crate::posture::schedule_active()
                && camera_active
                    .as_ref()
                    .is_some_and(|(j, _)| j.id.starts_with("posture-sample:"))
            {
                let (j, h) = camera_active.take().unwrap();
                h.abort();
                let _ = h.await;
                self.store.status(&j.id, "done")?;
            }
            if camera_active.as_ref().is_some_and(|(_, h)| h.is_finished()) {
                let (j, h) = camera_active.take().unwrap();
                if let Err(e) = h.await.context("摄像头执行器异常退出").and_then(|r| r) {
                    tracing::warn!(error=%e, "摄像头任务失败");
                    if j.id.starts_with("posture-sample:") {
                        self.record_posture(
                            &j,
                            &format!("本次采样失败：{e}，无法判断坐姿。"),
                            crate::posture::Assessment::Unknown,
                        )
                        .await?;
                    } else {
                        self.store.finish(&j.id, &format!("摄像头操作失败：{e}"))?;
                    }
                }
            }
            self.store.queue_due_posture_reports()?;
            self.store.queue_due_posture()?;
            let queued = self.store.jobs("queued")?;
            if camera_active.is_none()
                && let Some(j) = queued.iter().find(|j| camera::is_capture(&j.text))
            {
                self.store.status(&j.id, "running")?;
                let b = self.clone();
                let job = j.clone();
                camera_active = Some((
                    j.clone(),
                    tokio::spawn(async move {
                        if job.id.starts_with("posture-sample:") {
                            match tokio::time::timeout(
                                crate::posture::remaining_window(),
                                b.capture_camera(&job),
                            )
                            .await
                            {
                                Ok(result) => result,
                                Err(_) => b.store.status(&job.id, "done"),
                            }
                        } else {
                            b.capture_camera(&job).await
                        }
                    }),
                ));
            }
            // Commands stay responsive while the sole model task waits for an approval.
            for j in queued
                .iter()
                .filter(|j| is_command(j) && !camera::is_capture(&j.text))
                .take(1)
            {
                if (j.text.trim() == "/posture stop" || j.text.trim() == "/stop")
                    && camera_active.as_ref().is_some_and(|(c, _)| {
                        c.id.starts_with("posture-sample:")
                            && models::scope(&c.key).ok() == models::scope(&j.key).ok()
                    })
                {
                    let (c, h) = camera_active.take().unwrap();
                    h.abort();
                    let _ = h.await;
                    self.store.status(&c.id, "done")?;
                }
                if j.text.split_whitespace().next() == Some("/stop")
                    && camera_active.as_ref().is_some_and(|(c, _)| c.key == j.key)
                {
                    let (c, h) = camera_active.take().unwrap();
                    h.abort();
                    let _ = h.await;
                    self.store.finish(&c.id, "摄像头任务已停止。")?;
                }
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
            if let Err(e) = self.clear_completed_reactions().await {
                tracing::warn!(error=%e,"处理表情清理失败");
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
    fn group_delivery_ignores_source_thread_but_private_replies_stay_private() -> Result<()> {
        let mut j = Job {
            id: "job".into(),
            message_id: "om_origin".into(),
            key: String::new(),
            text: String::new(),
            result: String::new(),
            sent: 0,
        };
        for root in ["om_first", "om_other", "om_saved_yesterday"] {
            j.key = json!(["workspace", "owner", "oc_group", root]).to_string();
            assert_eq!(destination(&j)?, "oc_group");
        }
        j.key = json!(["workspace", "owner", "oc_private", ""]).to_string();
        assert_eq!(destination(&j)?, "om_origin");
        j.key = json!(["workspace", "owner"]).to_string();
        assert!(destination(&j).is_err());
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
