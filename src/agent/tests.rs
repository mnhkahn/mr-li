use super::*;
use std::{os::unix::fs::PermissionsExt, path::PathBuf};
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl Fixture {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("mr-li-agent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path)?;
        let file = path.join("fake-agent");
        std::fs::write(&file, include_str!("../../scripts/fixtures/fake-agent.py"))?;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700))?;
        Ok(Self(path))
    }
    fn hub(&self) -> Result<AgentHub> {
        let mut cfg = Config::fixture();
        let bin = self.0.join("fake-agent").to_string_lossy().into_owned();
        cfg.antigravity_bin = bin.clone();
        cfg.kilo_bin = bin.clone();
        cfg.trae_bin = bin.clone();
        cfg.cline_bin = bin;
        cfg.workspace = self.0.clone();
        cfg.data_dir = self.0.clone();
        AgentHub::new(Arc::new(cfg))
    }
    fn log(&self) -> String {
        std::fs::read_to_string(self.0.join("calls.jsonl")).unwrap_or_default()
    }
}
#[tokio::test]
async fn acp_streams_owned_output_reuses_process_and_pins_free_kilo() -> Result<()> {
    let fixture = Fixture::new()?;
    let hub = fixture.hub()?;
    for kind in [Kind::Antigravity, Kind::Kilo] {
        let id = hub.create_for(kind, None).await?;
        assert_eq!(hub.prompt(&id, "hello", None).await?, "hello world");
        assert_eq!(hub.prompt(&id, "again", None).await?, "hello world");
        hub.abort(&id).await?;
    }
    let calls = fixture.log();
    assert_eq!(calls.matches("\"method\": \"initialize\"").count(), 2);
    assert!(calls.contains("oauth-personal"));
    assert!(calls.contains("kilo/kilo-auto/free"));
    assert!(calls.contains("allow-once"));
    Ok(())
}
#[tokio::test]
async fn trae_native_server_handles_early_events_and_user_input() -> Result<()> {
    let fixture = Fixture::new()?;
    let hub = fixture.hub()?;
    let id = hub.create_for(Kind::Trae, None).await?;
    assert_eq!(hub.prompt(&id, "hello", None).await?, "native answer");
    let prompt = hub.prompt(&id, "question", None);
    tokio::pin!(prompt);
    let forms = tokio::select! {
        result=&mut prompt=>panic!("prompt finished before user input: {result:?}"),
        forms=async {loop {let forms=hub.forms(&id).await?;if forms.as_array().is_some_and(|f|!f.is_empty()){break Ok::<_,anyhow::Error>(forms)}tokio::time::sleep(Duration::from_millis(10)).await;}}=>forms?,
        _=tokio::time::sleep(Duration::from_secs(3))=>panic!("form not received"),
    };
    let form = forms[0]["id"].as_str().unwrap();
    assert!(
        hub.answer_form(
            &qualified(Kind::Trae, "other"),
            form,
            Some(json!({"choice":"A"}))
        )
        .await
        .is_err()
    );
    hub.answer_form(&id, form, Some(json!({"choice":"A"})))
        .await?;
    assert_eq!(prompt.await?, "native answer");
    assert_eq!(hub.forms(&id).await?, json!([]));
    assert!(
        hub.answer_form(&id, form, Some(json!({"choice":"A"})))
            .await
            .is_err()
    );
    assert!(fixture.log().contains("thread/start"));
    assert!(!fixture.log().contains("session/prompt"));
    hub.abort(&id).await?;
    Ok(())
}
#[tokio::test]
async fn cancellation_terminates_runtime_and_never_silently_replays() -> Result<()> {
    let fixture = Fixture::new()?;
    let hub = Arc::new(fixture.hub()?);
    let id = hub.create_for(Kind::Kilo, None).await?;
    let prompt = {
        let hub = hub.clone();
        let id = id.clone();
        tokio::spawn(async move { hub.prompt(&id, "wait", None).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    hub.abort(&id).await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), prompt)
            .await??
            .is_err()
    );
    hub.validate_session(&id).await?;
    assert_eq!(
        fixture
            .log()
            .matches("\"method\": \"session/prompt\"")
            .count(),
        1
    );
    hub.abort(&id).await?;
    Ok(())
}
#[tokio::test]
async fn persisted_session_loads_without_creating_a_new_conversation() -> Result<()> {
    let fixture = Fixture::new()?;
    let hub = fixture.hub()?;
    let id = hub.create_for(Kind::Kilo, None).await?;
    hub.abort(&id).await?;
    drop(hub);
    let hub = fixture.hub()?;
    hub.validate_session(&id).await?;
    assert_eq!(hub.prompt(&id, "resume", None).await?, "hello world");
    assert_eq!(
        fixture.log().matches("\"method\": \"session/new\"").count(),
        1
    );
    assert_eq!(
        fixture
            .log()
            .matches("\"method\": \"session/load\"")
            .count(),
        1
    );
    hub.abort(&id).await?;
    Ok(())
}
#[tokio::test]
async fn failed_auth_and_missing_cli_do_not_fall_back_to_opencode() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut cfg = fixture.hub()?.cfg.as_ref().clone();
    cfg.antigravity_bin = "/no/such/mr-li-agent".into();
    let hub = AgentHub::new(Arc::new(cfg))?;
    assert!(hub.create_for(Kind::Antigravity, None).await.is_err());
    assert!(fixture.log().is_empty());
    Ok(())
}

