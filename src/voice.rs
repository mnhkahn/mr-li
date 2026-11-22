//! Microphone frontend. Agent choice, sessions and tools belong to the existing bridge.
use crate::{
    config::Config,
    models, speech,
    store::{Job, Store},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashSet, process::Stdio, sync::Arc, time::Duration};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};

#[derive(Clone)]
pub struct Settings {
    python: String,
    helper: String,
    models: String,
    device: String,
    esp32_config: String,
    user: String,
    chat: String,
}
impl Settings {
    pub fn from_settings(get: impl Fn(&str, &str) -> String) -> Self {
        Self {
            python: get("MR_LI_VOICE_PYTHON", ".data/voice/venv/bin/python"),
            helper: get("MR_LI_VOICE_HELPER", "scripts/voice-input.py"),
            models: get("MR_LI_VOICE_MODELS", ".data/voice/models"),
            device: get("MR_LI_VOICE_INPUT_DEVICE", ""),
            esp32_config: get("MR_LI_VOICE_ESP32_CONFIG", ""),
            user: get("MR_LI_VOICE_USER_ID", ""),
            chat: get("MR_LI_VOICE_CHAT_ID", ""),
        }
    }
}
pub fn is_job(j: &Job) -> bool {
    j.id.starts_with("voice:")
}

fn bind(cfg: &Config, origins: Vec<Job>) -> Result<Job> {
    let mut candidates = Vec::new();
    let mut scopes = HashSet::new();
    for j in origins {
        let key: Vec<String> = serde_json::from_str(&j.key)?;
        if key.len() != 4
            || key[0] != cfg.workspace.to_string_lossy()
            || !cfg.allowed.contains(&key[1])
            || (!key[3].is_empty()
                && !cfg.allowed_chats.is_empty()
                && !cfg.allowed_chats.contains(&key[2]))
            || (!cfg.voice.user.is_empty() && key[1] != cfg.voice.user)
            || (!cfg.voice.chat.is_empty() && key[2] != cfg.voice.chat)
        {
            continue;
        }
        if scopes.insert(models::scope(&j.key)?) {
            candidates.push(j);
        }
    }
    ensure!(
        !candidates.is_empty(),
        "没有可绑定的飞书会话，请先在目标聊天发送消息，并检查语音用户/聊天配置"
    );
    ensure!(
        candidates.len() == 1,
        "存在多个用户或聊天，请配置 MR_LI_VOICE_USER_ID 和 MR_LI_VOICE_CHAT_ID，语音不会猜测身份"
    );
    Ok(candidates.remove(0))
}
fn enqueue(store: &Store, origin: &Job, text: &str) -> Result<Job> {
    let text = text.trim();
    ensure!(
        !text.is_empty() && text.len() <= 64 * 1024 && !text.contains('\0'),
        "语音转写文字无效"
    );
    // Transcripts are natural language, never a synthetic slash-command channel.
    ensure!(!text.starts_with('/'), "请用自然语言描述要求");
    let job = Job {
        id: format!("voice:{}", uuid::Uuid::new_v4().simple()),
        key: origin.key.clone(),
        message_id: origin.message_id.clone(),
        text: text.into(),
        result: String::new(),
        sent: 0,
    };
    store.insert(&job)?;
    Ok(job)
}
const RATE_LIMIT_REPLY: &str = "我听到了，但当前模型限流了，请稍后再试。";
const FAILURE_REPLY: &str = "我听到了，但这次处理失败了，请稍后再试。";

fn spoken_result(value: &Value) -> Result<String> {
    let raw = value["result"].as_str().context("缺少任务结果")?;
    if value["kind"] == "voice_error" {
        let limited = raw.contains("HTTP 429")
            || raw.to_ascii_lowercase().contains("rate limit")
            || raw.contains("限流");
        Ok(if limited {
            RATE_LIMIT_REPLY
        } else {
            FAILURE_REPLY
        }
        .into())
    } else if value["kind"] == "rich_reply" {
        let out: crate::reply::Output = serde_json::from_str(raw)?;
        Ok(out.fallback)
    } else {
        Ok(raw.into())
    }
}
#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Ready { wake_word: Option<String> },
    Wake,
    Text { text: String },
    CaptureEnd,
}

