use crate::config::Config;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

pub struct Observation {
    pub image: PathBuf,
    pub note: String,
    pub assessment: Assessment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assessment {
    NeedsReminder,
    NoObviousIssue,
    Unknown,
}
impl Assessment {
    pub fn from_report(value: &Value) -> Self {
        match value.as_str() {
            Some("needs_reminder") => Self::NeedsReminder,
            Some("no_obvious_issue") => Self::NoObviousIssue,
            _ => Self::Unknown,
        }
    }
    pub fn speech(self) -> Option<&'static str> {
        match self {
            Self::NeedsReminder => Some("坐姿提醒，头有些低或者歪了，请抬头，把头摆正，坐直一点。"),
            Self::NoObviousIssue => Some("本次坐姿检查没有发现明显低头或歪头，请继续保持。"),
            Self::Unknown => None,
        }
    }
}

/// System speech runs on the Mac mini; cancellation also stops the speech child.
pub async fn speak(cfg: &Config, assessment: Assessment) -> Result<()> {
    if !cfg.posture_speech {
        return Ok(());
    }
    let Some(text) = assessment.speech() else {
        return Ok(());
    };
    let mut child = tokio::process::Command::new("/usr/bin/say")
        .args([
            "-v",
            &cfg.posture_voice,
            "-a",
            &cfg.posture_audio_device,
            "-r",
            "180",
            text,
        ])
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("无法启动 Mac 语音播报")?;
    let status = match tokio::time::timeout(Duration::from_secs(20), child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!("Mac 语音播报超时，已停止")
        }
    };
    ensure!(status.success(), "Mac 语音播报失败，请检查语音与输出设备");
    Ok(())
}

/// Run an isolated local worker; the main bridge does not link the ML runtime.
pub async fn analyze(cfg: &Config, image: &Path) -> Result<Observation> {
    ensure!(
        cfg.pose_model.is_file(),
        "尚未安装本地姿态模型，请先运行 scripts/setup-pose.sh"
    );
    let prefix = image.with_file_name("posture-result");
    let mut cmd = tokio::process::Command::new(&cfg.pose_bin);
    cmd.arg("--model")
        .arg(&cfg.pose_model)
        .arg("--image")
        .arg(image)
        .arg("--output")
        .arg(&prefix);
    if !cfg.pose_roi.is_empty() {
        cmd.arg("--roi").arg(&cfg.pose_roi);
    }
    for key in ["FEISHU_APP_SECRET", "OPENCODE_SERVER_PASSWORD"] {
        cmd.env_remove(key);
    }
    let mut child = cmd
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("无法启动本地姿态程序，请检查 MR_LI_POSE_BIN")?;
    let status = match tokio::time::timeout(Duration::from_secs(90), child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!("本地姿态分析超时，已停止")
        }
    };
    ensure!(
        status.success(),
        "本地姿态模型执行失败，请检查模型兼容性与检测区域配置"
    );
    let report: Value =
        serde_json::from_slice(&tokio::fs::read(prefix.with_extension("json")).await?)?;
    let note = format!(
        "{}\n本地推理 {:.0} ms；加载 {:.0} ms。绿色为较高分关键点，橙色为较低分；分数不代表实际可见或一定正确。",
        report["note"].as_str().context("姿态结果缺少说明")?,
        report["inference_ms"].as_f64().unwrap_or(0.),
        report["model_load_ms"].as_f64().unwrap_or(0.)
    );
    Ok(Observation {
        image: prefix.with_extension("png"),
        note,
        assessment: Assessment::from_report(&report["assessment"]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_new_assessment_never_speaks_as_normal() {
        for value in [
            Value::Null,
            Value::String("future_model_value".into()),
            Value::Bool(true),
        ] {
            assert_eq!(Assessment::from_report(&value), Assessment::Unknown);
        }
        assert_eq!(Assessment::Unknown.speech(), None);
        assert!(Assessment::NeedsReminder.speech().is_some());
        assert!(Assessment::NoObviousIssue.speech().is_some());
    }
}
