mod agent;
mod bridge;
mod builtin;
mod camera;
mod commands;
mod config;
mod feishu;
mod files;
mod forms;
mod home_assistant;
mod models;
mod music;
mod notices;
mod opencode;
mod posture;
mod proxy;
mod reminders;
mod reply;
mod service;
mod speech;
mod store;
mod tablet;
mod voice;
use anyhow::{Context, Result};
use std::sync::Arc;
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mr_li=info".into()),
        )
        .init();
    let mode = std::env::args().nth(1).unwrap_or_else(|| "bridge".into());
    if mode == "proxy" {
        let args = std::env::args().skip(2).collect::<Vec<_>>().join(" ");
        println!("{}", proxy::command(&args).await?);
        return Ok(());
    }
    if mode == "music-playlist-worker" {
        let spec = std::env::args().nth(2).context("缺少本地歌单播放文件")?;
        return music::playlist_worker(std::path::Path::new(&spec));
    }
    if mode == "music" {
        let args = std::env::args().skip(2).collect::<Vec<_>>().join(" ");
        let storefront = music::local_setting(music::STOREFRONT_KEY, music::DEFAULT_STOREFRONT);
        let media_dir = music::local_setting(music::MEDIA_DIR_KEY, "");
        let data_dir = music::local_setting("MR_LI_DATA_DIR", ".data");
        println!("{}", music::command(&args, &storefront, &media_dir, std::path::Path::new(&data_dir)).await?);
        return Ok(());
    }
    if mode == "reply-check" {
        return reply::check_stdio().await;
    }
    if mode == "builtin-mcp" {
        return builtin::stdio().await;
    }
    if mode == "ha-mcp" {
        return home_assistant::stdio().await;
    }
    if mode == "ha" {
        let settings = home_assistant::Settings::load()?;
        let args = std::env::args().skip(2).collect::<Vec<_>>().join(" ");
        println!(
            "{}",
            serde_json::to_string_pretty(
                &home_assistant::command(settings.as_ref(), &args).await?
            )?
        );
        return Ok(());
    }
    if mode == "agent-worker" {
        return agent::worker().await;
    }
    if mode == "tablet-mcp" {
        return tablet::stdio().await;
    }
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
    if mode.starts_with("tablet-") {
        return tablet::command(&cfg, &mode).await;
    }
    if mode == "speak" {
        return speech::speak(
            &cfg,
            &std::env::args().skip(2).collect::<Vec<_>>().join(" "),
        )
        .await;
    }
    if mode == "voice" || mode == "voice-check" {
        return voice::run(cfg, mode == "voice-check").await;
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
        // OpenCode has its own watchdog. Its outage must not restart other Agents.
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
    if mode == "agents" {
        println!("{}", agent::AgentHub::new(cfg.clone())?.diagnostics());
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
    if let Err(e) = oc.health().await {
        tracing::warn!(error=%e,"OpenCode 暂不可用；其他 Agent 仍可通过 /model 选择");
    }
    let agents = Arc::new(agent::AgentHub::new(cfg.clone())?);
    store.use_v2_sessions()?;
    feishu.identify().await?;
    let tablet = tablet::Gateway::open(cfg.clone())?;
    let bridge = Arc::new(bridge::Bridge {
        tablet: Some(tablet.clone()),
        cfg,
        store: store.clone(),
        feishu: feishu.clone(),
        agents: agents.clone(),
    });
    let builtin = builtin::Server::bind(bridge.clone()).await?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r=builtin.serve()=>r?,
        _=builtin::maintain(&bridge.cfg, &oc)=>{},
        r=feishu.run(store.clone())=>r?,
        r=bridge.clone().run()=>r?,
        _=voice::maintain(bridge.cfg.clone())=>{},
        r=posture::follow_homework(&bridge.cfg, &store)=>r?,
        r=tablet.serve()=>r?,
        _=async {if let Some(ha)=&bridge.cfg.home_assistant {ha.maintain(&oc).await}else{std::future::pending::<()>().await}}=>{},
        _=tokio::signal::ctrl_c()=>{},
        _=terminate.recv()=>{},
    }
    tracing::info!("正在停止李老师");
    for j in store.jobs("running")? {
        if camera::is_capture(&j.text) || speech::is_command(&j.text) {
            continue;
        }
        let selected = store.agent(&models::scope(&j.key)?)?;
        if let Some(sid) = store.agent_session(&j.key, &selected)?
            && let Err(e) = agents.abort(&sid).await
        {
            tracing::warn!(error=%e,"退出时中止任务失败，重启后将再次尝试");
        }
    }
    Ok(())
}