const PROGRESS_OPEN: &str = "<voice-progress>";
const PROGRESS_CLOSE: &str = "</voice-progress>";
const ESP32_PROMPT_TIMEOUT: Duration = Duration::from_secs(3);

pub fn progress_updates(output: &str) -> impl Iterator<Item = &str> {
    output.split(PROGRESS_OPEN).skip(1).filter_map(|part| {
        let (text, _) = part.split_once(PROGRESS_CLOSE)?;
        let text = text.trim();
        (!text.is_empty() && text.chars().count() <= 32 && !text.chars().any(char::is_control))
            .then_some(text)
    })
}

pub fn final_reply(mut output: &str) -> String {
    let mut reply = String::new();
    while let Some((before, rest)) = output.split_once(PROGRESS_OPEN) {
        let Some((_, after)) = rest.split_once(PROGRESS_CLOSE) else {
            break;
        };
        reply.push_str(before);
        output = after;
    }
    reply.push_str(output);
    if reply.trim().is_empty() {
        "本次没有返回最终回复。".into()
    } else {
        reply.trim().to_owned()
    }
}

async fn converse(cfg: &Config, store: &Store, origin: &Job, text: &str) -> Result<()> {
    let job = enqueue(store, origin, text)?;
    reply_to_voice(cfg, store, &job, &mut false).await
}

async fn reply_to_voice(
    cfg: &Config,
    store: &Store,
    job: &Job,
    playing_final: &mut bool,
) -> Result<()> {
    // No timeout retry: side effects may already have happened. /stop remains available in Feishu.
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    loop {
        interval.tick().await;
        let value = store
            .command_task(&job.key, &job.id)?
            .context("语音任务丢失")?;
        if matches!(value["status"].as_str(), Some("replying" | "done")) {
            let text = spoken_result(&value)?;
            // Mark before playback so restarting never replays an old answer.
            store.status(&job.id, "done")?;
            *playing_final = true;
            println!("李老师：{text}");
            let chars: Vec<char> = text.chars().collect();
            for part in chars.chunks(500) {
                speech::speak(cfg, &part.iter().collect::<String>()).await?;
            }
            return Ok(());
        }
        bridge_alive(cfg)?;
        if let Some(progress) = store.take_voice_progress(&job.id)?
            && let Err(error) = speech::speak_progress(cfg, &progress).await
        {
            tracing::warn!(%error, "中间进度播报失败，继续等最终回复");
        }
    }
}

async fn read_event(output: &mut (impl AsyncBufRead + Unpin), line: &mut Vec<u8>) -> Result<Event> {
    output
        .take(65537_u64.saturating_sub(line.len() as u64))
        .read_until(b'\n', line)
        .await?;
    ensure!(
        !line.is_empty() && line.len() <= 65536 && line.ends_with(b"\n"),
        "语音输入进程已退出或返回了过大数据，请检查麦克风权限和模型"
    );
    let event = serde_json::from_slice(line).context("语音输入协议无效")?;
    line.clear();
    Ok(event)
}

async fn listen(input: &mut (impl AsyncWrite + Unpin), enabled: bool) -> Result<()> {
    input
        .write_all(if enabled {
            b"{\"listen\":true}\n"
        } else {
            b"{\"listen\":false}\n"
        })
        .await?;
    input.flush().await?;
    Ok(())
}

fn goodbye(text: &str) -> bool {
    matches!(
        text.trim_matches(|c: char| c.is_whitespace() || "，。！？,.!?".contains(c)),
        "结束对话" | "再见" | "停止对话"
    )
}

fn ready(word: Option<&str>) {
    println!(
        "麦克风已就绪，说“{}”开始对话。",
        word.unwrap_or("你好，李老师")
    );
}

