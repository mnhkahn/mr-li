//! Manual text-to-speech on the deployment Mac. Text is stdin, never shell code or options.
use crate::config::Config;
use anyhow::{Context, Result, ensure};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncWriteExt;

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
        Path::new("/usr/bin/say"),
        cfg,
        text,
        Duration::from_secs(180),
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
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("无法启动 Mac mini 本地语音合成")?;
    let mut input = child.stdin.take().context("无法打开语音输入")?;
    input
        .write_all(text.as_bytes())
        .await
        .context("写入播报文字失败")?;
    drop(input);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!("播报超过 3 分钟，已停止");
        }
    };
    ensure!(
        status.success(),
        "语音播放失败，请检查 Mac mini 的语音和喇叭输出设备"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
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
        play(&program, &cfg, &text, Duration::from_secs(2)).await?;
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
