//! Manual text-to-speech on the deployment Mac. Text is stdin, never shell code or options.
use crate::config::Config;
use anyhow::{Context, Result, ensure};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    play(
        Path::new(&cfg.speech_program),
        cfg,
        text,
        Duration::from_secs(180),
    )
    .await
}
pub async fn speak_posture(cfg: &Config, text: &str) -> Result<()> {
    let mut cfg = cfg.clone();
    cfg.speak_voice = cfg.posture_voice.clone();
    cfg.speak_audio_device = cfg.posture_audio_device.clone();
    play(
        Path::new(&cfg.speech_program),
        &cfg,
        text,
        Duration::from_secs(20),
    )
    .await
}
async fn play(program: &Path, cfg: &Config, text: &str, timeout: Duration) -> Result<()> {
    ensure!(!text.trim().is_empty(), "用法：/speak 要播报的文字");
    ensure!(
        text.chars().count() <= 500 && !text.contains('\0'),
        "播报文字最多 500 字，不能包含空字符"
    );
    let mut child = tokio::process::Command::new(program)
        .args([
            "-v",
            &cfg.speak_voice,
            "-a",
            &cfg.speak_audio_device,
            "-r",
            "180",
        ])
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
        play(&program, &cfg, &text, Duration::from_secs(10)).await?;
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
            play(&path, &Config::fixture(), "测试", Duration::from_secs(60)).await
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
        let result = play(&path, &cfg, "测试", Duration::from_millis(50)).await;
        assert!(result.unwrap_err().to_string().contains("已停止"));
        assert!(started.elapsed() < Duration::from_secs(2));
        std::fs::remove_file(path)?;
        Ok(())
    }
}
