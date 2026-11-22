//! Manual text-to-speech on the deployment Mac. Text is stdin, never shell code or options.
use crate::config::Config;
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::{ErrorKind, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SYSTEM_PROGRAM: &str = "/usr/bin/say";
const CLONE_SCRIPT: &str = "local-voice.sh";

#[derive(Clone, Copy)]
enum Voice {
    TingTing,
    Clone,
}

impl Voice {
    fn key(self) -> &'static str {
        match self {
            Self::TingTing => "tingting",
            Self::Clone => "clone",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::TingTing => "系统 Ting-Ting",
            Self::Clone => "本地克隆音色",
        }
    }

    fn program(self, cfg: &Config) -> &Path {
        match self {
            Self::TingTing => Path::new(SYSTEM_PROGRAM),
            Self::Clone if Path::new(&cfg.speech_program).ends_with(CLONE_SCRIPT) => {
                Path::new(&cfg.speech_program)
            }
            Self::Clone => Path::new("scripts/local-voice.sh"),
        }
    }
}

fn selection_path(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("speech-voice")
}

fn selection(cfg: &Config) -> Result<Option<Voice>> {
    match fs::read_to_string(selection_path(cfg)) {
        Ok(value) => match value.trim() {
            "tingting" => Ok(Some(Voice::TingTing)),
            "clone" => Ok(Some(Voice::Clone)),
            _ => anyhow::bail!("语音设置无效，可用 /voice tingting 或 /voice clone 重新选择"),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("无法读取语音设置"),
    }
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn select(cfg: &Config, voice: Voice) -> Result<()> {
    let program = voice.program(cfg);
    match voice {
        Voice::TingTing => ensure!(executable(program), "系统语音程序不可用"),
        Voice::Clone => {
            let root = program
                .parent()
                .and_then(Path::parent)
                .context("克隆脚本路径无效")?;
            let assets = root.join(".data/local-tts");
            ensure!(
                executable(program)
                    && executable(&root.join(".data/voice/venv/bin/python"))
                    && executable(&assets.join("bin/mr-li-local-voice"))
                    && [
                        "config.json",
                        "reference.wav",
                        "reference.txt",
                        "models/encoder.int8.onnx",
                        "models/decoder.int8.onnx",
                        "vocos_24khz.onnx",
                    ]
                    .iter()
                    .all(|file| assets.join(file).is_file()),
                "本地克隆音色需先安装脚本、运行环境和模型"
            );
        }
    }

    fs::create_dir_all(&cfg.data_dir).context("无法创建语音设置目录")?;
    let temp = cfg
        .data_dir
        .join(format!(".speech-voice-{}", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        writeln!(file, "{}", voice.key())?;
        file.sync_all()?;
        fs::rename(&temp, selection_path(cfg))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result.context("无法保存语音设置")
}

pub fn voice_command(cfg: &Config, args: &str) -> Result<String> {
    const USAGE: &str = "/voice tingting 切换到系统 Ting-Ting\n/voice clone 切换到本地克隆音色";
    let voice = match args {
        "" | "status" => {
            let current = match selection(cfg)? {
                Some(voice) => voice.label().to_owned(),
                None if cfg.speech_program == SYSTEM_PROGRAM => {
                    format!("系统语音（{}）", cfg.speak_voice)
                }
                None if Path::new(&cfg.speech_program).ends_with(CLONE_SCRIPT) => {
                    Voice::Clone.label().to_owned()
                }
                None => "部署配置中的语音".to_owned(),
            };
            return Ok(format!("Mac mini 当前语音：{current}\n{USAGE}"));
        }
        "tingting" => Voice::TingTing,
        "clone" => Voice::Clone,
        _ => anyhow::bail!("用法：/voice [status|tingting|clone]"),
    };
    select(cfg, voice)?;
    Ok(format!(
        "Mac mini 已切换到{}，从下一条播报起生效。",
        voice.label()
    ))
}

// Keep the tail (where workers print their final error) while draining the pipe so
// verbose dependencies cannot block the child or consume unbounded memory.
async fn diagnostics(mut input: tokio::process::ChildStderr) -> Result<Vec<u8>> {
    let mut tail = Vec::new();
    let mut buffer = [0u8; 2048];
    loop {
        let n = input.read(&mut buffer).await?;
        if n == 0 {
            return Ok(tail);
        }
        tail.extend_from_slice(&buffer[..n]);
        if tail.len() > 8192 {
            tail.drain(..tail.len() - 8192);
        }
    }
}
fn playback_error(stderr: &[u8]) -> &'static str {
    let detail = String::from_utf8_lossy(stderr).to_lowercase();
    if detail.contains("zerogpu") && detail.contains("quota") {
        "音色合成服务的免费额度已耗尽，暂时无法合成新文字；已有缓存仍可播放。请等待额度恢复，或配置 Hugging Face 登录凭证。"
    } else if detail.contains("no module named") {
        "语音程序缺少 Python 依赖，请检查它使用的独立运行环境"
    } else if detail.contains("tts api http 402") {
        "语音合成服务账户余额不足（HTTP 402），新文字尚未生成"
    } else if detail.contains("tts api http 401") {
        "语音合成服务凭证无效（HTTP 401），请检查API密钥"
    } else if detail.contains("tts api http 429") {
        "语音合成服务限流（HTTP 429），请稍后再试"
    } else if detail.contains("tts api") {
        "语音合成服务调用失败，请检查模型、音色、API额度与网络；喇叭播放尚未开始"
    } else {
        "语音播放失败，请检查 Mac mini 的语音程序和喇叭输出设备"
    }
}

pub fn is_command(text: &str) -> bool {
    text.split_whitespace().next() == Some("/speak")
}
pub fn is_stop(text: &str) -> bool {
    let mut words = text.split_whitespace();
    words.next() == Some("/speak") && words.next() == Some("stop") && words.next().is_none()
}
pub fn text(text: &str) -> Option<&str> {
    let text = text.trim();
    let (command, args) = text.split_once(char::is_whitespace)?;
    let args = args.trim();
    (command == "/speak" && !args.is_empty() && args != "stop").then_some(args)
}
pub async fn speak(cfg: &Config, text: &str) -> Result<()> {
    speak_shared(cfg, text, Duration::from_secs(180)).await
}
pub async fn speak_news(cfg: &Config, text: &str) -> Result<()> {
    speak_shared(cfg, text, Duration::from_secs(360)).await
}
pub async fn speak_progress(cfg: &Config, text: &str) -> Result<()> {
    speak_shared(cfg, text, Duration::from_secs(20)).await
}
pub async fn speak_posture(cfg: &Config, text: &str) -> Result<()> {
    let mut cfg = cfg.clone();
    cfg.speak_voice = cfg.posture_voice.clone();
    cfg.speak_audio_device = cfg.posture_audio_device.clone();
    speak_shared(&cfg, text, Duration::from_secs(20)).await
}

async fn audio_lock(cfg: &Config, exclusive: bool) -> Result<fs::File> {
    fs::create_dir_all(&cfg.data_dir)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(cfg.data_dir.join("voice-answer.lock"))?;
    loop {
        let result = if exclusive {
            fs2::FileExt::try_lock_exclusive(&lock)
        } else {
            fs2::FileExt::try_lock_shared(&lock)
        };
        match result {
            Ok(()) => return Ok(lock),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error).context("无法锁定语音设备"),
        }
    }
}

async fn speak_shared(cfg: &Config, text: &str, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let _lock = tokio::time::timeout_at(deadline, audio_lock(cfg, false))
        .await
        .context("喇叭正在等待语音回答")??;
    speak_with_selection(
        cfg,
        text,
        deadline.saturating_duration_since(tokio::time::Instant::now()),
    )
    .await
}

pub async fn ask_aloud(cfg: &Config, question: &str) -> Result<fs::File> {
    let lock = tokio::time::timeout(Duration::from_secs(20), audio_lock(cfg, true))
        .await
        .context("喇叭忙，无法播报问题")??;
    speak_with_selection(cfg, &format!("请回答。{question}"), Duration::from_secs(95)).await?;
    // Keep ordinary broadcasts quiet until the caller finishes recording.
    Ok(lock)
}

async fn speak_with_selection(cfg: &Config, text: &str, timeout: Duration) -> Result<()> {
    let (program, voice) = match selection(cfg)? {
        Some(Voice::TingTing) => (Voice::TingTing.program(cfg), "Ting-Ting"),
        Some(voice) => (voice.program(cfg), cfg.speak_voice.as_str()),
        None => (Path::new(&cfg.speech_program), cfg.speak_voice.as_str()),
    };
    play(program, voice, cfg, text, timeout).await
}
async fn play(
    program: &Path,
    voice: &str,
    cfg: &Config,
    text: &str,
    timeout: Duration,
) -> Result<()> {
    ensure!(!text.trim().is_empty(), "用法：/speak 要播报的文字");
    ensure!(
        text.chars().count() <= 500 && !text.contains('\0'),
        "播报文字最多 500 字，不能包含空字符"
    );
    let mut child = tokio::process::Command::new(program)
        .args(["-v", voice, "-a", &cfg.speak_audio_device, "-r", "180"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("无法启动 Mac mini 本地语音合成")?;
    let mut input = child.stdin.take().context("无法打开语音输入")?;
    input
        .write_all(text.as_bytes())
        .await
        .context("写入播报文字失败")?;
    drop(input);
    let stderr = child.stderr.take().context("无法读取语音程序诊断")?;
    let (status, stderr) = match tokio::time::timeout(timeout, async {
        tokio::try_join!(
            async { child.wait().await.map_err(anyhow::Error::from) },
            diagnostics(stderr)
        )
    })
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!("播报超过 {} 秒，已停止", timeout.as_secs());
        }
    };
    if !status.success() {
        let reason = playback_error(&stderr);
        tracing::warn!(exit_code=?status.code(),reason,"语音程序执行失败");
        anyhow::bail!("{reason}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[tokio::test]
    async fn quota_failure_is_reported_without_blame_on_audio_device_and_stderr_is_drained()
    -> Result<()> {
        let dir = std::env::temp_dir().join(format!("mr-li-speech-quota-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let program = dir.join("worker");
        std::fs::write(
            &program,
            "#!/bin/sh\ncat >/dev/null\n/usr/bin/head -c 131072 /dev/zero >&2\nprintf 'You have exceeded your ZeroGPU quota (90s requested vs. 0s left)' >&2\nexit 1\n",
        )?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        let error = play(
            &program,
            "Ting-Ting",
            &Config::fixture(),
            "测试",
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("免费额度已耗尽"));
        assert!(!error.contains("喇叭"));
        assert!(playback_error(b"No module named 'gradio_client'").contains("Python 依赖"));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn parses_multiline_text_without_treating_stop_as_an_utterance() {
        assert_eq!(text("/speak 你好\n请坐直"), Some("你好\n请坐直"));
        assert_eq!(text("/speak stop"), None);
        assert!(is_stop("/speak   stop \n"));
        assert!(!is_stop("/speak stop talking"));
        assert_eq!(text("/speak"), None);
        assert_eq!(text("/speaker hello"), None);
        assert_eq!(text("/speak stop talking"), Some("stop talking"));
    }
    #[tokio::test]
    async fn text_is_literal_stdin_and_not_options_or_shell() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("mr-li-speech-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let program = dir.join("say");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\ncat > '{}'\nprintf '%s\\n' \"$@\" > '{}'\n",
                dir.join("text").display(),
                dir.join("args").display()
            ),
        )?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        let cfg = Config::fixture();
        let text = format!(
            "-v ? ; $(touch '{}')\n你好，李老师。",
            dir.join("SHOULD_NOT_EXIST").display()
        );
        play(
            &program,
            &cfg.speak_voice,
            &cfg,
            &text,
            Duration::from_secs(10),
        )
        .await?;
        assert_eq!(std::fs::read_to_string(dir.join("text"))?, text);
        assert_eq!(
            std::fs::read_to_string(dir.join("args"))?,
            format!(
                "-v\n{}\n-a\n{}\n-r\n180\n",
                cfg.speak_voice, cfg.speak_audio_device
            )
        );
        assert!(!dir.join("SHOULD_NOT_EXIST").exists());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn manual_and_posture_share_configured_backend_and_stdin() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("mr-li-voice-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let program = dir.join("voice");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\ncat > '{}'\nprintf '%s\\n' \"$@\" > '{}'\n",
                dir.join("text").display(),
                dir.join("args").display()
            ),
        )?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        let mut cfg = Config::fixture();
        cfg.data_dir = dir.join("data");
        cfg.speech_program = program.to_string_lossy().into_owned();
        speak(&cfg, "新闻内容").await?;
        assert_eq!(std::fs::read_to_string(dir.join("text"))?, "新闻内容");
        cfg.posture_voice = "posture-voice".into();
        cfg.posture_audio_device = "posture-output".into();
        speak_posture(&cfg, "坐直一点").await?;
        assert_eq!(std::fs::read_to_string(dir.join("text"))?, "坐直一点");
        assert!(std::fs::read_to_string(dir.join("args"))?.contains("posture-output"));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn voice_selection_persists_and_applies_to_later_playback() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("mr-li-voice-mode-{}", uuid::Uuid::new_v4()));
        let mut cfg = Config::fixture();
        cfg.data_dir = dir.join("data");
        let program = dir.join("scripts/local-voice.sh");
        cfg.speech_program = program.display().to_string();
        assert!(voice_command(&cfg, "status")?.contains("本地克隆音色"));
        assert!(voice_command(&cfg, "clone").is_err());
        assert!(selection(&cfg)?.is_none());
        assert!(voice_command(&cfg, "clone /bin/sh").is_err());

        for relative in [
            "scripts/local-voice.sh",
            ".data/voice/venv/bin/python",
            ".data/local-tts/bin/mr-li-local-voice",
            ".data/local-tts/config.json",
            ".data/local-tts/reference.wav",
            ".data/local-tts/reference.txt",
            ".data/local-tts/models/encoder.int8.onnx",
            ".data/local-tts/models/decoder.int8.onnx",
            ".data/local-tts/vocos_24khz.onnx",
        ] {
            let path = dir.join(relative);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, "#!/bin/sh\ncat >/dev/null\n")?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        }
        fs::write(
            &program,
            format!("#!/bin/sh\ncat > '{}'\n", dir.join("spoken").display()),
        )?;
        assert!(voice_command(&cfg, "clone")?.contains("已切换到本地克隆音色"));
        let reloaded = cfg.clone();
        assert!(matches!(selection(&reloaded)?, Some(Voice::Clone)));
        speak_progress(&reloaded, "继续处理").await?;
        assert_eq!(fs::read_to_string(dir.join("spoken"))?, "继续处理");

        if executable(Path::new(SYSTEM_PROGRAM)) {
            assert!(voice_command(&cfg, "tingting")?.contains("已切换到系统 Ting-Ting"));
            assert!(voice_command(&reloaded, "")?.contains("当前语音：系统 Ting-Ting"));
            assert!(program.is_file());
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn cancellation_kills_the_speech_child() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("mr-li-speech-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("say");
        let pid_file = dir.join("pid");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 30\n",
                pid_file.display()
            ),
        )?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        let task = tokio::spawn(async move {
            play(
                &path,
                "Ting-Ting",
                &Config::fixture(),
                "测试",
                Duration::from_secs(60),
            )
            .await
        });
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = value.trim().parse::<u32>()
                {
                    break pid.to_string();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = tokio::process::Command::new("/bin/kill")
                    .args(["-0", &pid])
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .unwrap();
                if !status.success() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn rejects_long_text_and_stops_a_timed_out_process() -> Result<()> {
        let cfg = Config::fixture();
        assert!(
            play(
                Path::new("/nonexistent"),
                "Ting-Ting",
                &cfg,
                &"字".repeat(501),
                Duration::from_millis(20)
            )
            .await
            .is_err()
        );
        let path = std::env::temp_dir().join(format!("mr-li-speech-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, "#!/bin/sh\nexec /bin/sleep 30\n")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        let started = std::time::Instant::now();
        let result = play(&path, "Ting-Ting", &cfg, "测试", Duration::from_millis(50)).await;
        assert!(result.unwrap_err().to_string().contains("已停止"));
        assert!(started.elapsed() < Duration::from_secs(2));
        std::fs::remove_file(path)?;
        Ok(())
    }
}
