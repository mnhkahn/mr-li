use crate::config::Config;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HomeworkState {
    #[default]
    Unavailable,
    Idle,
    Confirming,
    ActiveSince(u64),
}

impl HomeworkState {
    pub fn started(self) -> Option<u64> {
        match self {
            Self::ActiveSince(start) => Some(start),
            _ => None,
        }
    }
}

#[derive(serde::Deserialize)]
struct HomeworkTimer {
    timer_running: bool,
    current_task_id: String,
    elapsed_seconds: Option<u64>,
}

impl HomeworkTimer {
    fn advances_from(&self, previous: &Self) -> bool {
        self.timer_running
            && previous.timer_running
            && self.current_task_id == previous.current_task_id
            && self
                .elapsed_seconds
                .zip(previous.elapsed_seconds)
                .is_some_and(|(current, before)| current > before)
    }
}

async fn homework_timer(cfg: &Config) -> Result<HomeworkTimer> {
    let mut command = tokio::process::Command::new(&cfg.homework_cli);
    command
        .args(["--no-analytics", "device", "homework-status"])
        .env("CYEAM_CLI_NO_UPDATE_NOTIFIER", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(18), command.output()).await??;
    ensure!(output.status.success(), "作业平板状态查询失败");
    #[derive(serde::Deserialize)]
    struct Response {
        ok: bool,
        data: HomeworkTimer,
    }
    let response: Response = serde_json::from_slice(&output.stdout)?;
    ensure!(response.ok, "作业平板状态查询失败");
    let timer = response.data;
    ensure!(
        !timer.timer_running
            || (!timer.current_task_id.is_empty() && timer.elapsed_seconds.is_some()),
        "作业平板未返回完整计时状态"
    );
    Ok(timer)
}

// The tablet persists its last snapshot. A ticking elapsed counter confirms
// that a reported running task is current even after a reconnect or app exit.
fn observe_timer(
    state: HomeworkState,
    previous: Option<&HomeworkTimer>,
    timer: Option<&HomeworkTimer>,
    now: u64,
) -> (HomeworkState, Duration) {
    let normal = Duration::from_secs(30);
    let Some(timer) = timer else {
        return (HomeworkState::Unavailable, normal);
    };
    if !timer.timer_running {
        return (HomeworkState::Idle, normal);
    }
    if previous.is_some_and(|p| timer.advances_from(p)) {
        return (
            HomeworkState::ActiveSince(state.started().unwrap_or(now)),
            normal,
        );
    }
    let delay = if previous.is_some_and(|p| {
        p.timer_running
            && p.current_task_id == timer.current_task_id
            && p.elapsed_seconds == timer.elapsed_seconds
    }) {
        normal
    } else {
        Duration::from_secs(3)
    };
    (HomeworkState::Confirming, delay)
}

pub async fn follow_homework(cfg: &Config, store: &crate::store::Store) -> Result<()> {
    let mut previous: Option<HomeworkTimer> = None;
    let mut state = HomeworkState::Unavailable;
    let mut last_reported = None;
    loop {
        let timer = if store.has_posture_watch()? {
            homework_timer(cfg).await.ok()
        } else {
            None
        };
        let now = now_epoch();
        let (next, delay) = observe_timer(state, previous.as_ref(), timer.as_ref(), now);
        store.set_homework_state(next, now)?;
        if last_reported != Some(next) {
            tracing::info!(homework = ?next, "坐姿联动作业状态");
            last_reported = Some(next);
        }
        state = next;
        previous = timer;
        tokio::time::sleep(delay).await;
    }
}

fn archive_path(cfg: &Config, job_id: &str, epoch: u64) -> Result<PathBuf> {
    let at = chrono::DateTime::<chrono::Utc>::from_timestamp(i64::try_from(epoch)?, 0)
        .context("照片拍摄时间超出范围")?
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
    Ok(cfg
        .photo_archive_dir
        .join(at.format("%Y-%m-%d").to_string())
        .join(format!(
            "{}-{}.jpg",
            at.format("%H-%M-%S"),
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, job_id.as_bytes())
        )))
}
/// Permanent originals, grouped by capture date in Asia/Shanghai; separate from report caches.
pub fn archive_photo(cfg: &Config, job_id: &str, image: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let epoch = std::fs::metadata(image)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let path = archive_path(cfg, job_id, epoch)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path.parent().context("照片目录缺失")?)?;
    if path.is_file() {
        return Ok(path);
    }
    let temp = path.with_extension("jpg.part");
    std::fs::copy(image, &temp)?;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(temp, &path)?;
    Ok(path)
}

