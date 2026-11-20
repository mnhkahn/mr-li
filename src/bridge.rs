use crate::{
    agent::{AgentHub, Kind},
    camera, commands,
    config::Config,
    feishu::Feishu,
    forms, models, speech,
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
    pub agents: Arc<AgentHub>,
    pub tablet: Option<Arc<crate::tablet::Gateway>>,
}
const HELP: &str = commands::HELP;
pub(crate) fn image_message(image_key: &str, note: Option<&str>) -> Value {
    if let Some(note) = note.filter(|note| !note.is_empty()) {
        let mut rows: Vec<Value> = note
            .lines()
            .map(|line| json!([{"tag":"text","text":line}]))
            .collect();
        rows.push(json!([{"tag":"img","image_key":image_key}]));
        json!({"msg_type":"post","content":{"zh_cn":{"content":rows}}})
    } else {
        json!({"msg_type":"image","content":{"image_key":image_key}})
    }
}

fn report_message(note: &str, images: &[(String, String)]) -> Value {
    let mut rows: Vec<Value> = note
        .lines()
        .map(|line| json!([{"tag":"text","text":line}]))
        .collect();
    for (label, key) in images {
        rows.push(json!([{"tag":"text","text":label}]));
        rows.push(json!([{"tag":"img","image_key":key}]));
    }
    json!({"msg_type":"post","content":{"zh_cn":{"content":rows}}})
}
fn capture_time(at: u64) -> String {
    let seconds = (at + 8 * 3600) % 86400;
    format!("北京时间 {:02}:{:02}", seconds / 3600, seconds % 3600 / 60)
}

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
        if j.id.starts_with("builtin:") {
            return;
        }
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
    fn selected_agent(&self, key: &str) -> Result<Kind> {
        Kind::parse(&self.store.agent(&models::scope(key)?)?)
    }
    fn session(&self, key: &str) -> Result<Option<String>> {
        self.store
            .agent_session(key, self.selected_agent(key)?.id())
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
                && !speech::is_command(&j.text)
                && let Some(sid) = self.session(&j.key)?
            {
                self.agents.abort(&sid).await?;
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
        let mut assessment = crate::posture::Assessment::Unknown;
        let note = if j.text.trim() == "/posture" {
            let observation = crate::posture::analyze(&self.cfg, &media.path).await?;
            media.path = observation.image;
            assessment = observation.assessment;
            Some(observation.note)
        } else {
            None
        };
        if background
            && self.store.posture_enabled(&j.key)?
            && let Err(e) = crate::posture::cache_frame(&self.cfg, &j.key, &media.path)
        {
            tracing::warn!(error=%e,"无法保留坐姿汇总图片");
        }
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
            image_message(&key, note.as_deref())
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
        let kind = self.selected_agent(&j.key)?;
        let model = self.selected_model(&j.key)?;
        if let Err(e) = crate::reply::acknowledge(self, j).await {
            tracing::warn!(error=%e,"更新已操作卡片失败，继续处理请求");
        }
        self.start_processing(j).await;
        let sid = if let Some(sid) = self.session(&j.key)? {
            // A missing session must not silently discard the conversation history.
            self.agents
                .validate_session(&sid)
                .await
                .context("会话不可用，请用 /new 创建新会话")?;
            sid
        } else {
            let sid = self.agents.create_for(kind, Some(&model)).await?;
            self.store.set_agent_session(&j.key, kind.id(), &sid)?;
            sid
        };
        let token = self.store.builtin_context(&j.id)?;
        let prompt = format!(
            "{}\n\n[李老师内置工具上下文]\n本次请求的 request_token 为 {token}。调用 teacher_* 工具时传入它，不向用户展示。不要复用历史 token；它只在本次任务执行期间有效。用户要求说话、拍照录像、坐姿检测、平板通知、代理查询/测速/切换时优先用对应 teacher_* 工具；代理使用 teacher_proxy，切换必须来自用户要求；同一副作用重试沿用相同 operation_id。工具超时后先查询任务，不要换 operation_id 重做。权限批准、会话重置和 Agent/模型切换仍由用户操作。",
            j.text
        );
        let prompt = format!("{prompt}\n\n{}", crate::reply::INSTRUCTIONS);
        let result = self.agents.prompt(&sid, &prompt, Some(&model)).await;
        self.store.revoke_builtin_context(&token)?;
        let text = result?;
        crate::reply::finish(&self.store, j, &text)?;
        Ok(())
    }
    async fn speak_job(&self, j: &Job) -> Result<()> {
        let text = speech::text(&j.text).context("请提供播报文字")?;
        speech::speak(&self.cfg, text).await?;
        self.store
            .finish(&j.id, "已通过 Mac mini 连接的喇叭播报。")?;
        Ok(())
    }
    pub(crate) async fn command(&self, j: &Job, active: Option<&Job>) -> Result<String> {
        let (cmd, args) = j
            .text
            .split_once(char::is_whitespace)
            .unwrap_or((&j.text, ""));
        let args = args.trim();
        let sid = self.session(&j.key)?;
        match cmd {
            "/help" => Ok(HELP.into()),
            "/proxy" => crate::proxy::command(args).await,
            "/ha" => {
                if args.is_empty() {
                    return Ok("/ha devices [名称/房间/类型]\n/ha commands <实体ID或设备ID>\n/ha control <实体ID> <domain.service> [JSON参数]".into());
                }
                let value =
                    crate::home_assistant::command(self.cfg.home_assistant.as_ref(), args).await?;
                Ok(serde_json::to_string_pretty(&value)?)
            }
            "/skills" | "/mcp" if self.selected_agent(&j.key)? != Kind::OpenCode => Ok(
                "此 Agent 的技能和 MCP 由其 CLI 配置及会话管理；平板 MCP 会在创建会话时传入。"
                    .into(),
            ),
            "/skills" => commands::skills(&self.agents.get("/api/skill").await?),
            "/mcp" => commands::mcp(&self.agents.get("/api/mcp").await?),
            "/tasks" => Ok(commands::task_list(
                &self.store.command_tasks(&j.key, &j.id)?,
            )),
            "/task" => {
                let id = args.strip_prefix("status ").unwrap_or(args).trim();
                ensure!(
                    !id.is_empty(),
                    "用法：/task <任务ID> 或 /task status <任务ID>"
                );
                let task = self
                    .store
                    .command_task(&j.key, id)?
                    .context("任务不存在，或不属于当前会话")?;
                Ok(commands::task(&task))
            }
            "/channel" => {
                let mut text = format!(
                    "消息入口：{}（已配置）",
                    if self.cfg.feishu_base_url.contains("larksuite.com") {
                        "Lark 国际版"
                    } else {
                        "飞书"
                    }
                );
                if let Some(tablet) = &self.tablet {
                    let devices = tablet.devices_status()?;
                    if devices.is_empty() {
                        text.push_str("\n平板：尚未配对");
                    }
                    for device in devices {
                        text.push_str(&format!(
                            "\n平板：{} · {}",
                            device["name"].as_str().unwrap_or("学习平板"),
                            if device["online"] == true {
                                "在线"
                            } else {
                                "离线"
                            }
                        ));
                    }
                } else {
                    text.push_str("\n平板网关：未启用");
                }
                Ok(text)
            }
            "/cron" => {
                ensure!(
                    args.is_empty() || args == "list",
                    "当前支持 /cron list；手动检测用 /posture，启停监测用 /posture start 或 /posture stop"
                );
                Ok(format!(
                    "当前聊天的定时任务：坐姿监测（每 10 分钟图文汇总）\n{}",
                    self.store.posture_watch_status(&models::scope(&j.key)?)?
                ))
            }
            "/baby" | "/notify" => {
                let tablet = self.tablet.as_ref().context("平板网关未启用")?;
                let now = crate::posture::now_epoch() as i64;
                let owner = models::scope(&j.key)?;
                if args == "list" {
                    return tablet.notices.list(&owner, now);
                }
                if let Some(id) = args.strip_prefix("cancel ") {
                    tablet.notices.cancel(&owner, id.trim())?;
                    return Ok("已取消尚未送达的通知。".into());
                }
                if args.is_empty() || args == "send" || args == "cancel" || args == "help" {
                    return Ok(format!(
                        "{cmd} <内容> 给宝宝发消息，在平板作业首页显示\n{cmd} list 查看本聊天你的消息和送达状态\n{cmd} cancel <ID> 取消尚未发送的消息\n{cmd} send --device <设备ID> <内容> 指定平板\n首页仅显示最新消息，当天结束后消失；已送达消息不能远程撤回，可发送新消息替换。"
                    ));
                }
                let publish = crate::notices::parse_send(args)?;
                let devices = tablet.devices_status()?;
                let device = if let Some(id) = publish.device {
                    ensure!(
                        devices.iter().any(|device| device["id"] == id),
                        "设备尚未配对"
                    );
                    id
                } else {
                    ensure!(
                        devices.len() == 1,
                        "请先配对平板；多台设备时用 {cmd} send --device <设备ID> <内容>"
                    );
                    devices[0]["id"]
                        .as_str()
                        .context("设备 ID 缺失")?
                        .to_owned()
                };
                let id = tablet
                    .notices
                    .publish(&owner, &j.id, &device, &publish.body, now)?;
                Ok(format!(
                    "通知已保存，等待平板确认送达。离线时会在当天重连后补发。\n{id}\n{cmd} list 查看状态"
                ))
            }
            "/speak" => {
                if args == "stop" {
                    for q in self.store.jobs("queued")? {
                        if q.key == j.key && speech::is_command(&q.text) {
                            self.store.finish(&q.id, "语音任务已取消。")?;
                        }
                    }
                    Ok("已停止本会话语音，并取消待播报文字。".into())
                } else {
                    Ok("/speak <文字> 将文字通过 Mac mini 连接的喇叭播放，最多 500 字\n/speak stop 停止本会话语音".into())
                }
            }

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
                let speaking = self
                    .store
                    .jobs("running")?
                    .iter()
                    .any(|q| q.key == j.key && speech::is_command(&q.text));
                let speech_queue = self
                    .store
                    .jobs("queued")?
                    .iter()
                    .filter(|q| q.key == j.key && speech::text(&q.text).is_some())
                    .count();
                Ok(format!(
                    "Agent：{}\n会话：{}\n模型：{}\n执行中：{}\n排队：{}\n语音播报中：{}\n待播报：{}",
                    self.selected_agent(&j.key)?.name(),
                    sid.as_deref().unwrap_or("尚未创建"),
                    if self.selected_agent(&j.key)? == Kind::OpenCode {
                        self.selected_model(&j.key)?
                    } else {
                        "免费账户 / 免费模型".into()
                    },
                    active.is_some_and(|a| a.key == j.key),
                    pending,
                    speaking,
                    speech_queue
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
                self.store
                    .reset_agent_session(&j.key, self.selected_agent(&j.key)?.id())?;
                Ok("下条消息将创建新会话，旧会话仍保存在对应 Agent。".into())
            }
            "/stop" => {
                if let Ok(scope) = models::scope(&j.key) {
                    self.store.stop_posture_watch(&scope)?;
                    crate::posture::clear_frames(&self.cfg, &j.key)?;
                }
                if let Some(sid) = sid {
                    self.agents.abort(&sid).await?;
                }
                for q in self.store.jobs("queued")? {
                    if q.key == j.key
                        && (!is_command(&q)
                            || camera::is_capture(&q.text)
                            || speech::is_command(&q.text))
                    {
                        self.store.finish(&q.id, "任务已由 /stop 取消。")?;
                    }
                }
                Ok("已请求停止当前会话任务和语音，并取消排队任务。".into())
            }
            "/approve" | "/deny" | "/answer" => {
                let sid = sid.context("尚无会话")?;
                let (id, answer) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
                ensure!(
                    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                    "请提供有效请求 ID"
                );
                let permissions = if self.selected_agent(&j.key)? == Kind::OpenCode {
                    self.agents
                        .get(&format!("/api/session/{sid}/permission"))
                        .await?
                } else {
                    json!([])
                };
                let questions = self.agents.forms(&sid).await?;
                let owns = |v: &Value| {
                    v.as_array()
                        .is_some_and(|a| a.iter().any(|r| r["id"] == id && r["sessionID"] == sid))
                };
                if owns(&permissions) {
                    ensure!(cmd != "/answer", "权限请求请用 /approve 或 /deny");
                    self.agents
                        .post(
                            &format!("/api/session/{sid}/permission/{id}/reply"),
                            json!({"decision":if cmd=="/approve"{"once"}else{"reject"}}),
                        )
                        .await?;
                } else if owns(&questions) {
                    ensure!(cmd != "/approve", "提问请用 /answer 或 /deny");
                    if cmd == "/deny" {
                        self.agents.answer_form(&sid, id, None).await?;
                    } else {
                        let form = questions
                            .as_array()
                            .and_then(|forms| forms.iter().find(|f| f["id"] == id))
                            .context("表单已失效")?;
                        let answers = crate::opencode::form_answers(
                            form,
                            serde_json::from_str(answer.trim()).context("请输入 JSON 答案")?,
                        )?;
                        self.agents.answer_form(&sid, id, Some(answers)).await?;
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
        if cmd == "/_reply_cancel" {
            ensure!(j.id.starts_with("reply-action:"), "请使用卡片取消按钮");
            if let Err(e) = crate::reply::acknowledge(self, j).await {
                tracing::warn!(error=%e,"更新取消卡片失败");
            }
            self.store.status(&j.id, "done")?;
        } else if cmd == "/agent" {
            let selected = self.selected_agent(&j.key)?;
            let token = uuid::Uuid::new_v4().simple().to_string();
            self.store.register_agent_card(&token, &j.key)?;
            self.store.finish_output(
                &j.id,
                "agent_card",
                &json!({"token":token,"card":crate::agent::picker(selected,&token,false)}),
            )?;
        } else if cmd == "/_agent-select" {
            ensure!(j.id.starts_with("card:"), "请在 Agent 卡片中选择");
            let token = words.next().context("卡片标识缺失")?;
            let selected = Kind::parse(words.next().context("请选择 Agent")?)?;
            let card = self
                .store
                .agent_card(token)?
                .context("卡片已过期，请重新输入 /agent")?;
            ensure!(
                card.origin_key == j.key && card.message_id == j.message_id,
                "卡片不属于当前会话"
            );
            let scope = models::scope(&j.key)?;
            ensure!(
                !active
                    .is_some_and(|a| models::scope(&a.key).ok().as_deref() == Some(scope.as_str())),
                "本聊天有任务正在执行，请先 /stop，再切换 Agent"
            );
            // Switching is allowed before installation/login, so the card always exposes
            // all five choices. Execution reports concrete setup failures lazily.
            let updated = crate::agent::picker(selected, token, true);
            self.store
                .switch_agent_and_finish(&j.id, &scope, selected.id(), &updated)?;
        } else if cmd == "/_form" {
            self.handle_form(j).await?;
        } else if matches!(cmd, "/models" | "/model") {
            ensure!(
                self.selected_agent(&j.key)? == Kind::OpenCode,
                "当前 Agent 使用免费账户或免费模型；请用 /agent 选择后端。/models 当前用于 OpenCode。"
            );
            let available = models::free_models(&self.agents.catalog().await?);
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
            ensure!(
                self.selected_agent(&j.key)? == Kind::OpenCode,
                "当前 Agent 已切换，请用 /agent 选择 OpenCode 后再设置模型"
            );
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
            let available = models::free_models(&self.agents.catalog().await?);
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
    async fn handle_form(&self, j: &Job) -> Result<()> {
        ensure!(j.id.starts_with("card:"), "请使用提问卡片操作");
        let payload: Value =
            serde_json::from_str(j.text.strip_prefix("/_form ").context("卡片操作缺失")?)?;
        let token = payload["token"].as_str().context("卡片标识缺失")?;
        let saved = self.store.form_card(token)?.context("卡片已失效")?;
        ensure!(
            saved.origin_key == j.key && saved.message_id == j.message_id,
            "卡片不属于当前会话"
        );
        let mut state = saved.state;
        if payload["revision"] != state["revision"] || state["done"] == true {
            self.store.status(&j.id, "done")?;
            return Ok(());
        }
        let sid = &saved.session;
        ensure!(
            self.session(&j.key)?.as_deref() == Some(sid),
            "会话已切换，旧提问已失效"
        );
        let id = state["form"]["id"]
            .as_str()
            .context("提问 ID 缺失")?
            .to_owned();
        let pending = self.agents.forms(sid).await?;
        let owned = pending
            .as_array()
            .context("提问列表无效")?
            .iter()
            .any(|f| f["id"] == id && f["sessionID"] == *sid);
        let mut message = None;
        let mut error = None;
        if !owned {
            message = Some("此提问已处理或失效。".to_owned());
        } else {
            let index = state["index"].as_u64().context("进度缺失")? as usize;
            match payload["operation"].as_str() {
                Some("cancel") => {
                    self.agents.answer_form(sid, &id, None).await?;
                    message = Some("已取消提问。".into());
                }
                Some("restart") => {
                    state["answers"] = json!({});
                    state["index"] = json!(
                        forms::next(&state["form"], &state["answers"], 0)
                            .unwrap_or(state["form"]["fields"].as_array().map_or(0, Vec::len))
                    );
                }
                Some("external_next") => {
                    ensure!(
                        state["form"]["fields"][index]["type"] == "external",
                        "此项不是外部操作"
                    );
                    state["index"] = json!(
                        forms::next(&state["form"], &state["answers"], index + 1)
                            .unwrap_or(state["form"]["fields"].as_array().map_or(0, Vec::len))
                    );
                }
                Some("next" | "skip") => {
                    let field = state["form"]["fields"]
                        .get(index)
                        .context("字段已失效")?
                        .clone();
                    let answer = if payload["operation"] == "skip" {
                        ensure!(field["required"] != true, "此项必填");
                        Ok(None)
                    } else {
                        forms::answer(&field, &payload["input"]).map(Some)
                    };
                    match answer {
                        Ok(value) => {
                            if let Some(value) = value {
                                let key = field["key"].as_str().context("字段标识缺失")?;
                                state["answers"][key] = value;
                            }
                            state["index"] = json!(
                                forms::next(&state["form"], &state["answers"], index + 1)
                                    .unwrap_or(
                                        state["form"]["fields"].as_array().map_or(0, Vec::len)
                                    )
                            );
                        }
                        Err(e) => error = Some(e.to_string()),
                    }
                }
                Some("submit") => {
                    ensure!(
                        index
                            >= state["form"]["fields"]
                                .as_array()
                                .context("字段缺失")?
                                .len(),
                        "请先完成填写"
                    );
                    // OpenCode is authoritative for pattern/format and other server constraints.
                    match self
                        .agents
                        .answer_form(sid, &id, Some(state["answers"].clone()))
                        .await
                    {
                        Ok(_) => message = Some("答案已提交，正在继续任务。".into()),
                        Err(e) => error = Some(format!("提交未成功：{e}。可重新填写后再提交。")),
                    }
                }
                _ => anyhow::bail!("未知卡片操作"),
            }
        }
        state["revision"] = json!(state["revision"].as_u64().unwrap_or(0) + 1);
        let card = if let Some(text) = message {
            state["done"] = json!(true);
            forms::status("提问处理结果", &text)
        } else {
            forms::card(&state, token, error.as_deref())?
        };
        state["card"] = card;
        self.store.finish_form_card(&j.id, token, &state)?;
        Ok(())
    }
    async fn close_resolved_forms(&self) -> Result<()> {
        for token in self.store.open_form_tokens()? {
            let Some(saved) = self.store.form_card(&token)? else {
                continue;
            };
            if saved.message_id.is_empty() {
                continue;
            }
            let same_session =
                self.session(&saved.origin_key)?.as_deref() == Some(saved.session.as_str());
            let pending = if same_session {
                self.agents.forms(&saved.session).await?
            } else {
                json!([])
            };
            if pending
                .as_array()
                .context("提问列表无效")?
                .iter()
                .any(|f| f["id"] == saved.state["form"]["id"] && f["sessionID"] == saved.session)
            {
                continue;
            }
            let card = forms::status("提问已结束", "此提问已处理、取消或随任务结束而失效。");
            self.feishu.update_card(&saved.message_id, &card).await?;
            let mut state = saved.state;
            state["done"] = json!(true);
            state["card"] = card;
            self.store
                .save_form_card(&token, &saved.origin_key, &saved.session, &state)?;
        }
        Ok(())
    }
    async fn notify_pending(&self, j: &Job, seen: &mut HashSet<String>) -> Result<()> {
        let Some(sid) = self.session(&j.key)? else {
            return Ok(());
        };
        self.agents.approve_pending(&sid).await?;
        let list = self.agents.forms(&sid).await?;
        for p in list.as_array().context("OpenCode 待处理提问格式不兼容")? {
            if p["sessionID"] != sid {
                continue;
            }
            let id = p["id"].as_str().context("待处理提问缺少 ID")?;
            if seen.contains(id) {
                continue;
            }
            ensure!(
                !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "表单 ID 无效"
            );
            let token = uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_OID,
                format!("form:{}:{sid}:{id}", j.key).as_bytes(),
            )
            .to_string();
            let saved = self.store.form_card(&token)?;
            if saved.as_ref().is_some_and(|c| !c.message_id.is_empty()) {
                seen.insert(id.into());
                continue;
            }
            let state=saved.map(|c|c.state).unwrap_or_else(||json!({"form":p,"answers":{},"index":forms::next(p,&json!({}),0).unwrap_or(p["fields"].as_array().map_or(0,Vec::len)),"revision":0}));
            self.store.save_form_card(&token, &j.key, &sid, &state)?;
            let card = forms::card(&state, &token, None)?;
            let message = self
                .feishu
                .reply_card(&destination(j)?, &card, &token, false)
                .await?;
            self.store.attach_form_card(&token, &message)?;
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
        if !payload["assets"].is_array() {
            let frozen = if let Some(path) = payload["snapshot"].as_str() {
                Some((
                    std::path::PathBuf::from(path),
                    payload["captured_at"].as_u64().unwrap_or(0),
                ))
            } else {
                crate::posture::freeze_report_frame(&self.cfg, &j.key, &j.id)?
            };
            payload["assets"] = json!([]);
            if let Some((path, at)) = frozen {
                payload["assets"].as_array_mut().unwrap().push(json!({"kind":"posture","path":path,"label":format!("最近一次坐姿检测图（含关键点和连线，{}）",capture_time(at))}));
            } else {
                payload["note"] = json!(format!(
                    "{}\n本批未取得近 10 分钟内的坐姿检测图。",
                    payload["note"].as_str().unwrap_or("")
                ));
            }
            self.store
                .finish_output(&j.id, "posture_report", &payload)?;
        }
        if payload["tablet_done"] != true {
            let capture = match &self.tablet {
                Some(tablet) => tablet.report_capture(&j.id).unwrap_or_else(
                    |e| json!({"status":"failed","error":format!("平板采集失败：{e}")}),
                ),
                None => json!({"status":"failed","error":"平板网关未启用"}),
            };
            if capture["status"] == "pending" {
                return Ok(());
            }
            let mut note = payload["note"].as_str().unwrap_or("").to_owned();
            if capture["status"] == "ready" {
                let at = capture_time(capture["captured_at"].as_u64().unwrap_or(0));
                let images = capture["images"].as_array().context("平板汇总图片缺失")?;
                for image in images {
                    let label = match image["name"].as_str() {
                        Some("image") => "平板照片",
                        Some("screenshot") => "平板当前屏幕截图",
                        _ => continue,
                    };
                    payload["assets"].as_array_mut().unwrap().push(json!({"kind":"tablet","device":capture["device"],"path":image["path"],"label":format!("{label}（{at}）")}));
                }
                if !images.iter().any(|image| image["name"] == "screenshot") {
                    note.push_str(&format!(
                        "\n平板屏幕截图未取得：{}",
                        capture["screenshot_error"]
                            .as_str()
                            .filter(|e| !e.is_empty())
                            .unwrap_or("平板未返回截图")
                    ));
                }
            } else {
                note.push_str(&format!(
                    "\n{}",
                    capture["error"]
                        .as_str()
                        .unwrap_or("平板照片和屏幕截图未取得")
                ));
            }
            payload["note"] = json!(note);
            payload["tablet_done"] = json!(true);
            self.store
                .finish_output(&j.id, "posture_report", &payload)?;
        }
        for i in 0..payload["assets"].as_array().unwrap().len() {
            if payload["assets"][i]["image_key"].is_string() {
                continue;
            }
            let asset = &payload["assets"][i];
            let path =
                std::path::PathBuf::from(asset["path"].as_str().context("汇总图片路径缺失")?);
            if asset["kind"] == "posture" {
                let expected = crate::posture::report_frame_path(&self.cfg, &j.key, &j.id)?;
                ensure!(
                    path == expected || path == expected.with_extension("jpg"),
                    "坐姿汇总图片路径不匹配"
                );
            } else {
                self.tablet
                    .as_ref()
                    .context("平板网关未启用")?
                    .validate_report_image(
                        asset["device"].as_str().context("平板设备缺失")?,
                        &path,
                    )?;
            }
            let key = self.feishu.upload_camera_media(&path, false, 0).await?;
            payload["assets"][i]["image_key"] = json!(key);
            self.store
                .finish_output(&j.id, "posture_report", &payload)?;
            // Keep the bridge heartbeat/control loop responsive between media uploads.
            return Ok(());
        }
        let images: Vec<(String, String)> = payload["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|asset| {
                Ok((
                    asset["label"].as_str().context("图片说明缺失")?.into(),
                    asset["image_key"]
                        .as_str()
                        .context("图片上传标识缺失")?
                        .into(),
                ))
            })
            .collect::<Result<_>>()?;
        let note = payload["note"].as_str().context("汇总文字缺失")?;
        if images.is_empty() {
            self.store.finish(&j.id, note)?;
        } else {
            self.store
                .finish_output(&j.id, "camera_media", &report_message(note, &images))?;
        }
        for asset in payload["assets"].as_array().unwrap() {
            if asset["kind"] == "posture"
                && let Some(path) = asset["path"].as_str()
            {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }

    async fn deliver_rich_reply(&self, j: &Job) -> Result<()> {
        let mut out: crate::reply::Output = serde_json::from_str(&j.result)?;
        let Some(packet) = out.packets.get(j.sent) else {
            self.store.status(&j.id, "done")?;
            return Ok(());
        };
        let uuid = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("rich:{}:{}:{}", j.id, j.sent, packet.kind).as_bytes(),
        )
        .to_string();
        let result: Result<()> = async {
            match packet.kind.as_str() {
                "text" => {
                    self.feishu
                        .reply(
                            &destination(j)?,
                            packet.content["text"].as_str().context("回复文字缺失")?,
                            &uuid,
                            false,
                        )
                        .await?
                }
                "interactive" => {
                    let message = self
                        .feishu
                        .reply_card(&destination(j)?, &packet.content, &uuid, false)
                        .await?;
                    if !packet.token.is_empty() {
                        self.store.attach_reply_card(&packet.token, &message)?;
                    }
                }
                "post" | "image" | "media" => {
                    self.feishu
                        .reply_camera_media(
                            &destination(j)?,
                            &json!({"msg_type":packet.kind,"content":packet.content}),
                            &uuid,
                            false,
                        )
                        .await?
                }
                _ => anyhow::bail!("未知富消息类型"),
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            // Only an explicit API rejection allows fallback. A network timeout may have sent the card.
            if packet.kind != "text" && e.downcast_ref::<crate::feishu::MessageRejected>().is_some()
            {
                if !packet.token.is_empty() {
                    self.store.delete_reply_card(&packet.token)?;
                }
                let text = if j.sent == 0 {
                    out.fallback.clone()
                } else {
                    "附件暂时无法显示。".into()
                };
                let packets = chunks(&text).into_iter().map(|text| crate::reply::Packet {
                    kind: "text".into(),
                    content: json!({"text":text}),
                    token: String::new(),
                });
                out.packets.splice(j.sent..j.sent + 1, packets);
                self.store
                    .finish_output(&j.id, "rich_reply", &serde_json::to_value(&out)?)?;
                tracing::warn!(error=%e,"飞书拒绝富消息，已降级为文字");
                return Ok(());
            }
            self.store.retry_delivery(&j.id)?;
            return Err(e);
        }
        self.store.sent(&j.id, j.sent + 1)?;
        Ok(())
    }

    async fn deliver(&self) -> Result<()> {
        // One durable result per tick; failed sends remain in the outbox for retry.
        if let Some(j) = self.store.jobs("replying")?.first() {
            let kind = self.store.output_kind(&j.id)?;
            if kind == "rich_reply" {
                return self.deliver_rich_reply(j).await;
            }
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
            if kind == "form_card_update" {
                let payload: Value = serde_json::from_str(&j.result)?;
                let card = self
                    .store
                    .form_card(payload["token"].as_str().context("卡片标识缺失")?)?
                    .context("卡片已失效")?
                    .state["card"]
                    .clone();
                if let Err(e) = self.feishu.update_card(&j.message_id, &card).await {
                    self.store.retry_delivery(&j.id)?;
                    return Err(e);
                }
                self.store.status(&j.id, "done")?;
                return Ok(());
            }
            if kind == "agent_card" || kind == "agent_card_update" {
                let mut payload: Value = serde_json::from_str(&j.result)?;
                let current = self.selected_agent(&j.key)?;
                let token = if kind == "agent_card" {
                    payload["token"].as_str()
                } else {
                    payload["body"]["elements"][1]["behaviors"][0]["value"]["token"].as_str()
                }
                .context("卡片标识缺失")?
                .to_owned();
                let card = crate::agent::picker(current, &token, kind == "agent_card_update");
                let result = if kind == "agent_card" {
                    payload["card"] = card;
                    let uuid = uuid::Uuid::new_v5(
                        &uuid::Uuid::NAMESPACE_OID,
                        format!("result:{}:agent-card", j.id).as_bytes(),
                    )
                    .to_string();
                    match self
                        .feishu
                        .reply_card(&destination(j)?, &payload["card"], &uuid, false)
                        .await
                    {
                        Ok(message) => self.store.attach_agent_card(&token, &message),
                        Err(e) => Err(e),
                    }
                } else {
                    self.feishu.update_card(&j.message_id, &card).await
                };
                if let Err(e) = result {
                    self.store.retry_delivery(&j.id)?;
                    return Err(e);
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
    async fn finish_stopped_task(
        &self,
        key: &str,
        active: &mut Option<(Job, JoinHandle<Result<()>>)>,
    ) -> Result<()> {
        if active.as_ref().is_some_and(|(job, _)| job.key == key) {
            let (job, handle) = active.take().unwrap();
            // Called only after /stop has confirmed backend termination. Before a
            // session ID exists no model prompt has been sent; dropping creation
            // also drops its supervised process and cancels a pending login.
            handle.abort();
            let _ = handle.await;
            self.store.finish(&job.id, "任务已由 /stop 停止。")?;
        }
        Ok(())
    }
    pub async fn run(self: Arc<Self>) -> Result<()> {
        self.recover().await?;
        let mut active: Option<(Job, JoinHandle<Result<()>>)> = None;
        let mut camera_active: Option<(Job, JoinHandle<Result<()>>)> = None;
        let mut speech_active: Option<(Job, JoinHandle<Result<()>>)> = None;
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
            if speech_active.as_ref().is_some_and(|(_, h)| h.is_finished()) {
                let (j, h) = speech_active.take().unwrap();
                if let Err(e) = h.await.context("语音执行器异常退出").and_then(|r| r) {
                    self.store.finish(&j.id, &format!("播报失败：{e}"))?;
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
            if speech_active.is_none()
                && let Some(j) = queued.iter().find(|j| speech::text(&j.text).is_some())
            {
                self.store.status(&j.id, "running")?;
                let b = self.clone();
                let job = j.clone();
                speech_active = Some((
                    j.clone(),
                    tokio::spawn(async move { b.speak_job(&job).await }),
                ));
            }
            // Commands stay responsive while the sole model task waits for an approval.
            for j in queued
                .iter()
                .filter(|j| {
                    is_command(j) && !camera::is_capture(&j.text) && speech::text(&j.text).is_none()
                })
                .take(1)
            {
                if (speech::is_stop(&j.text) || j.text.trim() == "/stop")
                    && speech_active.as_ref().is_some_and(|(s, _)| s.key == j.key)
                {
                    let (s, h) = speech_active.take().unwrap();
                    h.abort();
                    let _ = h.await;
                    self.store.finish(&s.id, "语音播报已停止。")?;
                }
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
                match self
                    .handle_command(j, active.as_ref().map(|(j, _)| j))
                    .await
                {
                    Ok(()) if j.text.trim() == "/stop" => {
                        self.finish_stopped_task(&j.key, &mut active).await?
                    }
                    Ok(()) => {}
                    Err(e) => self.store.finish(&j.id, &format!("操作失败：{e}"))?,
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
            if count.is_multiple_of(5)
                && let Err(e) = self.close_resolved_forms().await
            {
                tracing::warn!(error=%e,"提问卡片状态同步失败，将重试");
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
    #[tokio::test]
    async fn rich_outbox_binds_cards_and_falls_back_only_after_explicit_rejection()
    -> anyhow::Result<()> {
        use super::*;
        use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
        use std::sync::Mutex;
        struct Mock {
            mode: &'static str,
            messages: Mutex<Vec<Value>>,
        }
        for mode in ["success", "rejected", "http_failure"] {
            let mock = Arc::new(Mock {
                mode,
                messages: Mutex::new(Vec::new()),
            });
            let app = Router::new()
                .route(
                    "/open-apis/auth/v3/tenant_access_token/internal",
                    post(|| async {
                        Json(json!({"code":0,"tenant_access_token":"fake","expire":3600}))
                    }),
                )
                .route(
                    "/open-apis/im/v1/messages",
                    post(
                        |State(m): State<Arc<Mock>>, Json(body): Json<Value>| async move {
                            m.messages.lock().unwrap().push(body.clone());
                            if body["msg_type"] == "interactive" && m.mode == "http_failure" {
                                return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"code":1})));
                            }
                            if body["msg_type"] == "interactive" && m.mode == "rejected" {
                                return (StatusCode::OK, Json(json!({"code":230099})));
                            }
                            (
                                StatusCode::OK,
                                Json(json!({"code":0,"data":{"message_id":"om_sent_card"}})),
                            )
                        },
                    ),
                )
                .with_state(mock.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let mut cfg = Config::fixture();
            cfg.feishu_base_url = format!("http://{}", listener.local_addr()?);
            let cfg = Arc::new(cfg);
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let store = Arc::new(Store::open(std::path::Path::new(":memory:"))?);
            let b = Bridge {
                cfg: cfg.clone(),
                store: store.clone(),
                feishu: Arc::new(Feishu::new(cfg.clone())?),
                agents: Arc::new(AgentHub::new(cfg.clone())?),
                tablet: None,
            };
            let j = Job {
                id: format!("rich-{mode}"),
                key: json!([cfg.workspace, "ou_owner", "oc_chat", "root"]).to_string(),
                message_id: "om_original".into(),
                text: "问问题".into(),
                result: String::new(),
                sent: 0,
            };
            store.insert(&j)?;
            crate::reply::finish(&store,&j,&json!({"feishu_reply":1,"layout":"card","text":"要提醒宝宝吗？","actions":[{"id":"yes","label":"提醒","prompt":"请提醒宝宝写作业。"}]}).to_string())?;
            let pending = store.jobs("replying")?.remove(0);
            let output: crate::reply::Output = serde_json::from_str(&pending.result)?;
            let token = &output.packets[0].token;
            let sent = b.deliver_rich_reply(&pending).await;
            if mode == "success" {
                sent?;
                assert_eq!(
                    store.reply_card(token, "")?.unwrap().message_id,
                    "om_sent_card"
                );
                b.deliver_rich_reply(&store.jobs("replying")?.remove(0))
                    .await?;
                assert_eq!(
                    store.command_task(&j.key, &j.id)?.unwrap()["status"],
                    "done"
                );
                assert_eq!(mock.messages.lock().unwrap().len(), 1);
            } else if mode == "rejected" {
                sent?;
                assert!(store.reply_card(token, "")?.is_none());
                let pending = store.jobs("replying")?.remove(0);
                b.deliver_rich_reply(&pending).await?;
                let calls = mock.messages.lock().unwrap();
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[1]["msg_type"], "text");
                assert_ne!(calls[0]["uuid"], calls[1]["uuid"]);
                let text: Value = serde_json::from_str(calls[1]["content"].as_str().unwrap())?;
                assert_eq!(text["text"], "要提醒宝宝吗？");
            } else {
                assert!(sent.is_err());
                let pending = store.command_task(&j.key, &j.id)?.unwrap();
                let out: crate::reply::Output =
                    serde_json::from_str(pending["result"].as_str().unwrap())?;
                assert_eq!(out.packets[0].kind, "interactive");
                assert_eq!(mock.messages.lock().unwrap().len(), 1);
            }
            server.abort();
            let _ = server.await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn stop_cancels_initial_agent_creation_before_a_session_exists() -> anyhow::Result<()> {
        use super::*;
        let cfg = Arc::new(Config::fixture());
        let store = Arc::new(Store::open(std::path::Path::new(":memory:"))?);
        let b = Bridge {
            cfg: cfg.clone(),
            store: store.clone(),
            feishu: Arc::new(Feishu::new(cfg.clone())?),
            agents: Arc::new(AgentHub::new(cfg.clone())?),
            tablet: None,
        };
        let key = json!([cfg.workspace, "ou_owner", "oc_chat", "root"]).to_string();
        let job = Job {
            id: "starting".into(),
            key: key.clone(),
            message_id: "om_job".into(),
            text: "hello".into(),
            result: String::new(),
            sent: 0,
        };
        store.insert(&job)?;
        store.status(&job.id, "running")?;
        let mut active = Some((
            job.clone(),
            tokio::spawn(std::future::pending::<Result<()>>()),
        ));
        b.finish_stopped_task("other-owner", &mut active).await?;
        assert!(active.is_some());
        let stop = Job {
            id: "stop".into(),
            text: "/stop".into(),
            ..job.clone()
        };
        b.command(&stop, Some(&job)).await?;
        b.finish_stopped_task(&key, &mut active).await?;
        assert!(active.is_none());
        assert!(store.jobs("running")?.is_empty());
        assert!(
            store
                .jobs("replying")?
                .iter()
                .any(|j| j.id == job.id && j.result.contains("停止"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn agent_card_switches_only_when_chat_is_idle_and_restores_each_session()
    -> anyhow::Result<()> {
        use super::*;
        let cfg = Arc::new(Config::fixture());
        let store = Arc::new(Store::open(std::path::Path::new(":memory:"))?);
        let b = Bridge {
            cfg: cfg.clone(),
            store: store.clone(),
            feishu: Arc::new(Feishu::new(cfg.clone())?),
            agents: Arc::new(AgentHub::new(cfg.clone())?),
            tablet: None,
        };
        let key = json!([cfg.workspace, "ou_owner", "oc_chat", "root"]).to_string();
        let j = Job {
            id: "command".into(),
            key: key.clone(),
            message_id: "om_source".into(),
            text: "/agent".into(),
            result: String::new(),
            sent: 0,
        };
        store.insert(&j)?;
        b.handle_command(&j, None).await?;
        let out = store.jobs("replying")?.remove(0);
        let payload: Value = serde_json::from_str(&out.result)?;
        assert_eq!(
            payload["card"]["body"]["elements"][1]["options"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        let token = payload["token"].as_str().unwrap();
        store.attach_agent_card(token, "om_agent")?;
        let switch = Job {
            id: "card:choose".into(),
            key: key.clone(),
            message_id: "om_agent".into(),
            text: format!("/_agent-select {token} antigravity"),
            result: String::new(),
            sent: 0,
        };
        store.insert(&switch)?;
        assert!(b.handle_command(&switch, Some(&j)).await.is_err());
        assert_eq!(b.selected_agent(&key)?, Kind::OpenCode);
        store.set_session(&key, "ses_original")?;
        b.handle_command(&switch, None).await?;
        assert_eq!(b.selected_agent(&key)?, Kind::Antigravity);
        assert!(b.session(&key)?.is_none());
        store.set_agent_session(&key, "antigravity", "agent_antigravity_test")?;
        assert_eq!(b.session(&key)?.as_deref(), Some("agent_antigravity_test"));
        let back = Job {
            text: format!("/_agent-select {token} opencode"),
            ..switch.clone()
        };
        b.handle_command(&back, None).await?;
        assert_eq!(b.session(&key)?.as_deref(), Some("ses_original"));
        Ok(())
    }

    use super::*;
    #[tokio::test]
    async fn slash_commands_use_workspace_metadata_and_cancel_only_owned_speech() -> Result<()> {
        use axum::{Json, Router, extract::Query, routing::get};
        use std::collections::HashMap;
        let app=Router::new()
            .route("/api/skill",get(|Query(query):Query<HashMap<String,String>>|async move{
                assert!(query.contains_key("location[directory]"));
                Json(json!({"data":[{"name":"weather","description":"查询天气","content":"private-content"}]}))
            }))
            .route("/api/mcp",get(|Query(query):Query<HashMap<String,String>>|async move{
                assert!(query.contains_key("location[directory]"));
                Json(json!({"data":[{"name":"homework","status":{"status":"connected"}}]}))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let mut cfg = Config::fixture();
        cfg.opencode_url = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let cfg = Arc::new(cfg);
        let store = Arc::new(Store::open(std::path::Path::new(":memory:"))?);
        let b = Bridge {
            cfg: cfg.clone(),
            store: store.clone(),
            feishu: Arc::new(Feishu::new(cfg.clone())?),
            agents: Arc::new(AgentHub::new(cfg)?),
            tablet: None,
        };
        let key = json!(["workspace", "owner", "chat", "topic"]).to_string();
        let mut job = Job {
            id: "command".into(),
            key: key.clone(),
            message_id: "om_command".into(),
            text: "/skills".into(),
            result: String::new(),
            sent: 0,
        };
        let skills = b.command(&job, None).await?;
        assert!(skills.contains("weather"));
        assert!(!skills.contains("private-content"));
        job.text = "/mcp".into();
        assert!(b.command(&job, None).await?.contains("connected"));
        job.text = "/cron list".into();
        assert!(b.command(&job, None).await?.contains("坐姿监测"));
        job.text = "/channel".into();
        assert!(b.command(&job, None).await?.contains("平板网关：未启用"));
        for (id, k) in [
            ("own-speech", key.as_str()),
            ("foreign-speech", "other-conversation"),
        ] {
            store.insert(&Job {
                id: id.into(),
                key: k.into(),
                message_id: "om_test".into(),
                text: "/speak 你好".into(),
                result: String::new(),
                sent: 0,
            })?;
        }
        job.text = "/tasks".into();
        let tasks = b.command(&job, None).await?;
        assert!(tasks.contains("own-speech"));
        assert!(!tasks.contains("foreign-speech"));
        job.text = "/task status foreign-speech".into();
        assert!(b.command(&job, None).await.is_err());
        job.text = "/speak   stop".into();
        b.command(&job, None).await?;
        assert_eq!(
            store.command_task(&key, "own-speech")?.unwrap()["status"],
            "replying"
        );
        assert_eq!(
            store
                .command_task("other-conversation", "foreign-speech")?
                .unwrap()["status"],
            "queued"
        );
        server.abort();
        Ok(())
    }
    #[tokio::test]
    async fn notices_can_be_managed_across_topics_but_not_users_or_chats() -> Result<()> {
        let mut cfg = Config::fixture();
        cfg.data_dir =
            std::env::temp_dir().join(format!("mr-li-notify-command-{}", uuid::Uuid::new_v4()));
        let cfg = Arc::new(cfg);
        let tablet = crate::tablet::Gateway::open(cfg.clone())?;
        let first_key = json!(["workspace", "parent", "chat", "topic-one"]).to_string();
        let owner = models::scope(&first_key)?;
        let id = tablet.notices.publish(
            &owner,
            "source",
            "tablet",
            "明天带练习册",
            crate::posture::now_epoch() as i64,
        )?;
        let b = Bridge {
            cfg: cfg.clone(),
            store: Arc::new(Store::open(std::path::Path::new(":memory:"))?),
            agents: Arc::new(AgentHub::new(cfg.clone())?),
            feishu: Arc::new(Feishu::new(cfg.clone())?),
            tablet: Some(tablet.clone()),
        };
        let mut j = Job {
            id: "query".into(),
            key: json!(["workspace", "parent", "chat", "topic-two"]).to_string(),
            message_id: "om_query".into(),
            text: "/notify list".into(),
            result: String::new(),
            sent: 0,
        };
        assert!(b.command(&j, None).await?.contains("明天带练习册"));
        j.text = "/baby list".into();
        assert!(b.command(&j, None).await?.contains("明天带练习册"));
        j.key = json!(["workspace", "other-parent", "chat", "topic-two"]).to_string();
        assert!(!b.command(&j, None).await?.contains("明天带练习册"));
        j.text = format!("/baby cancel {id}");
        assert!(b.command(&j, None).await.is_err());
        j.key = json!(["workspace", "parent", "other-chat", "topic-two"]).to_string();
        assert!(b.command(&j, None).await.is_err());
        j.key = first_key;
        b.command(&j, None).await?;
        assert!(
            tablet
                .notices
                .pending(crate::posture::now_epoch() as i64)?
                .is_empty()
        );
        j.text = "/baby".into();
        assert!(b.command(&j, None).await?.contains("/baby <内容>"));
        let db =
            rusqlite::Connection::open(cfg.data_dir.join("tablet/teacher-devices-v1.sqlite3"))?;
        db.execute(
            "INSERT INTO devices VALUES ('tablet','学习平板','test-token')",
            [],
        )?;
        j.id = "baby-message".into();
        j.text = "/baby 宝宝，休息一下\n记得喝水！".into();
        let reply = b.command(&j, None).await?;
        assert!(reply.contains("等待平板确认送达"));
        assert!(reply.contains("/baby list"));
        // Retrying the same incoming message must not publish twice.
        assert_eq!(b.command(&j, None).await?, reply);
        let pending = tablet.notices.pending(crate::posture::now_epoch() as i64)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["device"], "tablet");
        assert_eq!(pending[0]["body"], "宝宝，休息一下\n记得喝水！");
        drop(db);
        drop(b);
        drop(tablet);
        std::fs::remove_dir_all(&cfg.data_dir)?;
        Ok(())
    }
    #[test]
    fn report_post_contains_summary_and_three_distinct_labeled_images() {
        let message = report_message(
            "十分钟坐姿汇总\n20:10 · 未触发提醒",
            &[
                ("坐姿检测图".into(), "img_pose".into()),
                ("平板照片".into(), "img_photo".into()),
                ("平板当前屏幕截图".into(), "img_screen".into()),
            ],
        );
        assert_eq!(message["msg_type"], "post");
        let rows = message["content"]["zh_cn"]["content"].as_array().unwrap();
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[3][0]["image_key"], "img_pose");
        assert_eq!(rows[4][0]["text"], "平板照片");
        assert_eq!(rows[5][0]["image_key"], "img_photo");
        assert_eq!(rows[6][0]["text"], "平板当前屏幕截图");
        assert_eq!(rows[7][0]["image_key"], "img_screen");
        assert!(message["note"].is_null());
    }
    #[tokio::test]
    async fn report_upload_retry_preserves_images_and_sends_one_post() -> Result<()> {
        use axum::{
            Json, Router,
            body::Bytes,
            extract::{Query, State},
            http::StatusCode,
            routing::post,
        };
        use std::{
            collections::HashMap,
            sync::{
                Mutex,
                atomic::{AtomicUsize, Ordering},
            },
        };
        #[derive(Default)]
        struct Mock {
            uploads: AtomicUsize,
            messages: Mutex<Vec<Value>>,
        }
        let mock = Arc::new(Mock::default());
        let app = Router::new()
            .route(
                "/open-apis/auth/v3/tenant_access_token/internal",
                post(|| async {
                    Json(json!({"code":0,"tenant_access_token":"test-token","expire":3600}))
                }),
            )
            .route(
                "/open-apis/im/v1/images",
                post(|State(mock): State<Arc<Mock>>, body: Bytes| async move {
                    assert!(!body.is_empty());
                    let n = mock.uploads.fetch_add(1, Ordering::SeqCst) + 1;
                    if n == 2 {
                        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"code":1})))
                    } else {
                        (
                            StatusCode::OK,
                            Json(json!({"code":0,"data":{"image_key":format!("img_{n}")}})),
                        )
                    }
                }),
            )
            .route(
                "/open-apis/im/v1/messages",
                post(
                    |State(mock): State<Arc<Mock>>,
                     Query(query): Query<HashMap<String, String>>,
                     Json(body): Json<Value>| async move {
                        assert_eq!(query["receive_id_type"], "chat_id");
                        assert_eq!(body["receive_id"], "oc_report");
                        assert_eq!(body["msg_type"], "post");
                        mock.messages.lock().unwrap().push(body);
                        Json(json!({"code":0,"data":{"message_id":"om_report"}}))
                    },
                ),
            )
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let mut cfg = Config::fixture();
        cfg.feishu_base_url = format!("http://{}", listener.local_addr()?);
        cfg.data_dir = std::env::temp_dir().join(format!("mr-li-report-{}", uuid::Uuid::new_v4()));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let cfg = Arc::new(cfg);
        let tablet = crate::tablet::Gateway::open(cfg.clone())?;
        let store = Arc::new(Store::open(std::path::Path::new(":memory:"))?);
        let b = Bridge {
            cfg: cfg.clone(),
            tablet: Some(tablet),
            store: store.clone(),
            feishu: Arc::new(Feishu::new(cfg.clone())?),
            agents: Arc::new(AgentHub::new(cfg.clone())?),
        };
        let j = Job {
            id: "posture-report:test".into(),
            key: json!(["workspace", "owner", "oc_report", "thread"]).to_string(),
            message_id: "om_source".into(),
            text: "/posture report".into(),
            result: String::new(),
            sent: 0,
        };
        let pose = crate::posture::report_frame_path(&cfg, &j.key, &j.id)?;
        std::fs::create_dir_all(pose.parent().unwrap())?;
        std::fs::write(&pose, b"pose image")?;
        let media = cfg.data_dir.join("tablet/media/test-tablet");
        std::fs::create_dir_all(&media)?;
        let photo = media.join(format!("{}.jpg", uuid::Uuid::new_v4()));
        let screenshot = media.join(format!("{}.jpg", uuid::Uuid::new_v4()));
        std::fs::write(&photo, b"tablet photo")?;
        std::fs::write(&screenshot, b"tablet screen")?;
        store.insert(&j)?;
        store.finish_output(&j.id,"posture_report",&json!({"note":"十分钟汇总","tablet_done":true,"assets":[
            {"kind":"posture","path":pose,"label":"坐姿检测图"},
            {"kind":"tablet","path":photo,"device":"test-tablet","label":"平板照片"},
            {"kind":"tablet","path":screenshot,"device":"test-tablet","label":"平板当前屏幕截图"}
        ]}))?;
        b.prepare_posture_report(&store.jobs("replying")?[0])
            .await?;
        assert!(
            b.prepare_posture_report(&store.jobs("replying")?[0])
                .await
                .is_err()
        );
        let staged: Value = serde_json::from_str(&store.jobs("replying")?[0].result)?;
        assert_eq!(staged["assets"][0]["image_key"], "img_1");
        for _ in 0..3 {
            b.prepare_posture_report(&store.jobs("replying")?[0])
                .await?;
        }
        assert_eq!(store.output_kind(&j.id)?, "camera_media");
        assert_eq!(mock.uploads.load(Ordering::SeqCst), 4);
        assert!(!pose.exists());
        assert!(photo.exists() && screenshot.exists());
        b.deliver().await?;
        b.deliver().await?;
        assert_eq!(mock.messages.lock().unwrap().len(), 1);
        let content: Value = serde_json::from_str(
            mock.messages.lock().unwrap()[0]["content"]
                .as_str()
                .unwrap(),
        )?;
        let rows = content["zh_cn"]["content"].as_array().unwrap();
        let keys: Vec<_> = rows
            .iter()
            .filter_map(|row| row[0]["image_key"].as_str())
            .collect();
        assert_eq!(keys, vec!["img_1", "img_3", "img_4"]);
        server.abort();
        std::fs::remove_dir_all(&cfg.data_dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn form_card_round_trip_rejects_invalid_and_stale_clicks() -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let form = json!({"id":"frm_test","sessionID":"ses_owner","title":"搜索设置","fields":[{"key":"choice","type":"string","required":true,"custom":false,"options":[{"label":"允许","value":"allow"},{"label":"禁用","value":"disable"}]}]});
        let server_form = form.clone();
        let server = tokio::spawn(async move {
            for i in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![];
                let mut buf = [0u8; 4096];
                let end = loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(i) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let route = headers.lines().next().unwrap().to_owned();
                let len = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < end + len {
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                }
                let body = if i == 3 {
                    assert!(route.starts_with("POST /api/session/ses_owner/form/frm_test/reply "));
                    assert_eq!(
                        serde_json::from_slice::<Value>(&bytes[end..end + len]).unwrap(),
                        json!({"answer":{"choice":"allow"}})
                    );
                    String::new()
                } else {
                    assert!(route.starts_with("GET /api/session/ses_owner/form "));
                    json!({"data":[server_form]}).to_string()
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let mut cfg = Config::fixture();
        cfg.opencode_url = format!("http://{address}");
        let cfg = Arc::new(cfg);
        let store = Arc::new(Store::open(std::path::Path::new(":memory:"))?);
        store.set_session(r#"["/tmp","ou_owner","oc_chat",""]"#, "ses_owner")?;
        store.save_form_card(
            "token",
            r#"["/tmp","ou_owner","oc_chat",""]"#,
            "ses_owner",
            &json!({"form":form,"index":0,"answers":{},"revision":0}),
        )?;
        store.attach_form_card("token", "om_card")?;
        let b = Bridge {
            tablet: None,
            agents: Arc::new(AgentHub::new(cfg.clone())?),
            feishu: Arc::new(Feishu::new(cfg.clone())?),
            cfg,
            store,
        };
        for (i, revision, operation, input) in [
            (0, 0, "next", json!({"answer":"wrong"})),
            (1, 1, "next", json!({"answer":"allow"})),
            (2, 1, "next", json!({"answer":"disable"})),
            (3, 2, "submit", json!({})),
        ] {
            let job = Job {
                id: format!("card:{i}"),
                key: r#"["/tmp","ou_owner","oc_chat",""]"#.into(),
                message_id: "om_card".into(),
                text: format!(
                    "/_form {}",
                    json!({"token":"token","revision":revision,"operation":operation,"input":input})
                ),
                result: String::new(),
                sent: 0,
            };
            b.store.insert(&job)?;
            b.handle_form(&job).await?;
        }
        let state = b.store.form_card("token")?.unwrap().state;
        assert_eq!(state["done"], true);
        assert_eq!(state["answers"], json!({"choice":"allow"}));
        assert!(b.store.open_form_tokens()?.is_empty());
        server.await?;
        Ok(())
    }
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
        store.set_session(r#"["/tmp","ou_owner","oc_chat",""]"#, "ses_owner")?;
        let b = Bridge {
            tablet: None,
            agents: Arc::new(AgentHub::new(cfg.clone())?),
            feishu: Arc::new(Feishu::new(cfg.clone())?),
            cfg,
            store,
        };
        let j = Job {
            id: "om_cmd".into(),
            key: r#"["/tmp","ou_owner","oc_chat",""]"#.into(),
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
