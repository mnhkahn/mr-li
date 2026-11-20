//! Agent boundary: session IDs, cancellation and forms are independent of wire protocol.
mod rpc;
use crate::{config::Config, opencode::OpenCode};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rpc::Rpc;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    process::Command,
    sync::{Mutex as AsyncMutex, broadcast},
};
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TEXT: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    OpenCode,
    Antigravity,
    Kilo,
    Trae,
    Cline,
}
impl Kind {
    pub const ALL: [Self; 5] = [
        Self::OpenCode,
        Self::Antigravity,
        Self::Kilo,
        Self::Trae,
        Self::Cline,
    ];
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenCode => "opencode",
            Self::Antigravity => "antigravity",
            Self::Kilo => "kilo",
            Self::Trae => "trae",
            Self::Cline => "cline",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenCode => "OpenCode",
            Self::Antigravity => "Antigravity CLI",
            Self::Kilo => "Kilo CLI",
            Self::Trae => "TRAE 国内版",
            Self::Cline => "Cline CLI",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        let s = if s == "gemini" { "antigravity" } else { s };
        Self::ALL
            .into_iter()
            .find(|k| k.id() == s)
            .context("Agent 选项无效")
    }
    pub fn transport(self) -> &'static str {
        match self {
            Self::OpenCode => "HTTP",
            Self::Trae => "app-server / stdio",
            _ => "ACP / stdio",
        }
    }
}
fn qualified(kind: Kind, raw: &str) -> String {
    format!("agent_{}_{}", kind.id(), URL_SAFE_NO_PAD.encode(raw))
}
fn split(id: &str) -> Result<(Kind, String)> {
    if !id.starts_with("agent_") {
        return Ok((Kind::OpenCode, id.into()));
    }
    let (kind, raw) = id[6..].split_once('_').context("Agent 会话格式无效")?;
    let kind = Kind::parse(kind)?;
    ensure!(kind != Kind::OpenCode, "Agent 会话格式无效");
    let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(raw)?)?;
    ensure!(!raw.is_empty() && raw.len() < 1024, "Agent 会话标识无效");
    Ok((kind, raw))
}
struct Form {
    request: Value,
    value: Value,
}
struct Runtime {
    rpc: Arc<Rpc>,
    raw: String,
    kind: Kind,
    model_catalog: Value,
    active: Mutex<bool>,
    forms: Mutex<HashMap<String, Form>>,
}
pub struct AgentHub {
    cfg: Arc<Config>,
    pub open: OpenCode,
    sessions: AsyncMutex<HashMap<String, Arc<Runtime>>>,
}
impl AgentHub {
    pub fn new(cfg: Arc<Config>) -> Result<Self> {
        Ok(Self {
            open: OpenCode::new(cfg.clone())?,
            cfg,
            sessions: Default::default(),
        })
    }
    pub fn binary(&self, kind: Kind) -> &str {
        match kind {
            Kind::OpenCode => &self.cfg.opencode_bin,
            Kind::Antigravity => &self.cfg.antigravity_bin,
            Kind::Kilo => &self.cfg.kilo_bin,
            Kind::Trae => &self.cfg.trae_bin,
            Kind::Cline => &self.cfg.cline_bin,
        }
    }
    pub fn installed(&self, kind: Kind) -> bool {
        let binary = self.binary(kind);
        let executable = |path: std::path::PathBuf| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        };
        if binary.contains('/') {
            executable(binary.into())
        } else {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .any(|p| executable(p.join(binary)))
        }
    }
    pub fn diagnostics(&self) -> String {
        Kind::ALL
            .iter()
            .map(|k| {
                format!(
                    "{} · {} · {}",
                    k.name(),
                    k.transport(),
                    if self.installed(*k) {
                        "已找到程序（登录与额度待实际请求确认）"
                    } else {
                        "未安装或路径不可用"
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    async fn launch(&self, kind: Kind) -> Result<(Arc<Rpc>, Value)> {
        let mut command = Command::new(self.binary(kind));
        command.current_dir(&self.cfg.workspace);
        match kind {
            Kind::Antigravity => {
                command.arg("acp");
            }
            Kind::Kilo => {
                command.arg("acp").env("KILO_CONFIG_CONTENT",json!({"model":"kilo/kilo-auto/free","small_model":"kilo/kilo-auto/free","agent":{"build":{"model":"kilo/kilo-auto/free"},"plan":{"model":"kilo/kilo-auto/free"},"general":{"model":"kilo/kilo-auto/free"},"explore":{"model":"kilo/kilo-auto/free"},"compaction":{"model":"kilo/kilo-auto/free"},"title":{"model":"kilo/kilo-auto/free"},"summary":{"model":"kilo/kilo-auto/free"}}}).to_string());
            }
            Kind::Trae => {
                command.args(["app-server", "--listen", "stdio://"]);
            }
            Kind::Cline => {
                command.args(["--acp", "--provider", "cline"]);
                // Cline 3.0.74 accepts ACP mcpServers but does not load them.
                // Its core does load this documented settings-file override.
                command.env("CLINE_MCP_SETTINGS_PATH", self.cline_mcp_settings()?);
                // A pre-existing detached hub does not inherit this process's
                // settings path. Keep execution and MCP children in our worker.
                command.env("CLINE_SESSION_BACKEND_MODE", "local");
            }
            Kind::OpenCode => bail!("OpenCode 使用独立 HTTP 服务"),
        }
        // Credentials are owned by the CLI login, never by the Feishu bridge.
        for key in [
            "XIAOLI_HA_TOKEN",
            "FEISHU_APP_SECRET",
            "OPENCODE_SERVER_PASSWORD",
            "CLINE_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_GENAI_USE_VERTEXAI",
            "AGY_ADC_AUTH",
        ] {
            command.env_remove(key);
        }
        let mut command = supervise(command)?;
        let rpc = Rpc::spawn(&mut command)
            .with_context(|| format!("{} 未安装；请配置对应的 MR_LI_*_BIN", kind.name()))?;
        let params = if kind == Kind::Trae {
            json!({"clientInfo":{"name":"mr_li","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})
        } else {
            json!({"protocolVersion":1,"clientInfo":{"name":"mr-li","version":env!("CARGO_PKG_VERSION")},"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}})
        };
        // Kilo cold startup exceeded 30s on the busy 4GB Mac mini. Give
        // initialization more time; subsequent control requests stay bounded.
        let startup_timeout = if kind == Kind::Kilo {
            Duration::from_secs(90)
        } else {
            CONTROL_TIMEOUT
        };
        let init = rpc
            .request("initialize", params, startup_timeout)
            .await
            .with_context(|| {
                if kind == Kind::Trae {
                    "TRAE 协议握手失败，需要支持 app-server 的 2.0 版".into()
                } else {
                    format!(
                        "{} 启动或协议握手失败（等待上限 {} 秒），请检查 CLI 日志",
                        kind.name(),
                        startup_timeout.as_secs()
                    )
                }
            })?;
        if kind == Kind::Trae {
            rpc.notify("initialized", json!({})).await?;
            let account = rpc
                .request(
                    "account/read",
                    json!({"refreshToken":false}),
                    CONTROL_TIMEOUT,
                )
                .await?;
            ensure!(
                account["account"]["type"] == "trae",
                "TRAE 未登录国内账号；请在部署用户下运行 traecli login。不会使用 API Key 或其他账户提供方"
            );
        } else {
            ensure!(init["protocolVersion"] == 1, "Agent ACP 协议版本不兼容");
            if kind == Kind::Antigravity {
                ensure!(
                    init["authMethods"]
                        .as_array()
                        .is_some_and(|m| m.iter().any(|m| m["id"] == "oauth-personal")),
                    "Antigravity 未提供 Google 账号授权方式，请更新 CLI"
                );
                rpc.request(
                    "authenticate",
                    json!({"methodId":"oauth-personal"}),
                    CONTROL_TIMEOUT,
                )
                .await
                .context("请在部署用户下先运行 agy acp --login，用 Google 个人账号登录")?;
            }
        }
        Ok((rpc, init))
    }
    fn mcp_servers(&self) -> Result<Value> {
        let mut servers = Vec::new();
        if !self.cfg.tablet_public_url.is_empty() {
            servers.push(json!({"name":"homework","command":std::env::current_exe()?,"args":["tablet-mcp"],"env":[{"name":"MR_LI_DATA_DIR","value":std::fs::canonicalize(&self.cfg.data_dir)?},{"name":"MR_LI_TABLET_LISTEN","value":self.cfg.tablet_listen}]}));
        }
        if let Some(ha) = &self.cfg.home_assistant {
            servers.push(ha.mcp_acp()?)
        }
        servers.push(crate::builtin::mcp_acp(&self.cfg)?);
        Ok(json!(servers))
    }
    fn cline_mcp_settings(&self) -> Result<std::path::PathBuf> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let directory = self.cfg.data_dir.join("agents/cline");
        std::fs::create_dir_all(&directory)?;
        let directory = std::fs::canonicalize(directory)?;
        let path = directory.join("mcp-settings.json");
        let mut servers = serde_json::Map::new();
        for server in self.mcp_servers()?.as_array().unwrap() {
            let env: serde_json::Map<String, Value> = server["env"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| (e["name"].as_str().unwrap().into(), e["value"].clone()))
                .collect();
            servers.insert(
                server["name"].as_str().unwrap().into(),
                json!({
                    "command":server["command"], "args":server["args"], "env":env,
                    "disabled":false, "autoApprove":[]
                }),
            );
        }
        let temporary = directory.join(format!(".mcp-{}.json", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(&json!({"mcpServers":servers}))?)?;
            std::fs::rename(&temporary, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.context("无法生成 Cline 内置工具配置")?;
        Ok(path)
    }
    async fn select_free(
        &self,
        kind: Kind,
        rpc: &Rpc,
        raw: &str,
        session: &Value,
        preferred: Option<&str>,
    ) -> Result<()> {
        let model = match kind {
            Kind::Antigravity => {
                let model = "gemini-3.8-flash-high";
                ensure!(
                    session["models"]["availableModels"]
                        .as_array()
                        .is_some_and(|models| models.iter().any(|m| m["modelId"] == model)),
                    "Antigravity 未提供免费档 Gemini 3.8 Flash 模型，不会切换到其他模型"
                );
                model.to_owned()
            }
            Kind::Kilo => "kilo/kilo-auto/free".to_owned(),
            Kind::Cline => {
                let catalog = self.cline_catalog().await?;
                let preferred = preferred.unwrap_or(&self.cfg.cline_model);
                cline_choice(
                    &catalog,
                    session,
                    if preferred == "auto" { "" } else { preferred },
                )?
            }
            _ => return Ok(()),
        };
        rpc.request(
            "session/set_model",
            json!({"sessionId":raw,"modelId":model}),
            CONTROL_TIMEOUT,
        )
        .await?;
        Ok(())
    }
    async fn remember(&self, id: &str, runtime: Arc<Runtime>) {
        let mut sessions = self.sessions.lock().await;
        // Bound resident CLIs; persisted sessions can be resumed lazily.
        if sessions.len() >= 8
            && let Some(key) = sessions
                .iter()
                .find(|(_, r)| !*r.active.lock().unwrap())
                .map(|(k, _)| k.clone())
        {
            sessions.remove(&key);
        }
        sessions.insert(id.into(), runtime);
    }
    pub async fn create_for(&self, kind: Kind, selected: Option<&str>) -> Result<String> {
        if kind == Kind::OpenCode {
            return self.open.create(selected).await;
        }
        let (rpc, _) = self.launch(kind).await?;
        let result=if kind==Kind::Trae {
            rpc.request("thread/start",json!({"cwd":self.cfg.workspace,"modelProvider":"trae","approvalPolicy":"never","sandbox":"danger-full-access","config":self.trae_config()?}),CONTROL_TIMEOUT).await
        } else {
            rpc.request("session/new",json!({"cwd":self.cfg.workspace,"mcpServers":self.mcp_servers()?}),CONTROL_TIMEOUT).await
        }.with_context(||format!("{} 无法创建会话，请在部署用户下先登录；额度或账号权限不足时不会切换到付费后端",kind.name()))?;
        let raw = if kind == Kind::Trae {
            result["thread"]["id"].as_str()
        } else {
            result["sessionId"].as_str()
        }
        .context("Agent 未返回会话 ID")?
        .to_owned();
        self.select_free(kind, &rpc, &raw, &result, selected)
            .await?;
        let id = qualified(kind, &raw);
        self.remember(
            &id,
            Arc::new(Runtime {
                rpc,
                raw,
                kind,
                model_catalog: result.clone(),
                active: Mutex::new(false),
                forms: Default::default(),
            }),
        )
        .await;
        Ok(id)
    }
    fn trae_config(&self) -> Result<Value> {
        let mut servers = serde_json::Map::new();
        for server in self.mcp_servers()?.as_array().unwrap() {
            let env: serde_json::Map<String, Value> = server["env"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| (e["name"].as_str().unwrap().into(), e["value"].clone()))
                .collect();
            servers.insert(
                server["name"].as_str().unwrap().into(),
                json!({"command":server["command"],"args":server["args"],"env":env}),
            );
        }
        Ok(if servers.is_empty() {
            json!({})
        } else {
            json!({"mcp_servers":servers})
        })
    }
    async fn runtime(&self, id: &str) -> Result<Arc<Runtime>> {
        let cached = self.sessions.lock().await.get(id).cloned();
        if let Some(runtime) = cached {
            if runtime.rpc.alive().await {
                return Ok(runtime);
            }
            ensure!(
                !*runtime.active.lock().unwrap(),
                "Agent 任务仍在停止，请稍后再试"
            );
            self.sessions.lock().await.remove(id);
        }
        let (kind, raw) = split(id)?;
        let (rpc, init) = self.launch(kind).await?;
        let result = if kind == Kind::Trae {
            rpc.request("thread/resume",json!({"threadId":raw,"cwd":self.cfg.workspace,"modelProvider":"trae","approvalPolicy":"never","sandbox":"danger-full-access","config":self.trae_config()?}),CONTROL_TIMEOUT).await?
        } else {
            ensure!(
                init["agentCapabilities"]["loadSession"] == true,
                "此 Agent 不支持恢复会话，请用 /new 创建新会话"
            );
            rpc.request(
                "session/load",
                json!({"sessionId":raw,"cwd":self.cfg.workspace,"mcpServers":self.mcp_servers()?}),
                CONTROL_TIMEOUT,
            )
            .await?
        };
        // Cline is revalidated with the chat's selected model before every
        // prompt; a legacy global preference must not block session restore.
        if kind != Kind::Cline {
            self.select_free(kind, &rpc, &raw, &result, None).await?;
        }
        let runtime = Arc::new(Runtime {
            rpc,
            raw,
            kind,
            model_catalog: result.clone(),
            active: Mutex::new(false),
            forms: Default::default(),
        });
        self.remember(id, runtime.clone()).await;
        Ok(runtime)
    }
    pub async fn validate_session(&self, id: &str) -> Result<()> {
        if split(id)?.0 == Kind::OpenCode {
            self.open.get(&format!("/api/session/{id}")).await?;
        } else {
            self.runtime(id).await?;
        }
        Ok(())
    }
    pub async fn prompt(&self, id: &str, text: &str, selected: Option<&str>) -> Result<String> {
        self.prompt_with_files(id, text, selected, &[]).await
    }
    pub async fn prompt_with_files(
        &self,
        id: &str,
        text: &str,
        selected: Option<&str>,
        files: &[Value],
    ) -> Result<String> {
        if split(id)?.0 == Kind::OpenCode {
            return self.open.prompt_with_files(id, text, selected, files).await;
        }
        let runtime = self.runtime(id).await?;
        ensure!(
            files.is_empty() || runtime.kind != Kind::Trae,
            "当前 TRAE 后端暂不支持图片，请切换 Agent"
        );
        if runtime.kind == Kind::Cline {
            self.select_free(
                runtime.kind,
                &runtime.rpc,
                &runtime.raw,
                &runtime.model_catalog,
                selected,
            )
            .await?;
        }
        {
            let mut active = runtime.active.lock().unwrap();
            ensure!(!*active, "此 Agent 会话已有任务");
            *active = true;
        }
        let result =
            tokio::time::timeout(self.cfg.task_timeout, run_prompt(&runtime, id, text, files))
                .await;
        *runtime.active.lock().unwrap() = false;
        runtime.forms.lock().unwrap().clear();
        match result {
            Ok(Ok(text)) => Ok(text),
            failure => {
                // Termination is acknowledged before allowing another task. No cross-provider retry.
                runtime
                    .rpc
                    .kill()
                    .await
                    .map_err(|_| crate::opencode::AbortUnconfirmed)?;
                match failure {
                    Ok(Err(e)) => Err(e),
                    _ => bail!("Agent 任务超时，进程已停止；请用 /new 创建新会话"),
                }
            }
        }
    }
    pub async fn abort(&self, id: &str) -> Result<()> {
        if split(id)?.0 == Kind::OpenCode {
            return self.open.abort(id).await;
        }
        let runtime = self.sessions.lock().await.get(id).cloned();
        if let Some(runtime) = runtime {
            // Kill + wait is stronger than an unacknowledged cancel notification and also
            // covers restart recovery, a turn/start race and a wedged upstream model.
            runtime
                .rpc
                .kill()
                .await
                .map_err(|_| crate::opencode::AbortUnconfirmed)?;
            runtime.forms.lock().unwrap().clear();
        }
        Ok(())
    }
    pub async fn forms(&self, id: &str) -> Result<Value> {
        if split(id)?.0 == Kind::OpenCode {
            return self.open.get(&format!("/api/session/{id}/form")).await;
        }
        Ok(self
            .sessions
            .lock()
            .await
            .get(id)
            .map(|r| {
                Value::Array(
                    r.forms
                        .lock()
                        .unwrap()
                        .values()
                        .map(|f| f.value.clone())
                        .collect(),
                )
            })
            .unwrap_or(json!([])))
    }
    pub async fn answer_form(&self, id: &str, form: &str, answer: Option<Value>) -> Result<()> {
        if split(id)?.0 == Kind::OpenCode {
            if let Some(answer) = answer {
                self.open
                    .post(
                        &format!("/api/session/{id}/form/{form}/reply"),
                        json!({"answer":answer}),
                    )
                    .await?;
            } else {
                self.open
                    .delete(&format!("/api/session/{id}/form/{form}"))
                    .await?;
            }
            return Ok(());
        }
        let runtime = self
            .sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .context("Agent 表单已失效")?;
        let (request, value) = {
            let forms = runtime.forms.lock().unwrap();
            let f = forms.get(form).context("提问已处理或失效")?;
            (f.request.clone(), f.value.clone())
        };
        let answer = answer.context("此 Agent 的提问不能单独取消，请用 /stop 停止任务")?;
        let mut answers = serde_json::Map::new();
        for field in value["fields"].as_array().context("提问字段无效")? {
            let key = field["key"].as_str().context("提问字段无效")?;
            let input = answer.get(key).context("提问答案缺失")?;
            let values = if let Some(v) = input.as_array() {
                v.clone()
            } else {
                vec![input.clone()]
            };
            ensure!(values.iter().all(Value::is_string), "提问答案必须是文字");
            answers.insert(key.into(), json!({"answers":values}));
        }
        runtime
            .rpc
            .reply(request, json!({"answers":answers}))
            .await?;
        runtime.forms.lock().unwrap().remove(form);
        Ok(())
    }
    pub async fn cline_catalog(&self) -> Result<Value> {
        let response = reqwest::Client::new()
            .get("https://api.cline.bot/api/v1/ai/cline/recommended-models")
            .timeout(Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?;
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 2 * 1024 * 1024,
                "Cline 免费模型目录过大"
            );
            bytes.extend_from_slice(&chunk);
        }
        let catalog: Value = serde_json::from_slice(&bytes)?;
        Ok(catalog)
    }
    // OpenCode-specific capabilities stay behind this boundary.
    pub async fn catalog(&self) -> Result<Value> {
        self.open.catalog().await
    }
    pub async fn get(&self, path: &str) -> Result<Value> {
        self.open.get(path).await
    }
    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.open.post(path, body).await
    }
    pub async fn approve_pending(&self, id: &str) -> Result<usize> {
        if split(id)?.0 == Kind::OpenCode {
            self.open.approve_pending(id).await
        } else {
            Ok(0)
        }
    }
}

fn cline_choice(catalog: &Value, session: &Value, preferred: &str) -> Result<String> {
    let free: HashSet<&str> = catalog["free"]
        .as_array()
        .context("Cline 免费模型目录格式不兼容")?
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    let models = session["models"]["availableModels"]
        .as_array()
        .context("Cline 未返回模型目录，请先运行 cline auth 登录 Cline 账号")?;
    let offered = models
        .iter()
        .filter_map(|m| m["modelId"].as_str())
        .find(|id| free.contains(id) && (preferred.is_empty() || *id == preferred));
    // Cline 3.0.74 omits cline-free/* promo routes from ACP's model list.
    // These IDs come directly from the live official free catalog; submit the
    // exact free route and let set_model/prompt reject unavailable promotions.
    let promo = catalog["free"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str())
        .find(|id| id.starts_with("cline-free/") && (preferred.is_empty() || *id == preferred));
    offered.or(promo).map(str::to_owned).context("Cline 当前账号没有可验证的免费模型；请在 cline 中选择 Cline 账户提供方，或用 /new 刷新免费目录")
}

pub use rpc::worker;
fn supervise(command: Command) -> Result<Command> {
    // Protocol unit tests use a tiny fake executable directly; the production
    // worker is exercised by scripts/smoke-agents.py against the built binary.
    if cfg!(test) {
        return Ok(command);
    }
    let source = command.as_std();
    let mut worker = Command::new(std::env::current_exe()?);
    worker.args([
        std::ffi::OsStr::new("agent-worker"),
        std::ffi::OsStr::new("--"),
        source.get_program(),
    ]);
    worker.args(source.get_args());
    if let Some(path) = source.get_current_dir() {
        worker.current_dir(path);
    }
    for (key, value) in source.get_envs() {
        if let Some(value) = value {
            worker.env(key, value);
        } else {
            worker.env_remove(key);
        }
    }
    Ok(worker)
}

async fn run_prompt(runtime: &Runtime, id: &str, text: &str, files: &[Value]) -> Result<String> {
    let mut events = runtime.rpc.subscribe();
    let mut output = String::new();
    if runtime.kind == Kind::Trae {
        let response = runtime
            .rpc
            .request(
                "turn/start",
                json!({"threadId":runtime.raw,"input":[{"type":"text","text":text}]}),
                CONTROL_TIMEOUT,
            )
            .await?;
        let turn = response["turn"]["id"]
            .as_str()
            .context("TRAE 未返回 turn ID")?;
        let mut items = HashMap::<String, String>::new();
        let mut order = Vec::<String>::new();
        loop {
            let event = next_event(&mut events).await?;
            let params = &event["params"];
            if params["threadId"] != runtime.raw {
                if !event["id"].is_null() {
                    runtime.rpc.unsupported(event["id"].clone()).await?;
                }
                continue;
            }
            if params
                .get("turnId")
                .is_some_and(|t| !t.is_null() && t != turn)
            {
                continue;
            }
            if event.get("id").is_some() {
                handle_request(runtime, id, &event).await?;
                continue;
            }
            match event["method"].as_str() {
                Some("item/agentMessage/delta") => {
                    let key = params["itemId"].as_str().context("TRAE 消息 ID 缺失")?;
                    if !items.contains_key(key) {
                        order.push(key.into());
                    }
                    let value = items.entry(key.into()).or_default();
                    append(value, params["delta"].as_str().unwrap_or(""))?;
                    ensure!(
                        items.values().map(String::len).sum::<usize>() <= MAX_TEXT,
                        "Agent 结果过长"
                    );
                }
                Some("item/completed") if params["item"]["type"] == "agentMessage" => {
                    let key = params["item"]["id"].as_str().context("TRAE 消息 ID 缺失")?;
                    if let Some(text) = params["item"]["text"].as_str() {
                        ensure!(text.len() <= MAX_TEXT, "Agent 结果过长");
                        if !items.contains_key(key) {
                            order.push(key.into());
                        }
                        items.insert(key.into(), text.into());
                    }
                }
                Some("turn/completed") if params["turn"]["id"] == turn => {
                    ensure!(
                        params["turn"]["status"] == "completed",
                        "TRAE 任务未完成：{} {}",
                        params["turn"]["status"],
                        params["turn"]["error"]["message"]
                            .as_str()
                            .unwrap_or("")
                            .chars()
                            .take(600)
                            .collect::<String>()
                    );
                    // Prefer final-answer items supplied with the completed turn, then the
                    // full completed message; never forward reasoning/tool output.
                    if let Some(turn_items) = params["turn"]["items"].as_array() {
                        for item in turn_items.iter().filter(|i| i["type"] == "agentMessage") {
                            if let Some(text) = item["text"].as_str() {
                                append(&mut output, text)?;
                                append(&mut output, "\n")?;
                            }
                        }
                    }
                    if output.is_empty() {
                        for key in &order {
                            append(&mut output, &items[key])?;
                            append(&mut output, "\n")?;
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
    } else {
        let response = runtime.rpc.request(
            "session/prompt",
            {
                let mut blocks = vec![json!({"type":"text","text":text})];
                for file in files {
                    let uri = file["uri"].as_str().context("图片缺少 URI")?;
                    let (_, data) = uri.split_once(",").context("图片 URI 无效")?;
                    blocks.push(json!({"type":"image","mimeType":file["mime"],"data":data}));
                }
                json!({"sessionId":runtime.raw,"prompt":blocks})
            },
            Duration::from_secs(24 * 3600),
        );
        tokio::pin!(response);
        let result = loop {
            tokio::select! { biased;
                event=events.recv()=>consume_acp(runtime,id,event.context("Agent 事件流丢失")?,&mut output).await?,
                response=&mut response=>break response?,
            }
        };
        // Reader publishes preceding notifications before resolving the prompt response.
        while let Ok(event) = events.try_recv() {
            consume_acp(runtime, id, event, &mut output).await?;
        }
        ensure!(
            result["stopReason"] == "end_turn",
            "Agent 未正常完成任务：{}",
            result["stopReason"]
        );
    }
    ensure!(!output.trim().is_empty(), "Agent 未返回可展示的文本结果");
    Ok(output.trim().into())
}
fn append(output: &mut String, text: &str) -> Result<()> {
    ensure!(output.len() + text.len() <= MAX_TEXT, "Agent 结果过长");
    output.push_str(text);
    Ok(())
}
async fn next_event(events: &mut broadcast::Receiver<Value>) -> Result<Value> {
    let v = events.recv().await.context("Agent 事件流丢失")?;
    ensure!(v["method"] != "mr-li/disconnected", "Agent 连接已断开");
    Ok(v)
}
async fn consume_acp(runtime: &Runtime, id: &str, event: Value, output: &mut String) -> Result<()> {
    ensure!(event["method"] != "mr-li/disconnected", "Agent 连接已断开");
    if event.get("id").is_some() {
        return handle_request(runtime, id, &event).await;
    }
    let params = &event["params"];
    if params["sessionId"] == runtime.raw
        && event["method"] == "session/update"
        && params["update"]["sessionUpdate"] == "agent_message_chunk"
        && params["update"]["content"]["type"] == "text"
    {
        append(
            output,
            params["update"]["content"]["text"].as_str().unwrap_or(""),
        )?;
    }
    Ok(())
}
async fn handle_request(runtime: &Runtime, id: &str, event: &Value) -> Result<()> {
    let p = &event["params"];
    let owned = p["sessionId"] == runtime.raw || p["threadId"] == runtime.raw;
    if !owned {
        return runtime.rpc.unsupported(event["id"].clone()).await;
    }
    match event["method"].as_str() {
        Some("session/request_permission") => {
            let option = p["options"]
                .as_array()
                .and_then(|a| a.iter().find(|o| o["kind"] == "allow_once"))
                .and_then(|o| o["optionId"].as_str());
            runtime
                .rpc
                .reply(
                    event["id"].clone(),
                    option
                        .map(|o| json!({"outcome":{"outcome":"selected","optionId":o}}))
                        .unwrap_or(json!({"outcome":{"outcome":"cancelled"}})),
                )
                .await
        }
        Some("item/commandExecution/requestApproval" | "item/fileChange/requestApproval") => {
            runtime
                .rpc
                .reply(event["id"].clone(), json!({"decision":"accept"}))
                .await
        }
        Some("item/tool/requestUserInput") => {
            let fields=p["questions"].as_array().context("Agent 提问格式不兼容")?.iter().map(|q|{
                ensure!(q["isSecret"]!=true,"Agent 请求了私密信息，请在 CLI 中完成操作");
                Ok(json!({"key":q["id"],"type":if q["multiSelect"]==true{"multiselect"}else{"string"},"required":true,"title":q["header"],"description":q["question"],"custom":q["isOther"]==true || q["options"].is_null(),"options":q["options"].as_array().map(|opts|opts.iter().map(|o|json!({"label":o["label"],"value":o["label"]})).collect::<Vec<_>>()).unwrap_or_default()}))
            }).collect::<Result<Vec<_>>>()?;
            ensure!(
                !fields.is_empty() && fields.len() <= 20,
                "Agent 提问数量无效"
            );
            let token = uuid::Uuid::new_v4().simple().to_string();
            let mut forms = runtime.forms.lock().unwrap();
            ensure!(forms.len() < 20, "Agent 待处理提问过多");
            forms.insert(token.clone(),Form{request:event["id"].clone(),value:json!({"id":token,"sessionID":id,"title":"Agent 需要补充信息","fields":fields})});
            Ok(())
        }
        _ => runtime.rpc.unsupported(event["id"].clone()).await,
    }
}

pub fn picker(current: Kind, token: &str, switched: bool) -> Value {
    json!({"schema":"2.0","config":{"update_multi":true,"enable_forward":false,"summary":{"content":format!("李老师 · Agent · {}",current.name())}},"header":{"title":{"tag":"plain_text","content":"选择 Agent"},"template":if switched{"green"}else{"blue"}},"body":{"elements":[{"tag":"markdown","content":format!("**{}：{}**",if switched{"已切换"}else{"当前 Agent"},current.name())},{"tag":"select_static","name":"agent_selector","width":"fill","initial_option":current.id(),"placeholder":{"tag":"plain_text","content":"展开选择 Agent"},"options":Kind::ALL.iter().map(|k|json!({"text":{"tag":"plain_text","content":k.name()},"value":k.id()})).collect::<Vec<_>>(),"behaviors":[{"type":"callback","value":{"action":"select_agent","token":token}}]},{"tag":"markdown","text_size":"notation","content":"选择只影响你在当前聊天的任务。各 Agent 分别保留会话；首次使用需在服务运行的电脑上安装并登录。卡片 30 分钟内有效。"}]}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cline_only_accepts_live_free_models_and_never_falls_back_to_paid() {
        let catalog = json!({"free":[{"id":"free-a"}],"recommended":[{"id":"paid"}]});
        let session =
            json!({"models":{"availableModels":[{"modelId":"paid"},{"modelId":"free-a"}]}});
        assert_eq!(cline_choice(&catalog, &session, "").unwrap(), "free-a");
        assert!(cline_choice(&catalog, &session, "paid").is_err());
        assert!(cline_choice(&json!({"free":[]}), &session, "").is_err());
        assert!(
            cline_choice(
                &catalog,
                &json!({"models":{"availableModels":[{"modelId":"paid"}]}}),
                ""
            )
            .is_err()
        );
        assert!(cline_choice(&json!({}), &session, "").is_err());
        let promo = json!({"free":[{"id":"cline-free/test"}]});
        assert_eq!(
            cline_choice(&promo, &session, "").unwrap(),
            "cline-free/test"
        );
        assert!(cline_choice(&promo, &session, "paid").is_err());
        assert!(cline_choice(&json!({"free":[]}), &session, "cline-free/test").is_err());
    }
    #[test]
    fn session_ids_round_trip_without_path_injection() {
        for kind in [Kind::Antigravity, Kind::Kilo, Kind::Trae, Kind::Cline] {
            let id = qualified(kind, "a/b?c#中文");
            assert!(!id.contains('/'));
            assert_eq!(split(&id).unwrap(), (kind, "a/b?c#中文".into()));
        }
        assert_eq!(
            split("ses_legacy").unwrap(),
            (Kind::OpenCode, "ses_legacy".into())
        );
        assert!(split("agent_paid_bad").is_err());
    }
    #[test]
    fn dropdown_only_contains_supported_agents() {
        let c = picker(Kind::OpenCode, "token", false);
        let options = c["body"]["elements"][1]["options"].as_array().unwrap();
        assert_eq!(options.len(), 5);
        assert!(
            options
                .iter()
                .all(|o| Kind::parse(o["value"].as_str().unwrap()).is_ok())
        );
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod protocol_tests;