// Keep only the latest annotated PNG per conversation. Upload only when a report is due.
fn frame_dir(cfg: &Config, key: &str) -> Result<PathBuf> {
    let scope = crate::models::scope(key)?;
    Ok(cfg
        .data_dir
        .join("posture-images")
        .join(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, scope.as_bytes()).to_string()))
}
pub fn cache_frame(cfg: &Config, key: &str, image: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = frame_dir(cfg, key)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let tmp = dir.join("latest.next");
    std::fs::copy(image, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(tmp, dir.join("latest.png"))?;
    Ok(())
}
pub fn report_frame_path(cfg: &Config, key: &str, job_id: &str) -> Result<PathBuf> {
    Ok(frame_dir(cfg, key)?.join(format!(
        "report-{}.png",
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, job_id.as_bytes())
    )))
}
pub fn freeze_report_frame(
    cfg: &Config,
    key: &str,
    job_id: &str,
) -> Result<Option<(PathBuf, u64)>> {
    let latest = frame_dir(cfg, key)?.join("latest.png");
    let meta = match std::fs::metadata(&latest) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let captured = meta.modified()?;
    if captured.elapsed()?.as_secs() > 600 {
        return Ok(None);
    }
    let path = report_frame_path(cfg, key, job_id)?;
    std::fs::copy(latest, &path)?;
    Ok(Some((
        path,
        captured.duration_since(std::time::UNIX_EPOCH)?.as_secs(),
    )))
}
pub fn clear_frames(cfg: &Config, key: &str) -> Result<()> {
    match std::fs::remove_dir_all(frame_dir(cfg, key)?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub struct Observation {
    pub image: PathBuf,
    pub note: String,
    pub assessment: Assessment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i64)]
pub enum Assessment {
    UnspecifiedReminder = 0,
    NoObviousIssue = 1,
    Unknown = 2,
    Away = 3,
    Observe = 4,
    HeadLow = 5,
    HeadTilted = 6,
    HeadLowAndTilted = 7,
}
impl Assessment {
    pub fn from_report(value: &Value) -> Self {
        match value["assessment"].as_str() {
            Some("needs_reminder") => match value["reminder_reason"].as_str() {
                Some("head_low") => Self::HeadLow,
                Some("head_tilted") => Self::HeadTilted,
                Some("head_low_and_tilted") => Self::HeadLowAndTilted,
                // Older workers reported the cause only in the note.
                _ => match value["note"].as_str().unwrap_or_default() {
                    note if note.starts_with("疑似低头过多") => Self::HeadLow,
                    note if note.starts_with("疑似歪头，") => Self::HeadTilted,
                    _ => Self::UnspecifiedReminder,
                },
            },
            Some("no_obvious_issue") => Self::NoObviousIssue,
            Some("away") => Self::Away,
            Some("observe") => Self::Observe,
            _ => Self::Unknown,
        }
    }
    pub fn speech(self) -> Option<&'static str> {
        match self {
            Self::HeadLow => Some("坐姿提醒，你低头了，请抬头。"),
            Self::HeadTilted => Some("坐姿提醒，你的头歪了，请把头摆正。"),
            Self::HeadLowAndTilted => {
                Some("坐姿提醒，你低头了，请抬头。你的头也歪了，请把头摆正。")
            }
            _ => None,
        }
    }
}

