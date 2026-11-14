use crate::{config::Config, opencode::OpenCode};
use anyhow::{Context, Result, ensure};
use std::{os::unix::process::CommandExt, process::Command, sync::Arc, time::Duration};

pub fn opencode(cfg: &Config) -> Result<()> {
    let v = Command::new(&cfg.opencode_bin)
        .arg("--version")
        .output()
        .context("找不到 OpenCode，请配置 OPENCODE_BIN")?;
    ensure!(
        v.status.success() && String::from_utf8_lossy(&v.stdout).trim() == "1.19.0",
        "需要 OpenCode 1.19.0"
    );
    let url = reqwest::Url::parse(&cfg.opencode_url)?;
    let mut cmd = Command::new(&cfg.opencode_bin);
    cmd.args([
        "serve",
        "--hostname",
        "127.0.0.1",
        "--port",
        &url.port().unwrap_or(80).to_string(),
    ])
    .current_dir(&cfg.workspace)
    .env("OPENCODE_SERVER_PASSWORD", &cfg.password)
    .env("OPENCODE_SERVER_USERNAME", &cfg.username)
    .env_remove("FEISHU_APP_SECRET")
    .env_remove("FEISHU_ALLOWED_OPEN_IDS");
    Err(cmd.exec()).context("无法启动 OpenCode")
}
fn restart(domain: &str, name: &str) -> Result<()> {
    ensure!(
        matches!(domain, "system") || domain.starts_with("gui/"),
        "无效 launchd domain"
    );
    let status = Command::new("/bin/launchctl")
        .args([
            "kickstart",
            "-k",
            &format!("{domain}/com.cyeam.mr-li.{name}"),
        ])
        .status()?;
    ensure!(status.success(), "无法重启 {name}");
    Ok(())
}
pub async fn watchdog(cfg: Arc<Config>, oc: &OpenCode, domain: &str) -> Result<()> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let failures = cfg.data_dir.join("health-failures");
    if oc.health().await.is_err() {
        let n = std::fs::read_to_string(&failures)
            .unwrap_or_default()
            .parse::<u32>()
            .unwrap_or(0)
            + 1;
        std::fs::write(&failures, n.to_string())?;
        if n >= 3 {
            tracing::warn!("OpenCode 连续三次健康检查失败，重启服务");
            restart(domain, "opencode")?;
            std::fs::write(&failures, "0")?;
        }
    } else {
        std::fs::write(&failures, "0")?;
    }
    let age = std::fs::metadata(cfg.data_dir.join("heartbeat"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .unwrap_or(Duration::from_secs(0));
    if age > Duration::from_secs(180) {
        tracing::warn!("桥接心跳超过 180 秒未更新，重启服务");
        restart(domain, "bridge")?;
    }
    Ok(())
}