#[tokio::test]
async fn timeout_and_quota_failures_stop_without_cross_backend_retry() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut cfg = fixture.hub()?.cfg.as_ref().clone();
    cfg.task_timeout = Duration::from_millis(100);
    let hub = AgentHub::new(Arc::new(cfg))?;
    let id = hub.create_for(Kind::Kilo, None).await?;
    assert!(
        hub.prompt(&id, "wait", None)
            .await
            .unwrap_err()
            .to_string()
            .contains("超时")
    );
    assert!(!hub.sessions.lock().await[&id].rpc.alive().await);
    let id = hub.create_for(Kind::Trae, None).await?;
    assert!(
        hub.prompt(&id, "quota", None)
            .await
            .unwrap_err()
            .to_string()
            .contains("free quota exhausted")
    );
    assert!(!hub.sessions.lock().await[&id].rpc.alive().await);
    assert_eq!(
        fixture.log().matches("\"method\": \"initialize\"").count(),
        2
    );
    Ok(())
}

#[test]
fn home_assistant_mcp_is_passed_to_acp_and_trae_without_credentials() -> Result<()> {
    let mut cfg = Config::fixture();
    cfg.home_assistant = crate::home_assistant::Settings::from_settings(
        |k, _| match k {
            "XIAOLI_HA_URL" => "http://127.0.0.1:8123".into(),
            "XIAOLI_HA_TOKEN" => "private-ha-token".into(),
            _ => String::new(),
        },
        "/private/ha.env",
    )?;
    let hub = AgentHub::new(Arc::new(cfg))?;
    let acp = hub.mcp_servers()?;
    assert_eq!(acp[0]["name"], "home_assistant");
    assert_eq!(acp[0]["args"], json!(["ha-mcp"]));
    let trae = hub.trae_config()?;
    assert_eq!(
        trae["mcp_servers"]["home_assistant"]["env"]["MR_LI_ENV"],
        "/private/ha.env"
    );
    let teacher = acp
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "teacher")
        .unwrap();
    assert_eq!(teacher["args"], json!(["builtin-mcp"]));
    assert_eq!(
        trae["mcp_servers"]["teacher"]["args"],
        json!(["builtin-mcp"])
    );
    assert!(!acp.to_string().contains("private-ha-token"));
    assert!(!trae.to_string().contains("private-ha-token"));
    Ok(())
}