/// System speech runs on the Mac mini; cancellation also stops the speech child.
pub async fn speak(cfg: &Config, assessment: Assessment, scope: &str) -> Result<()> {
    if !cfg.posture_speech {
        return Ok(());
    }
    let Some(text) = assessment.speech() else {
        return Ok(());
    };
    crate::speech::speak_posture(cfg, text, scope).await
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
    if let Some(detector) = &cfg.pose_detector {
        ensure!(
            detector.is_file(),
            "尚未安装人体检测模型，请运行 scripts/setup-pose.sh"
        );
        cmd.arg("--detector").arg(detector);
    }
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
        assessment: Assessment::from_report(&report),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn homework_timer_requires_progress_and_stops_when_idle_or_stale() {
        let snapshot = |elapsed, timer_running| HomeworkTimer {
            timer_running,
            current_task_id: "task".into(),
            elapsed_seconds: Some(elapsed),
        };
        let first = snapshot(12, true);
        let running = snapshot(15, true);
        let idle = snapshot(15, false);
        assert_eq!(
            observe_timer(HomeworkState::Unavailable, None, Some(&first), 100),
            (HomeworkState::Confirming, Duration::from_secs(3))
        );
        let active = HomeworkState::ActiveSince(103);
        assert_eq!(
            observe_timer(HomeworkState::Confirming, Some(&first), Some(&running), 103).0,
            active
        );
        assert_eq!(
            observe_timer(active, Some(&running), Some(&idle), 110).0,
            HomeworkState::Idle
        );
        assert_eq!(
            observe_timer(active, Some(&running), Some(&running), 110).0,
            HomeworkState::Confirming
        );
        assert_eq!(
            observe_timer(active, Some(&running), None, 110).0,
            HomeworkState::Unavailable
        );
    }

    #[test]
    fn archives_use_beijing_capture_dates_without_overwrite_or_stop_cleanup() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};
        let root = std::env::temp_dir().join(format!("mr-li-archive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root)?;
        let mut cfg = Config::fixture();
        cfg.data_dir = root.join("cache");
        cfg.photo_archive_dir = root.join("photos");
        let source = root.join("original.jpg");
        std::fs::write(&source, b"original-photo")?;
        let epoch =
            chrono::DateTime::parse_from_rfc3339("2026-11-15T15:59:59Z")?.timestamp() as u64;
        let set_time = |time: SystemTime| {
            std::fs::File::options()
                .write(true)
                .open(&source)?
                .set_modified(time)
        };
        set_time(UNIX_EPOCH + Duration::from_secs(epoch))?;
        let a = archive_photo(&cfg, "job1", &source)?;
        let b = archive_photo(&cfg, "job2", &source)?;
        assert_ne!(a, b);
        assert_eq!(
            a.parent().unwrap(),
            cfg.photo_archive_dir.join("2026-11-15")
        );
        assert!(
            a.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("23-59-59-")
        );
        assert_eq!(std::fs::read(&a)?, b"original-photo");
        assert_eq!(std::fs::metadata(&a)?.permissions().mode() & 0o077, 0);
        let key = serde_json::json!(["workspace", "owner", "chat", "thread"]).to_string();
        cache_frame(&cfg, &key, &source)?;
        clear_frames(&cfg, &key)?;
        assert!(a.is_file() && b.is_file());
        set_time(UNIX_EPOCH + Duration::from_secs(epoch + 1))?;
        let next = archive_photo(&cfg, "job3", &source)?;
        assert_eq!(
            next.parent().unwrap(),
            cfg.photo_archive_dir.join("2026-11-16")
        );
        assert!(
            next.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("00-00-00-")
        );
        assert_eq!(archive_photo(&cfg, "job3", &source)?, next);
        assert_eq!(std::fs::read_dir(next.parent().unwrap())?.count(), 1);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn report_freezes_one_frame_and_keeps_scopes_separate() -> Result<()> {
        let root = std::env::temp_dir().join(format!("mr-li-frames-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root)?;
        let mut cfg = Config::fixture();
        cfg.data_dir = root.clone();
        let key = serde_json::json!(["workspace", "owner", "chat", "thread"]).to_string();
        let other = serde_json::json!(["workspace", "another", "chat", "thread"]).to_string();
        let source = root.join("source.jpg");
        std::fs::write(&source, b"original-frame")?;
        cache_frame(&cfg, &key, &source)?;
        let (frozen, _) = freeze_report_frame(&cfg, &key, "report1")?.unwrap();
        std::fs::write(&source, b"next-minute-frame")?;
        cache_frame(&cfg, &key, &source)?;
        assert_eq!(std::fs::read(&frozen)?, b"original-frame");
        let (next, _) = freeze_report_frame(&cfg, &key, "report2")?.unwrap();
        assert_eq!(std::fs::read(next)?, b"next-minute-frame");
        assert!(freeze_report_frame(&cfg, &other, "report1")?.is_none());
        let latest = frame_dir(&cfg, &key)?.join("latest.png");
        std::fs::File::options()
            .write(true)
            .open(latest)?
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(601))?;
        assert!(freeze_report_frame(&cfg, &key, "report3")?.is_none());
        clear_frames(&cfg, &key)?;
        assert!(!frozen.exists());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn reminder_uses_the_reported_cause() {
        for (reason, expected) in [
            ("head_low", "坐姿提醒，你低头了，请抬头。"),
            ("head_tilted", "坐姿提醒，你的头歪了，请把头摆正。"),
            (
                "head_low_and_tilted",
                "坐姿提醒，你低头了，请抬头。你的头也歪了，请把头摆正。",
            ),
        ] {
            let report = serde_json::json!({
                "assessment": "needs_reminder",
                "reminder_reason": reason,
            });
            assert_eq!(Assessment::from_report(&report).speech(), Some(expected));
        }
        let legacy =
            serde_json::json!({"assessment": "needs_reminder", "note": "疑似歪头，请把头摆正"});
        assert_eq!(Assessment::from_report(&legacy), Assessment::HeadTilted);
        let unspecified = serde_json::json!({
            "assessment": "needs_reminder",
            "note": "疑似低头或歪头",
        });
        assert_eq!(Assessment::from_report(&unspecified).speech(), None);
    }

    #[test]
    fn missing_or_new_assessment_never_speaks_as_normal() {
        for value in [
            Value::Null,
            Value::String("future_model_value".into()),
            Value::Bool(true),
        ] {
            assert_eq!(
                Assessment::from_report(&serde_json::json!({"assessment": value})),
                Assessment::Unknown
            );
        }
        assert_eq!(Assessment::Unknown.speech(), None);
        assert_eq!(Assessment::Away.speech(), None);
        assert_eq!(Assessment::Observe.speech(), None);
        assert_eq!(Assessment::NoObviousIssue.speech(), None);
    }
}
