mod bridge;
mod camera;
mod config;
mod feishu;
mod models;
mod opencode;
mod service;
mod store;
use anyhow::{Context, Result};
use std::sync::Arc;
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mr_li=info".into()),
        )
        .init();
    let mode = std::env::args().nth(1).unwrap_or_else(|| "bridge".into());
    let cfg = Arc::new(config::Config::load(mode == "enroll")?);
    if mode == "enroll" {
        let chat = std::env::args()
            .nth(2)
            .context("enroll 需要群 chat_id 或 p2p")?;
        let challenge = std::env::args()
            .nth(3)
            .context("enroll 需要匹配的确认短语")?;
        anyhow::ensure!(challenge.len() >= 8, "确认短语至少 8 字节");
        let feishu = feishu::Feishu::new(cfg)?;
        feishu.identify().await?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            r=feishu.observe(&chat,&challenge)=>r?,
            _=tokio::signal::ctrl_c()=>{},
            _=terminate.recv()=>{},
            _=tokio::time::sleep(std::time::Duration::from_secs(600))=>{},
        }
        return Ok(());
    }
    let oc = Arc::new(opencode::OpenCode::new(cfg.clone())?);
    if mode == "opencode" {
        return service::opencode(&cfg);
    }
    if mode == "watchdog" {
        let domain = std::env::args()
            .nth(2)
            .context("watchdog 需要 launchd domain")?;
        return service::watchdog(cfg.clone(), &oc, &domain).await;
    }
    if mode == "probe" {
        oc.health().await?;
        let age = std::fs::metadata(cfg.data_dir.join("heartbeat"))?
            .modified()?
            .elapsed()?;
        anyhow::ensure!(age < std::time::Duration::from_secs(180), "桥接心跳过期");
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("health") {
        oc.health().await?;
        println!("OpenCode V2 healthy");
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("check") {
        println!("配置检查通过（未连接飞书或调用模型）");
        return Ok(());
    }
    std::fs::create_dir_all(&cfg.data_dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cfg.data_dir.join("bridge.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("已有一个李老师进程使用此数据目录")?;
    let store = Arc::new(store::Store::open(&cfg.data_dir.join("mr-li.sqlite3"))?);
    let feishu = Arc::new(feishu::Feishu::new(cfg.clone())?);
    oc.health()
        .await
        .context("请先启动受支持的 OpenCode 服务")?;
    store.use_v2_sessions()?;
    feishu.identify().await?;
    let bridge = Arc::new(bridge::Bridge {
        cfg,
        store: store.clone(),
        feishu: feishu.clone(),
        oc: oc.clone(),
    });
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r=feishu.run(store.clone())=>r?,
        r=bridge.run()=>r?,
        _=tokio::signal::ctrl_c()=>{},
        _=terminate.recv()=>{},
    }
    tracing::info!("正在停止李老师");
    for j in store.jobs("running")? {
        if camera::is_capture(&j.text) {
            continue;
        }
        if let Some(sid) = store.session(&j.key)?
            && let Err(e) = oc.abort(&sid).await
        {
            tracing::warn!(error=%e,"退出时中止任务失败，重启后将再次尝试");
        }
    }
    Ok(())
}