async fn esp32_dialogue(
    cfg: &Config,
    store: &Store,
    origin: &Job,
    input: &mut (impl AsyncWrite + Unpin),
    output: &mut (impl AsyncBufRead + Unpin),
) -> Result<()> {
    let mut line = Vec::new();
    let mut pending: Option<Job> = None;
    let mut capturing = false;
    loop {
        let mut playing_final = false;
        let event = if let Some(job) = pending.as_ref().filter(|_| !capturing) {
            tokio::select! { biased;
                event = read_event(output, &mut line) => event?,
                reply = reply_to_voice(cfg, store, job, &mut playing_final) => {
                    pending = None;
                    if let Err(error) = reply {
                        tracing::warn!(%error, "语音回复未能播完");
                    }
                    continue;
                }
            }
        } else {
            read_event(output, &mut line).await?
        };
        if playing_final {
            // A wake interrupted audio. The completed result remains available on the job.
            pending = None;
        }
        match event {
            Event::Ready { wake_word } => ready(wake_word.as_deref()),
            Event::Wake => {
                capturing = true;
                // A busy shared speaker must not consume the board's entire wake window.
                if !matches!(
                    tokio::time::timeout(
                        ESP32_PROMPT_TIMEOUT,
                        speech::speak_posture(cfg, "我在，请说。"),
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    tracing::info!("唤醒提示音未能及时播完，直接开始录音");
                }
                listen(input, true).await?;
            }
            Event::CaptureEnd => capturing = false,
            Event::Text { text } => {
                println!("你：{text}");
                capturing = false;
                // Acknowledge the clip immediately so the helper can poll the next wake.
                listen(input, !goodbye(&text)).await?;
                if goodbye(&text) {
                    pending = None;
                    let _ = tokio::time::timeout(
                        ESP32_PROMPT_TIMEOUT,
                        speech::speak(cfg, "好的，再见。"),
                    )
                    .await;
                } else {
                    // Newest turn gets the speaker. Prior Agent work stays in the bridge queue.
                    pending = Some(enqueue(store, origin, &text)?);
                }
            }
        }
    }
}
fn bridge_alive(cfg: &Config) -> Result<()> {
    let age = std::fs::metadata(cfg.data_dir.join("heartbeat"))
        .context("请先启动李老师 bridge 服务")?
        .modified()?
        .elapsed()?;
    ensure!(
        age < Duration::from_secs(180),
        "bridge 心跳过期，请先恢复服务；已有语音任务不会自动重试"
    );
    Ok(())
}
pub async fn maintain(cfg: Arc<Config>) {
    if cfg.voice.esp32_config.is_empty() {
        return std::future::pending::<()>().await;
    }
    loop {
        if let Err(error) = run(cfg.clone(), false).await {
            tracing::warn!(%error, "ESP32 语音监听中断，稍后重连");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

pub async fn run(cfg: Arc<Config>, check: bool) -> Result<()> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cfg.data_dir.join("voice.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("已有麦克风语音进程")?;
    let mut command = tokio::process::Command::new(&cfg.voice.python);
    command.args([&cfg.voice.helper, "--models", &cfg.voice.models]);
    if !cfg.voice.esp32_config.is_empty() {
        command.args(["--esp32-config", &cfg.voice.esp32_config]);
    } else if !cfg.voice.device.is_empty() {
        command.args(["--device", &cfg.voice.device]);
    }
    if check {
        let status = command
            .arg("--check")
            .status()
            .await
            .context("请先运行 scripts/setup-voice.sh")?;
        ensure!(status.success(), "语音依赖或输入设备检查失败");
        return Ok(());
    }
    bridge_alive(&cfg)?;
    let store = Store::open(&cfg.data_dir.join("mr-li.sqlite3"))?;
    let origin = bind(&cfg, store.voice_origins()?)?;
    tracing::info!(scope=%models::scope(&origin.key)?, "语音沿用此聊天的当前Agent与会话，转写和结果仅在本机输出");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("请先运行 scripts/setup-voice.sh")?;
    let mut input = child.stdin.take().context("语音助手没有stdin")?;
    let mut output = BufReader::new(child.stdout.take().context("语音助手没有stdout")?);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let dialogue = async {
        if !cfg.voice.esp32_config.is_empty() {
            return esp32_dialogue(&cfg, &store, &origin, &mut input, &mut output).await;
        }
        let mut line = Vec::new();
        loop {
            let enabled = match read_event(&mut output, &mut line).await? {
                Event::Ready { wake_word } => {
                    ready(wake_word.as_deref());
                    continue;
                }
                Event::CaptureEnd => continue,
                Event::Wake => {
                    speech::speak_posture(&cfg, "我在，请说。").await?;
                    true
                }
                Event::Text { text } => {
                    println!("你：{text}");
                    if goodbye(&text) {
                        speech::speak(&cfg, "好的，再见。").await?;
                        false
                    } else {
                        // A failed task is never resubmitted automatically.
                        if let Err(e) = converse(&cfg, &store, &origin, &text).await {
                            tracing::error!(error=%e, "语音任务未完成，请在飞书查看状态");
                            speech::speak(&cfg, FAILURE_REPLY).await?;
                            // Avoid accepting more work while the bridge may still be executing.
                            return Err(e);
                        }
                        true
                    }
                }
            };
            // The helper has closed its microphone before emitting wake/text and only reopens
            // after playback finishes. No Agent or TTS implementation is part of the helper.
            tokio::time::sleep(Duration::from_millis(350)).await;
            listen(&mut input, enabled).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::select! {
        r=dialogue=>r,
        _=tokio::signal::ctrl_c()=>Ok(()),
        _=terminate.recv()=>Ok(()),
    };
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn esp32_accepts_a_second_wake_while_previous_agent_is_still_running() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mr-li-wake-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let mut cfg = Config::fixture();
        cfg.data_dir = dir.clone();
        cfg.speech_program = dir.join("speech").to_string_lossy().into();
        std::fs::write(dir.join("heartbeat"), "test")?;
        std::fs::write(
            &cfg.speech_program,
            format!(
                "#!/bin/sh\ncat >/dev/null\n[ ! -f '{}/busy' ] || exec sleep 20\n",
                dir.display()
            ),
        )?;
        std::fs::set_permissions(&cfg.speech_program, std::fs::Permissions::from_mode(0o700))?;
        let store = Store::open(&dir.join("mr-li.sqlite3"))?;
        let origin = origin(&cfg, "ou_owner", "oc_chat");
        let (mut events, output) = tokio::io::duplex(1024);
        let (acks, mut input) = tokio::io::duplex(1024);
        let mut acks = BufReader::new(acks).lines();
        let mut output = BufReader::new(output);
        let exchange = async {
            events.write_all(b"{\"event\":\"wake\"}\n").await?;
            assert_eq!(
                acks.next_line().await?.as_deref(),
                Some("{\"listen\":true}")
            );
            events
                .write_all("{\"event\":\"text\",\"text\":\"第一条命令\"}\n".as_bytes())
                .await?;
            assert_eq!(
                acks.next_line().await?.as_deref(),
                Some("{\"listen\":true}")
            );
            let first = store.jobs("queued")?.into_iter().next().unwrap();
            store.status(&first.id, "running")?;
            std::fs::write(dir.join("busy"), "")?;

            events.write_all(b"{\"event\":\"wake\"}\n").await?;
            assert_eq!(
                acks.next_line().await?.as_deref(),
                Some("{\"listen\":true}")
            );
            events
                .write_all("{\"event\":\"text\",\"text\":\"第二条命令\"}\n".as_bytes())
                .await?;
            assert_eq!(
                acks.next_line().await?.as_deref(),
                Some("{\"listen\":true}")
            );
            assert_eq!(store.jobs("running")?[0].id, first.id);
            assert_eq!(store.jobs("queued")?[0].text, "第二条命令");
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(15), async {
            tokio::select! {
                result = exchange => result,
                result = esp32_dialogue(&cfg, &store, &origin, &mut input, &mut output) => {
                    result?;
                    anyhow::bail!("ESP32 测试在完成前退出");
                }
            }
        })
        .await??;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn frontend_resumes_only_after_configured_speech_and_routes_transcript_through_queue()
    -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mr-li-voice-loop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let mut cfg = Config::fixture();
        cfg.data_dir = dir.clone();
        cfg.voice.python = "python3".into();
        cfg.voice.helper = dir.join("input.py").to_string_lossy().into();
        cfg.speech_program = dir.join("speech").to_string_lossy().into();
        cfg.speak_voice = "configured-clone".into();
        cfg.speak_audio_device = "configured-output".into();
        cfg.posture_voice = "posture-clone".into();
        cfg.posture_audio_device = "posture-output".into();
        std::fs::write(dir.join("heartbeat"), "test")?;
        std::fs::write(
            &cfg.speech_program,
            format!(
                "#!/bin/sh\ncat >> '{0}/spoken'\nprintf '\\n' >> '{0}/spoken'\nprintf '%s\\n' \"$@\" >> '{0}/args'\n",
                dir.display()
            ),
        )?;
        std::fs::set_permissions(&cfg.speech_program, std::fs::Permissions::from_mode(0o700))?;
        std::fs::write(
            &cfg.voice.helper,
            r#"import json, pathlib, sys
root = pathlib.Path(__file__).parent
def emit(event, **kw): print(json.dumps(dict(event=event, **kw)), flush=True)
def resume(expected, spoken):
    assert json.loads(sys.stdin.readline()) == {'listen': expected}
    assert spoken in (root / 'spoken').read_text()
emit('ready')
emit('wake')
resume(True, '我在，请说。')
emit('text', text='看看今天的任务')
resume(True, '已经查到今天的任务。')
assert '正在查看今天的任务。\n已经查到今天的任务。' in (root / 'spoken').read_text()
emit('text', text='再查一下明天的任务')
resume(True, '我听到了，但当前模型限流了，请稍后再试。')
emit('text', text='结束对话')
resume(False, '好的，再见。')
(root / 'passed').write_text('ok')
"#,
        )?;
        let store = Store::open(&dir.join("mr-li.sqlite3"))?;
        let source = origin(&cfg, "ou_owner", "oc_chat");
        store.insert(&source)?;
        store.save_incoming(&source.id, &json!({}))?;
        store.status(&source.id, "done")?;
        let worker = async {
            let mut completed = 0;
            loop {
                if let Some(j) = store.jobs("queued")?.into_iter().find(is_job) {
                    assert_eq!(j.key, source.key);
                    if completed == 0 {
                        assert_eq!(j.text, "看看今天的任务");
                        store.status(&j.id, "running")?;
                        let transcript = "<voice-progress>正在查看今天的任务。</voice-progress>\n已经查到今天的任务。";
                        let update = progress_updates(transcript).next().unwrap();
                        store.set_voice_progress(&j.id, update)?;
                        while !std::fs::read_to_string(dir.join("spoken"))?.contains(update) {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        store.finish(&j.id, &final_reply(transcript))?;
                    } else {
                        assert_eq!(j.text, "再查一下明天的任务");
                        store.finish_failure(
                            &j.id,
                            "任务失败：OpenCode HTTP 429。可用 /status 查看会话。",
                        )?;
                    }
                    completed += 1;
                    if completed == 2 {
                        return Ok::<(), anyhow::Error>(());
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let (frontend, worker) = tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(run(Arc::new(cfg), false), worker)
        })
        .await?;
        worker?;
        // EOF follows all four handshakes, including resuming after a spoken failure.
        assert!(frontend.is_err());
        assert_eq!(std::fs::read_to_string(dir.join("passed"))?, "ok");
        let args = std::fs::read_to_string(dir.join("args"))?;
        assert!(args.starts_with("-v\nposture-clone\n-a\nposture-output\n"));
        assert_eq!(args.matches("configured-clone").count(), 4);
        assert_eq!(args.matches("configured-output").count(), 4);
        assert!(store.jobs("queued")?.is_empty());
        assert!(store.jobs("replying")?.is_empty());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn marked_progress_waits_for_complete_text_and_is_removed_from_reply() {
        assert_eq!(progress_updates("<voice-progress>正在查找").count(), 0);
        let text = "<voice-progress>正在查找。</voice-progress>\n<voice-progress>已发送请求。</voice-progress>\n已打开腾讯视频。";
        assert_eq!(
            progress_updates(text).collect::<Vec<_>>(),
            ["正在查找。", "已发送请求。"]
        );
        assert_eq!(final_reply(text), "已打开腾讯视频。");
    }
    fn origin(cfg: &Config, user: &str, chat: &str) -> Job {
        Job {
            id: uuid::Uuid::new_v4().to_string(),
            key: json!([cfg.workspace, user, chat, ""]).to_string(),
            message_id: "om_real".into(),
            text: "hello".into(),
            result: String::new(),
            sent: 0,
        }
    }
    #[test]
    fn only_failed_jobs_use_short_spoken_errors_and_keep_full_diagnostics() -> Result<()> {
        let cfg = Config::fixture();
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let source = origin(&cfg, "ou_owner", "oc_chat");
        let j = enqueue(&store, &source, "查一下时间")?;
        let detail = "任务失败：OpenCode HTTP 429；未切换付费模型。";
        store.finish_failure(&j.id, detail)?;
        let value = store.command_task(&j.key, &j.id)?.unwrap();
        assert_eq!(value["result"], detail);
        assert_eq!(value["kind"], "voice_error");
        assert_eq!(spoken_result(&value)?, RATE_LIMIT_REPLY);
        assert!(store.jobs("replying")?.is_empty());
        store.finish_failure(&j.id, "任务失败：网络连接中断")?;
        assert_eq!(
            spoken_result(&store.command_task(&j.key, &j.id)?.unwrap())?,
            FAILURE_REPLY
        );
        store.finish(&j.id, "HTTP 429表示请求过于频繁")?;
        assert_eq!(
            spoken_result(&store.command_task(&j.key, &j.id)?.unwrap())?,
            "HTTP 429表示请求过于频繁"
        );
        store.insert(&source)?;
        store.finish_failure(&source.id, detail)?;
        assert_eq!(store.output_kind(&source.id)?, "text");
        assert_eq!(store.jobs("replying")?.len(), 1);
        Ok(())
    }
    #[test]
    fn binding_requires_an_unambiguous_allowed_scope() -> Result<()> {
        let mut cfg = Config::fixture();
        let a = origin(&cfg, "ou_owner", "chat_a");
        let b = origin(&cfg, "ou_owner", "chat_b");
        assert!(bind(&cfg, vec![a.clone(), b.clone()]).is_err());
        assert!(bind(&cfg, vec![origin(&cfg, "stranger", "chat_a")]).is_err());
        cfg.voice.chat = "chat_a".into();
        assert_eq!(bind(&cfg, vec![b, a.clone()])?.key, a.key);
        Ok(())
    }
    #[test]
    fn transcripts_use_current_agent_and_session_and_never_enter_feishu_outbox() -> Result<()> {
        let cfg = Config::fixture();
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let origin = origin(&cfg, "ou_owner", "chat");
        let scope = models::scope(&origin.key)?;
        let job = enqueue(&store, &origin, "  查一下天气  ")?;
        assert_eq!(job.key, origin.key);
        assert_eq!(job.text, "查一下天气");
        assert_eq!(store.agent(&scope)?, "opencode");
        // Change after enqueue, before execution: no Agent choice was captured by ASR.
        store.switch_agent_and_finish(&job.id, &scope, "kilo", &json!({}))?;
        assert_eq!(store.agent(&models::scope(&job.key)?)?, "kilo");
        store.set_agent_session(&origin.key, "kilo", "session_selected")?;
        assert_eq!(
            store.agent_session(&job.key, "kilo")?.as_deref(),
            Some("session_selected")
        );
        store.finish(&job.id, "明天晴天")?;
        assert!(store.jobs("replying")?.is_empty());
        assert_eq!(
            spoken_result(&store.command_task(&job.key, &job.id)?.unwrap())?,
            "明天晴天"
        );
        assert_eq!(
            store.command_task(&job.key, &job.id)?.unwrap()["status"],
            "done"
        );
        store.finish_output(
            &job.id,
            "rich_reply",
            &json!({"fallback":"简短回复","packets":[]}),
        )?;
        assert!(store.jobs("replying")?.is_empty());
        assert_eq!(
            spoken_result(&store.command_task(&job.key, &job.id)?.unwrap())?,
            "简短回复"
        );
        assert!(enqueue(&store, &origin, "/stop").is_err());
        assert!(store.voice_origins()?.is_empty());
        store.insert(&origin)?;
        store.save_incoming(&origin.id, &json!({}))?;
        assert_eq!(store.voice_origins()?.len(), 1);
        Ok(())
    }
}
