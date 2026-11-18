//! mr-li tablet gateway v1 and local stdio MCP adapter; no legacy server transport.
use crate::config::Config;
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Multipart, Path as UrlPath, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot};

const MAX_IMAGE: usize = 2 * 1024 * 1024;
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |v, (a, b)| v | (a ^ b)) == 0
}
fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}
struct Registry(Mutex<Connection>);
impl Registry {
    fn open(dir: &Path) -> Result<Self> {
        private_dir(dir)?;
        let path = dir.join("teacher-devices-v1.sqlite3");
        let db = Connection::open(&path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS pairings(code TEXT PRIMARY KEY,expires INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS devices(id TEXT PRIMARY KEY,name TEXT NOT NULL,token TEXT NOT NULL);")?;
        Ok(Self(Mutex::new(db)))
    }
    fn pairing(&self) -> Result<String> {
        let code = secret();
        let db = self.0.lock().unwrap();
        db.execute("DELETE FROM pairings WHERE expires <= ?", [now()])?;
        db.execute(
            "INSERT INTO pairings VALUES (?,?)",
            params![code, now() + 300],
        )?;
        Ok(code)
    }
    fn claim(&self, body: &Value) -> Result<(String, String, String)> {
        let code = body["code"].as_str().unwrap_or("");
        let id = body["device_id"].as_str().unwrap_or("");
        let name = body["device_name"].as_str().unwrap_or("学习平板");
        ensure!(
            body["device_kind"] == "android"
                && !id.is_empty()
                && id.len() <= 128
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')),
            "设备格式错误"
        );
        ensure!(name.len() <= 160, "设备名称过长");
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        ensure!(
            tx.execute(
                "DELETE FROM pairings WHERE code=? AND expires>?",
                params![code, now()]
            )? == 1,
            "配对码已使用或过期"
        );
        let token = secret();
        tx.execute("INSERT INTO devices VALUES (?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,token=excluded.token",params![id,name,token])?;
        tx.commit()?;
        Ok((id.into(), name.into(), token))
    }
    fn authorize(&self, id: &str, token: &str) -> bool {
        let stored = self
            .0
            .lock()
            .unwrap()
            .query_row("SELECT token FROM devices WHERE id=?", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional();
        matches!(stored, Ok(Some(v)) if same_secret(&v,token))
    }
    fn devices(&self) -> Result<Vec<Value>> {
        let db = self.0.lock().unwrap();
        let mut q = db.prepare("SELECT id,name FROM devices ORDER BY name,id")?;
        Ok(q.query_map([], |r| {
            Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
struct Peer {
    generation: String,
    tx: mpsc::Sender<Value>,
    tools: Value,
}
type Pending = HashMap<(String, String, u64), oneshot::Sender<Value>>;
pub struct Gateway {
    cfg: Arc<Config>,
    pub notices: crate::notices::Notices,
    registry: Registry,
    control: String,
    peers: Mutex<HashMap<String, Peer>>,
    pending: Mutex<Pending>,
    next: std::sync::atomic::AtomicU64,
    capture_mu: Mutex<()>,
    instance: String,
}
type ApiResult = std::result::Result<Json<Value>, ApiError>;
struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
fn error(status: StatusCode, e: impl std::fmt::Display) -> ApiError {
    ApiError(status, e.to_string())
}
impl Gateway {
    pub fn open(cfg: Arc<Config>) -> Result<Arc<Self>> {
        let dir = cfg.data_dir.join("tablet");
        let registry = Registry::open(&dir)?;
        let key = dir.join("control-token");
        let control = match std::fs::read_to_string(&key) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&key)?;
                let v = secret();
                f.write_all(v.as_bytes())?;
                v
            }
            Err(e) => return Err(e.into()),
        };
        ensure!(control.len() >= 32, "平板网关控制凭证损坏");
        let notices = crate::notices::Notices::open(&dir.join("notices.sqlite3"))?;
        Ok(Arc::new(Self {
            cfg,
            notices,
            registry,
            control,
            peers: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            next: std::sync::atomic::AtomicU64::new(1),
            capture_mu: Mutex::new(()),
            instance: uuid::Uuid::new_v4().to_string(),
        }))
    }
    fn capture_path(&self, id: &str) -> PathBuf {
        self.cfg
            .data_dir
            .join("tablet/report-captures")
            .join(format!(
                "{}.json",
                uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, id.as_bytes())
            ))
    }
    fn save_capture(&self, path: &Path, value: &Value) -> Result<()> {
        private_dir(path.parent().context("采集目录缺失")?)?;
        let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temp, serde_json::to_vec(value)?)?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(temp, path)?;
        Ok(())
    }
    /// Persist admission before dispatch. Retries/restarts must never repeat a shutter action.
    pub fn report_capture(self: &Arc<Self>, id: &str) -> Result<Value> {
        let _lock = self.capture_mu.lock().unwrap();
        let path = self.capture_path(id);
        if path.exists() {
            let mut capture: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            if capture["status"] == "pending"
                && (capture["instance"] != self.instance
                    || now() - capture["started"].as_i64().unwrap_or(0) > 160)
            {
                capture["status"] = json!("failed");
                capture["error"] = json!("平板采集超时或被服务重启中断，本次不重新拍摄");
                self.save_capture(&path, &capture)?;
            }
            return Ok(capture);
        }
        let candidates: Vec<String> = self
            .peers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| {
                p.tools.as_array().is_some_and(|tools| {
                    tools.iter().any(|t| t["name"] == "self.camera.take_photo")
                })
            })
            .map(|(id, _)| id.clone())
            .collect();
        let question = format!(
            "mr-li-report:{}\n采集本次十分钟汇总的平板照片和当前屏幕截图，原样附图，无需模型分析。",
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, id.as_bytes())
        );
        let mut capture = json!({"status":"pending","started":now(),"instance":self.instance,"question":question,"images":[]});
        if candidates.len() != 1 {
            capture["status"] = json!("failed");
            capture["error"] = json!(if candidates.is_empty() {
                "平板离线或拍照工具未就绪，未取得平板照片和屏幕截图"
            } else {
                "多个平板在线，无法确定汇总采集设备，未发起拍照"
            });
            self.save_capture(&path, &capture)?;
            return Ok(capture);
        }
        let device = candidates[0].clone();
        capture["device"] = json!(device);
        self.save_capture(&path, &capture)?;
        let gateway = self.clone();
        tokio::spawn(async move {
            let result = gateway
                .call(
                    &device,
                    "tools/call",
                    json!({"name":"self.camera.take_photo","arguments":{"question":question}}),
                )
                .await;
            let _lock = gateway.capture_mu.lock().unwrap();
            let update = || -> Result<()> {
                let mut capture: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
                // An upload is authoritative even if the subsequent RPC reply was lost.
                if capture["status"] == "pending" {
                    capture["status"] = json!("failed");
                    capture["error"] = json!(match result {
                        Err(e) => format!("平板采集失败：{e}"),
                        Ok(_) => "平板拍照请求结束，但未收到本次照片上传".into(),
                    });
                    gateway.save_capture(&path, &capture)?;
                }
                Ok(())
            };
            if let Err(e) = update() {
                tracing::warn!(error=%e,"保存平板汇总采集结果失败");
            }
        });
        Ok(capture)
    }
    fn record_report_upload(
        &self,
        device: &str,
        question: &str,
        images: &Value,
        screenshot_error: &str,
    ) -> Result<()> {
        let Some(marker) = question
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("mr-li-report:"))
        else {
            return Ok(());
        };
        let Ok(id) = uuid::Uuid::parse_str(marker) else {
            return Ok(());
        };
        let path = self
            .cfg
            .data_dir
            .join("tablet/report-captures")
            .join(format!("{id}.json"));
        let _lock = self.capture_mu.lock().unwrap();
        if !path.exists() {
            return Ok(());
        }
        let mut capture: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        if capture["device"] != device
            || capture["question"] != question
            || capture["instance"] != self.instance
            || capture["status"] != "pending"
        {
            return Ok(());
        }
        capture["images"] = images.clone();
        capture["screenshot_error"] = json!(screenshot_error);
        capture["captured_at"] = json!(now());
        capture["status"] = json!("ready");
        self.save_capture(&path, &capture)
    }
    pub fn validate_report_image(&self, device: &str, path: &Path) -> Result<()> {
        let expected = std::fs::canonicalize(self.cfg.data_dir.join("tablet/media").join(device))?;
        let actual = std::fs::canonicalize(path)?;
        ensure!(
            actual.parent() == Some(expected.as_path())
                && path
                    .file_name()
                    .and_then(|p| p.to_str())
                    .is_some_and(valid_media_file),
            "平板汇总图片路径不匹配"
        );
        Ok(())
    }
    fn routes(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/tablet/v1/pair", post(pair))
            .route("/tablet/v1/ws", get(upgrade))
            .route("/tablet/v1/snapshot", post(snapshot))
            .route("/tablet/v1/photo-check", post(explain))
            .route("/tablet/v1/media/{file}", get(media))
            .route("/_tablet/rpc", post(local_rpc))
            .layer(DefaultBodyLimit::max(2 * MAX_IMAGE + 64 * 1024))
            .with_state(self.clone())
    }
    pub async fn serve(self: Arc<Self>) -> Result<()> {
        if self.cfg.tablet_public_url.is_empty() {
            return std::future::pending().await;
        }
        public_url(&self.cfg.tablet_public_url)?;
        local_address(&self.cfg.tablet_listen)?;
        let listener = tokio::net::TcpListener::bind(&self.cfg.tablet_listen).await?;
        self.notices.recover()?;
        let gateway = self.clone();
        tokio::spawn(async move {
            let oc = match crate::opencode::OpenCode::new(gateway.cfg.clone()) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error=%e,"初始化平板 MCP 失败");
                    return;
                }
            };
            let mut first = true;
            loop {
                let result = async {
                    gateway.register_mcp(&oc, first).await?;
                    first = false;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(e) = result {
                    tracing::warn!(error=%e,"平板 MCP 接入失败，稍后重试");
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        let gateway = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = gateway.deliver_notices().await {
                    tracing::warn!(error=%e,"通知同步失败");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        tracing::info!(listen=%self.cfg.tablet_listen,"平板网关已启动");
        axum::serve(listener, self.routes()).await?;
        Ok(())
    }
    async fn deliver_notices(&self) -> Result<()> {
        for item in self.notices.pending(now())? {
            let device = item["device"].as_str().context("通知设备缺失")?;
            let id = item["id"].as_str().context("通知 ID 缺失")?;
            let supported = self.peers.lock().unwrap().get(device).map(|peer| {
                peer.tools
                    .as_array()
                    .is_some_and(|tools| tools.iter().any(|t| t["name"] == "self.notify.send"))
            });
            if supported != Some(true) {
                self.notices.defer(
                    id,
                    if supported.is_none() {
                        "平板离线，等待连接"
                    } else {
                        "平板未声明通知工具"
                    },
                    now(),
                )?;
                continue;
            }
            if !self.notices.claim(id, now())? {
                continue;
            }
            let result=tokio::time::timeout(Duration::from_secs(10),self.call(device,"tools/call",json!({"name":"self.notify.send","arguments":{"title":"李老师通知","body":item["body"]}}))).await;
            let (delivered, error) = match result {
                Ok(Ok(result)) if notice_delivered(&result) => (true, String::new()),
                Ok(Err(e)) => (false, format!("未确认送达：{e}")),
                _ => (false, "平板未确认通知，未自动重发；请先查看 APP".into()),
            };
            self.notices.finish(id, delivered, &error)?;
        }
        Ok(())
    }
    async fn register_mcp(&self, oc: &crate::opencode::OpenCode, force: bool) -> Result<()> {
        let status = oc.get("/api/mcp").await?;
        let connected = status.as_array().is_some_and(|servers| {
            servers.iter().any(|server| {
                server["name"] == "homework" && server["status"]["status"] == "connected"
            })
        });
        if force || !connected {
            oc.request(
                reqwest::Method::PUT,
                "/api/experimental/mcp/homework",
                Some(json!({"config":mcp_config(&self.cfg)?})),
                Duration::from_secs(25),
            )
            .await?;
            let status = oc.get("/api/mcp").await?;
            ensure!(
                status.as_array().is_some_and(|servers| servers
                    .iter()
                    .any(|server| server["name"] == "homework"
                        && server["status"]["status"] == "connected")),
                "OpenCode 未连接平板 MCP"
            );
            tracing::info!("平板 MCP 已接入 OpenCode");
        }
        Ok(())
    }
    fn device(&self, headers: &HeaderMap) -> std::result::Result<String, ApiError> {
        let id = headers
            .get("Device-Id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let token = bearer(headers);
        if !self.registry.authorize(id, token) {
            return Err(error(StatusCode::UNAUTHORIZED, "请重新扫码配对"));
        }
        Ok(id.into())
    }
    async fn call(&self, device: &str, method: &str, params: Value) -> Result<Value> {
        let (tx, generation) = {
            let peers = self.peers.lock().unwrap();
            let peer = peers.get(device).context("平板未连接")?;
            (peer.tx.clone(), peer.generation.clone())
        };
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key = (device.to_owned(), generation.clone(), id);
        let (reply, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(key.clone(), reply);
        let request = json!({"type":"mcp","session_id":generation,"payload":{"jsonrpc":"2.0","id":id,"method":method,"params":params}});
        // Scope the guard so cancellation/HTTP disconnect also releases correlation state.
        struct Cleanup<'a>(&'a Mutex<Pending>, (String, String, u64));
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                self.0.lock().unwrap().remove(&self.1);
            }
        }
        let _cleanup = Cleanup(&self.pending, key);
        let response = tokio::time::timeout(Duration::from_secs(150), async {
            tx.send(request).await.context("平板连接已关闭")?;
            rx.await.context("平板已断开")
        })
        .await
        .context("平板操作超时（未自动重试）")??;
        if !response["error"].is_null() {
            bail!("平板操作失败：{}", response["error"]);
        }
        Ok(response["result"].clone())
    }
    async fn socket(self: Arc<Self>, device: String, token: String, mut ws: WebSocket) {
        let hello = tokio::time::timeout(Duration::from_secs(15), ws.recv()).await;
        if !matches!(hello, Ok(Some(Ok(Message::Text(ref t)))) if serde_json::from_str::<Value>(t).is_ok_and(|v|v["type"]=="hello" && v["service"]=="mr-li" && v["protocol"]==1))
        {
            return;
        }
        let generation = uuid::Uuid::new_v4().to_string();
        if ws
            .send(Message::Text(
                json!({"type":"hello","service":"mr-li","protocol":1,"session_id":generation})
                    .to_string()
                    .into(),
            ))
            .await
            .is_err()
        {
            return;
        }
        let (tx, mut rx) = mpsc::channel::<Value>(16);
        self.peers.lock().unwrap().insert(
            device.clone(),
            Peer {
                generation: generation.clone(),
                tx,
                tools: json!([]),
            },
        );
        self.pending
            .lock()
            .unwrap()
            .retain(|(d, g, _), _| d != &device || g == &generation);
        let init = {
            let gateway = self.clone();
            let device = device.clone();
            let generation = generation.clone();
            tokio::spawn(async move {
                let result = async {
                    gateway.call(&device,"initialize",json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"mr-li","version":"0.1.0"}})).await?;
                    let tools = gateway.call(&device,"tools/list",json!({})).await?;
                    if let Some(p) = gateway.peers.lock().unwrap().get_mut(&device) && p.generation == generation { p.tools = tools["tools"].clone(); }
                    gateway.notices.retry(&device)?;
                    Ok::<_,anyhow::Error>(())
                }.await;
                if let Err(e) = result {
                    tracing::warn!(error=%e,"获取平板工具失败，关闭连接等待平板重连");
                    let mut peers = gateway.peers.lock().unwrap();
                    if peers
                        .get(&device)
                        .is_some_and(|p| p.generation == generation)
                    {
                        peers.remove(&device);
                    }
                }
            })
        };
        let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
        let mut last_seen = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _=heartbeat.tick()=> {
                    let current = self.peers.lock().unwrap().get(&device).is_some_and(|p|p.generation==generation);
                    if !current || last_seen.elapsed()>Duration::from_secs(75) || !self.registry.authorize(&device,&token) { break; }
                    if ws.send(Message::Ping(Bytes::new())).await.is_err() { break; }
                }
                outgoing=rx.recv()=> {
                    let Some(value)=outgoing else { break; };
                    let current=self.peers.lock().unwrap().get(&device).is_some_and(|p|p.generation==generation);
                    if !current || !self.registry.authorize(&device,&token) { break; }
                    if ws.send(Message::Text(value.to_string().into())).await.is_err() { break; }
                }
                incoming=ws.next()=> {
                    let current=self.peers.lock().unwrap().get(&device).is_some_and(|p|p.generation==generation);
                    if !current || !self.registry.authorize(&device,&token) { break; }
                    last_seen=tokio::time::Instant::now();
                    match incoming {
                        Some(Ok(Message::Text(text))) => {
                            let Ok(value)=serde_json::from_str::<Value>(&text) else { continue; };
                            if value["type"]!="mcp" || value["session_id"]!=generation { continue; }
                            let payload=&value["payload"];
                            if let Some(id)=payload["id"].as_u64()
                                && let Some(reply)=self.pending.lock().unwrap().remove(&(device.clone(),generation.clone(),id)) { let _=reply.send(payload.clone()); }
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        _ => {}
                    }
                }
            }
        }
        init.abort();
        let mut peers = self.peers.lock().unwrap();
        if peers
            .get(&device)
            .is_some_and(|p| p.generation == generation)
        {
            peers.remove(&device);
        }
        drop(peers);
        self.pending
            .lock()
            .unwrap()
            .retain(|(d, g, _), _| d != &device || g != &generation);
    }
    fn save_image(&self, device: &str, bytes: &[u8]) -> Result<PathBuf> {
        ensure!(
            bytes.len() <= MAX_IMAGE && bytes.starts_with(&[0xff, 0xd8, 0xff]),
            "需要不超过 2 MB 的 JPEG"
        );
        let dir = self.cfg.data_dir.join("tablet/media").join(device);
        private_dir(&dir)?;
        let path = dir.join(format!("{}.jpg", uuid::Uuid::new_v4()));
        std::fs::write(&path, bytes)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(std::fs::canonicalize(path)?)
    }
    pub fn devices_status(&self) -> Result<Vec<Value>> {
        let mut devices = self.registry.devices()?;
        let peers = self.peers.lock().unwrap();
        for device in &mut devices {
            device["online"] = json!(peers.contains_key(device["id"].as_str().unwrap_or("")));
        }
        Ok(devices)
    }
    async fn rpc(&self, request: &Value) -> Result<Value> {
        match request["method"].as_str().unwrap_or("") {
            "initialize" => Ok(
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mr-li-tablet","version":"0.1.0"}}),
            ),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools":[
                {"name":"tablet_devices","description":"列出已配对平板及在线状态","inputSchema":{"type":"object","properties":{}}},
                {"name":"tablet_tools","description":"查询平板可调用工具及参数。调用前先查询。","inputSchema":{"type":"object","properties":{"device_id":{"type":"string"}}}},
                {"name":"tablet_call","description":"调用平板工具。先用 tablet_tools 查看名称和参数。拍照检查返回本机照片/截图路径，请读取图片回答用户；勿递归调用拍照。临时开放需要家长验证码时不可绕过。","inputSchema":{"type":"object","properties":{"device_id":{"type":"string"},"name":{"type":"string"},"arguments":{"type":"object"}},"required":["name"]}}
            ]})),
            "tools/call" => {
                let params = &request["params"];
                let args = &params["arguments"];
                if params["name"] == "tablet_devices" {
                    return Ok(text_content(&serde_json::to_string(
                        &self.devices_status()?,
                    )?));
                }
                let device = {
                    let peers = self.peers.lock().unwrap();
                    match args["device_id"].as_str().filter(|s| !s.is_empty()) {
                        Some(d) => d.to_owned(),
                        None if peers.len() == 1 => peers.keys().next().unwrap().clone(),
                        _ => bail!("请通过 tablet_devices 查询并指定 device_id"),
                    }
                };
                if params["name"] == "tablet_tools" {
                    let peers = self.peers.lock().unwrap();
                    let peer = peers.get(&device).context("平板未连接")?;
                    return Ok(text_content(&peer.tools.to_string()));
                }
                ensure!(params["name"] == "tablet_call", "未知工具");
                let name = args["name"].as_str().context("缺少平板工具名")?;
                // Streaming/audio are explicitly deferred; don't accept a successful start and discard frames.
                ensure!(
                    !matches!(
                        name,
                        "self.camera.start_stream"
                            | "self.camera.stop_stream"
                            | "self.audio_speaker.play_ogg_url"
                            | "self.audio_speaker.stop"
                            | "self.camera.record_video"
                    ),
                    "首版暂未接入连续画面、录像和音频播放"
                );
                {
                    let peers = self.peers.lock().unwrap();
                    let peer = peers.get(&device).context("平板未连接")?;
                    ensure!(
                        peer.tools
                            .as_array()
                            .is_some_and(|ts| ts.iter().any(|t| t["name"] == name)),
                        "平板未声明该工具，请稍后查询 tablet_tools"
                    );
                }
                let mut result=self.call(&device,"tools/call",json!({"name":name,"arguments":args.get("arguments").cloned().unwrap_or(json!({}))})).await?;
                if name == "self.camera.snapshot" {
                    let file = result["preview_url"]
                        .as_str()
                        .unwrap_or("")
                        .rsplit('/')
                        .next()
                        .unwrap_or("");
                    if valid_media_file(file) {
                        let path = std::fs::canonicalize(
                            self.cfg
                                .data_dir
                                .join("tablet/media")
                                .join(&device)
                                .join(file),
                        )?;
                        if let Some(content) = result["content"].as_array_mut() {
                            content.push(json!({"type":"text","text":format!("本机照片路径：{}。需要查看时使用文件读取工具。",path.display())}));
                        }
                    }
                }
                Ok(result)
            }
            _ => bail!("不支持的 MCP 方法"),
        }
    }
}
fn notice_delivered(result: &Value) -> bool {
    if result["isError"] == true {
        return false;
    }
    result["delivered"] == true
        || result["content"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["type"] == "text"
                    && item["text"]
                        .as_str()
                        .and_then(|text| serde_json::from_str::<Value>(text).ok())
                        .is_some_and(|value| value["delivered"] == true)
            })
        })
}
fn text_content(text: &str) -> Value {
    json!({"content":[{"type":"text","text":text}]})
}
fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
}
fn public_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "MR_LI_TABLET_PUBLIC_URL 需要 HTTP(S) 根地址"
    );
    Ok(url)
}
async fn pair(State(g): State<Arc<Gateway>>, Json(body): Json<Value>) -> ApiResult {
    let mut url = public_url(&g.cfg.tablet_public_url)
        .map_err(|e| error(StatusCode::SERVICE_UNAVAILABLE, e))?;
    let (id, name, token) = g
        .registry
        .claim(&body)
        .map_err(|e| error(StatusCode::UNAUTHORIZED, e))?;
    url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "地址错误"))?;
    url.set_path("/tablet/v1/ws");
    Ok(Json(
        json!({"service":"mr-li","protocol":1,"device":{"id":id,"name":name},"connection":{"url":url.as_str(),"token":token}}),
    ))
}
async fn upgrade(
    State(g): State<Arc<Gateway>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> std::result::Result<Response, ApiError> {
    let device = g.device(&headers)?;
    let token = bearer(&headers).to_owned();
    Ok(ws
        .max_message_size(128 * 1024)
        .max_frame_size(128 * 1024)
        .on_upgrade(move |ws| g.socket(device, token, ws)))
}
async fn snapshot(State(g): State<Arc<Gateway>>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let device = g.device(&headers)?;
    let path = g
        .save_image(&device, &body)
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(
        json!({"uploaded":true,"image_size":body.len(),"local_path":path,"preview_url":format!("/tablet/v1/media/{}",path.file_name().unwrap().to_string_lossy()),"resolution":headers.get("X-Teacher-Resolution").and_then(|v|v.to_str().ok()).unwrap_or("vga")}),
    ))
}
fn valid_media_file(file: &str) -> bool {
    file.strip_suffix(".jpg")
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        && !file.contains('/')
        && !file.contains('\\')
}
async fn media(
    State(g): State<Arc<Gateway>>,
    headers: HeaderMap,
    UrlPath(file): UrlPath<String>,
) -> std::result::Result<Response, ApiError> {
    let device = g.device(&headers)?;
    if !valid_media_file(&file) {
        return Err(error(StatusCode::NOT_FOUND, "图片不存在"));
    }
    let bytes = tokio::fs::read(g.cfg.data_dir.join("tablet/media").join(device).join(file))
        .await
        .map_err(|_| error(StatusCode::NOT_FOUND, "图片不存在"))?;
    Ok((
        [
            ("Content-Type", "image/jpeg"),
            ("Cache-Control", "no-store"),
        ],
        bytes,
    )
        .into_response())
}
async fn explain(
    State(g): State<Arc<Gateway>>,
    headers: HeaderMap,
    mut fields: Multipart,
) -> ApiResult {
    let device = g.device(&headers)?;
    let mut images = Vec::new();
    let mut question = String::new();
    let mut screenshot_error = String::new();
    while let Some(field) = fields
        .next_field()
        .await
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?
    {
        let name = field.name().unwrap_or("").to_owned();
        let bytes = field
            .bytes()
            .await
            .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
        match name.as_str() {
            "image" | "screenshot" => {
                if images.iter().any(|(n, _)| n == &name) {
                    return Err(error(StatusCode::BAD_REQUEST, "图片字段重复"));
                }
                images.push((name, bytes));
            }
            "question" | "screenshot_error" => {
                if bytes.len() > 16000 {
                    return Err(error(StatusCode::BAD_REQUEST, "文字过长"));
                }
                let text = String::from_utf8(bytes.to_vec())
                    .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
                if name == "question" {
                    question = text;
                } else {
                    screenshot_error = text;
                }
            }
            _ => return Err(error(StatusCode::BAD_REQUEST, "未知字段")),
        }
    }
    if question.trim().is_empty() || !images.iter().any(|(n, _)| n == "image") {
        return Err(error(StatusCode::BAD_REQUEST, "缺少照片或问题"));
    }
    for (_, bytes) in &images {
        if bytes.len() > MAX_IMAGE || !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "图片必须为不超过 2 MB 的 JPEG",
            ));
        }
    }
    let mut text = format!("平板拍照已完成。请读取以下本机图片，结合图片回答问题：{question}\n");
    let mut saved = Vec::new();
    for (name, bytes) in images {
        let path = g
            .save_image(&device, &bytes)
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        text.push_str(&format!("{name}: {}\n", path.display()));
        saved.push(json!({"name":name,"path":path}));
    }
    if !screenshot_error.is_empty() {
        text.push_str(&format!("屏幕截图未取得：{screenshot_error}\n"));
    }
    g.record_report_upload(&device, &question, &json!(saved), &screenshot_error)
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({"response":text})))
}
async fn local_rpc(
    State(g): State<Arc<Gateway>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> ApiResult {
    if !same_secret(bearer(&headers), &g.control) {
        return Err(error(StatusCode::UNAUTHORIZED, "需要本机控制凭证"));
    }
    let id = request["id"].clone();
    let response = match g.rpc(&request).await {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(e) if request["method"] == "tools/call" => {
            let mut result = text_content(&e.to_string());
            result["isError"] = json!(true);
            json!({"jsonrpc":"2.0","id":id,"result":result})
        }
        Err(e) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":e.to_string()}}),
    };
    Ok(Json(response))
}

fn local_address(listen: &str) -> Result<String> {
    let address: std::net::SocketAddr =
        listen.parse().context("MR_LI_TABLET_LISTEN 需要 IP:端口")?;
    let ip = if address.ip().is_unspecified() {
        if address.is_ipv4() {
            "127.0.0.1".into()
        } else {
            "[::1]".into()
        }
    } else {
        ensure!(
            address.ip().is_loopback(),
            "本机 MCP 请将监听地址设为 127.0.0.1 或 0.0.0.0"
        );
        match address.ip() {
            std::net::IpAddr::V4(v) => v.to_string(),
            std::net::IpAddr::V6(v) => format!("[{v}]"),
        }
    };
    Ok(format!("http://{ip}:{}/_tablet/rpc", address.port()))
}
fn mcp_config(cfg: &Config) -> Result<Value> {
    let executable = std::env::current_exe()?;
    Ok(
        json!({"type":"local","command":[executable,"tablet-mcp"],"environment":{
        "MR_LI_DATA_DIR":std::fs::canonicalize(&cfg.data_dir)?,
        "MR_LI_TABLET_LISTEN":cfg.tablet_listen,
    },"disabled":false,"timeout":{"startup":20000}}),
    )
}

pub async fn command(cfg: &Config, mode: &str) -> Result<()> {
    match mode {
        "tablet-serve" => {
            ensure!(
                !cfg.tablet_public_url.is_empty(),
                "请配置 MR_LI_TABLET_PUBLIC_URL"
            );
            Gateway::open(Arc::new(cfg.clone()))?.serve().await
        }
        "tablet-pair" => {
            let base = public_url(&cfg.tablet_public_url)?;
            let registry = Registry::open(&cfg.data_dir.join("tablet"))?;
            let code = registry.pairing()?;
            let payload =
                json!({"service":"mr-li","protocol":1,"pair_url":format!("{}tablet/v1/pair",base),"code":code}).to_string();
            let svg = qrcode::QrCode::new(payload.as_bytes())?
                .render::<qrcode::render::svg::Color>()
                .min_dimensions(320, 320)
                .build();
            let path = cfg.data_dir.join("tablet/pairing.svg");
            std::fs::write(&path, svg)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            println!(
                "二维码：{}\n5 分钟内在平板解除旧绑定后扫码，点“保存并连接”。\n也可粘贴：{payload}",
                std::fs::canonicalize(path)?.display()
            );
            Ok(())
        }
        "tablet-devices" => {
            let registry = Registry::open(&cfg.data_dir.join("tablet"))?;
            println!("{}", serde_json::to_string_pretty(&registry.devices()?)?);
            Ok(())
        }
        "tablet-revoke" => {
            let device = std::env::args()
                .nth(2)
                .context("用法：mr-li tablet-revoke <设备ID>")?;
            let registry = Registry::open(&cfg.data_dir.join("tablet"))?;
            ensure!(
                registry
                    .0
                    .lock()
                    .unwrap()
                    .execute("DELETE FROM devices WHERE id=?", [device])?
                    > 0,
                "未找到设备"
            );
            println!("已撤销设备凭证，连接会在下一次检查时关闭");
            Ok(())
        }
        "tablet-mcp-config" => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"mcp":{"servers":{"homework":mcp_config(cfg)?}}})
                )?
            );
            Ok(())
        }
        "tablet-mcp" => stdio().await,
        _ => bail!("未知平板命令"),
    }
}

/// Standalone stdio adapter reads only gateway settings, never Feishu credentials.
pub async fn stdio() -> Result<()> {
    let env_path = std::env::var("MR_LI_ENV").unwrap_or_else(|_| ".env".into());
    let values = if Path::new(&env_path).exists() {
        dotenvy::from_path_iter(env_path)?.collect::<std::result::Result<HashMap<_, _>, _>>()?
    } else {
        HashMap::new()
    };
    let setting = |key: &str, default: &str| {
        std::env::var(key)
            .ok()
            .or_else(|| values.get(key).cloned())
            .unwrap_or_else(|| default.into())
    };
    let data_dir = PathBuf::from(setting("MR_LI_DATA_DIR", ".data"));
    let listen = setting("MR_LI_TABLET_LISTEN", "127.0.0.1:8787");
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let key = std::fs::read_to_string(data_dir.join("tablet/control-token"))
        .context("请先启动启用平板网关的 mr-li")?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(170))
        .build()?;
    let url = local_address(&listen)?;
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    // Bounded newline-delimited JSON-RPC (MCP stdio). No status text on stdout.
    loop {
        let mut line = Vec::new();
        let count = tokio::io::AsyncReadExt::take(&mut input, 1024 * 1024 + 1)
            .read_until(b'\n', &mut line)
            .await?;
        if count == 0 {
            break;
        }
        ensure!(line.len() <= 1024 * 1024, "MCP 请求过大");
        let request: Value = serde_json::from_slice(&line)?;
        if request.get("id").is_none() {
            continue;
        }
        let reply=async {
                    Ok::<_,anyhow::Error>(client.post(&url).bearer_auth(&key).json(&request).send().await?.error_for_status()?.json::<Value>().await?)
                }.await.unwrap_or_else(|e|json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":e.to_string()}}));
        output.write_all(format!("{reply}\n").as_bytes()).await?;
        output.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::SinkExt;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{Message as ClientMessage, client::IntoClientRequest},
    };
    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("mr-li-tablet-{}", uuid::Uuid::new_v4())))
        }
        fn config(&self) -> Config {
            let mut c = Config::fixture();
            c.data_dir = self.0.clone();
            c.tablet_public_url = "https://tablet.example".into();
            c
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn claim_body(code: &str, device: &str) -> Value {
        json!({"code":code,"device_id":device,"device_name":"学习平板","device_kind":"android"})
    }
    #[tokio::test]
    async fn opencode_v23_registration_uses_experimental_put_and_array_status() -> Result<()> {
        use axum::extract::Query;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let sandbox = Sandbox::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let mut cfg = sandbox.config();
        cfg.opencode_url = format!("http://{}", listener.local_addr()?);
        let writes = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/api/mcp",
                get(
                    |State(writes): State<Arc<AtomicUsize>>,
                     Query(query): Query<HashMap<String, String>>| async move {
                        assert!(query.contains_key("location[directory]"));
                        let servers = if writes.load(Ordering::SeqCst) == 0 {
                            json!([])
                        } else {
                            json!([{"name":"homework","status":{"status":"connected"}}])
                        };
                        Json(json!({"data":servers}))
                    },
                ),
            )
            .route(
                "/api/experimental/mcp/homework",
                axum::routing::put(
                    |State(writes): State<Arc<AtomicUsize>>,
                     Query(query): Query<HashMap<String, String>>,
                     Json(body): Json<Value>| async move {
                        assert!(query.contains_key("location[directory]"));
                        assert!(body["config"]["command"].is_array());
                        assert_eq!(body["config"]["timeout"]["startup"], 20000);
                        assert_eq!(body["config"]["disabled"], false);
                        assert!(
                            Path::new(
                                body["config"]["environment"]["MR_LI_DATA_DIR"]
                                    .as_str()
                                    .unwrap()
                            )
                            .is_absolute()
                        );
                        writes.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(writes.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let cfg = Arc::new(cfg);
        let g = Gateway::open(cfg.clone())?;
        let oc = crate::opencode::OpenCode::new(cfg)?;
        g.register_mcp(&oc, true).await?;
        g.register_mcp(&oc, false).await?;
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        server.abort();
        Ok(())
    }
    #[test]
    fn pairing_is_single_use_expiring_and_rotates_persisted_credentials() -> Result<()> {
        let sandbox = Sandbox::new();
        let dir = sandbox.0.join("tablet");
        let registry = Registry::open(&dir)?;
        let code = registry.pairing()?;
        assert!(registry.claim(&claim_body(&code, "../bad")).is_err());
        let (_, _, token) = registry.claim(&claim_body(&code, "homework-tablet-test"))?;
        assert!(registry.claim(&claim_body(&code, "other")).is_err());
        assert!(registry.authorize("homework-tablet-test", &token));
        assert!(!registry.authorize("other", &token));
        let code = registry.pairing()?;
        let (_, _, new_token) = registry.claim(&claim_body(&code, "homework-tablet-test"))?;
        assert!(!registry.authorize("homework-tablet-test", &token));
        drop(registry);
        let registry = Registry::open(&dir)?;
        assert!(registry.authorize("homework-tablet-test", &new_token));
        let expired = registry.pairing()?;
        registry.0.lock().unwrap().execute(
            "UPDATE pairings SET expires=? WHERE code=?",
            params![now() - 1, expired],
        )?;
        assert!(registry.claim(&claim_body(&expired, "test")).is_err());
        assert_eq!(
            std::fs::metadata(dir.join("teacher-devices-v1.sqlite3"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        Ok(())
    }
    async fn test_server(
        sandbox: &Sandbox,
    ) -> Result<(Arc<Gateway>, String, tokio::task::JoinHandle<()>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let mut cfg = sandbox.config();
        cfg.tablet_public_url = base.clone();
        let g = Gateway::open(Arc::new(cfg))?;
        let app = g.routes();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Ok((g, base, task))
    }
    async fn http_pair(g: &Gateway, base: &str, device: &str) -> Result<String> {
        let code = g.registry.pairing()?;
        let r = reqwest::Client::new()
            .post(format!("{base}/tablet/v1/pair"))
            .json(&claim_body(&code, device))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        assert_eq!(
            r["connection"]["url"],
            format!("{}/tablet/v1/ws", base.replacen("http", "ws", 1))
        );
        assert_eq!(r["service"], "mr-li");
        assert_eq!(r["protocol"], 1);
        Ok(r["connection"]["token"].as_str().unwrap().into())
    }
    #[tokio::test]
    async fn homework_websocket_mcp_roundtrip_and_token_rotation() -> Result<()> {
        let sandbox = Sandbox::new();
        let (g, base, server) = test_server(&sandbox).await?;
        let token = http_pair(&g, &base, "homework-tablet-test").await?;
        let mut request =
            format!("{}/tablet/v1/ws", base.replacen("http", "ws", 1)).into_client_request()?;
        request
            .headers_mut()
            .insert("Device-Id", "homework-tablet-test".parse()?);
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse()?);
        let (mut ws, _) = connect_async(request).await?;
        ws.send(ClientMessage::Text(
            json!({"type":"hello","service":"mr-li","protocol":1})
                .to_string()
                .into(),
        ))
        .await?;
        let hello = ws.next().await.unwrap()?;
        let hello: Value = serde_json::from_str(hello.to_text()?)?;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["service"], "mr-li");
        assert_eq!(hello["protocol"], 1);
        let session = hello["session_id"].clone();
        let client = tokio::spawn(async move {
            while let Some(message) = ws.next().await {
                let message = message.unwrap();
                if let ClientMessage::Text(text) = message {
                    let value: Value = serde_json::from_str(&text).unwrap();
                    let p = &value["payload"];
                    let result = match p["method"].as_str().unwrap() {
                        "initialize" => {
                            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"homework-buddy-android","version":"0.1.0"}})
                        }
                        "tools/list" => {
                            json!({"tools":[{"name":"self.homework.get_status","inputSchema":{"type":"object"}}]})
                        }
                        "tools/call" => {
                            assert_eq!(p["params"]["name"], "self.homework.get_status");
                            text_content("今天有三项作业")
                        }
                        _ => panic!("unexpected RPC"),
                    };
                    ws.send(ClientMessage::Text(json!({"type":"mcp","session_id":session,"payload":{"jsonrpc":"2.0","id":p["id"],"result":result}}).to_string().into())).await.unwrap();
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ready = g
                    .peers
                    .lock()
                    .unwrap()
                    .get("homework-tablet-test")
                    .is_some_and(|p| p.tools.as_array().is_some_and(|t| !t.is_empty()));
                if ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let http = reqwest::Client::new();
        let rpc = json!({"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"tablet_call","arguments":{"name":"self.homework.get_status"}}});
        assert_eq!(
            http.post(format!("{base}/_tablet/rpc"))
                .json(&rpc)
                .send()
                .await?
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let result = http
            .post(format!("{base}/_tablet/rpc"))
            .bearer_auth(&g.control)
            .json(&rpc)
            .send()
            .await?
            .json::<Value>()
            .await?;
        assert_eq!(result["result"]["content"][0]["text"], "今天有三项作业");
        let unsupported = json!({"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"tablet_call","arguments":{"name":"self.camera.start_stream"}}});
        let failure = http
            .post(format!("{base}/_tablet/rpc"))
            .bearer_auth(&g.control)
            .json(&unsupported)
            .send()
            .await?
            .json::<Value>()
            .await?;
        assert_eq!(failure["result"]["isError"], true);
        assert!(
            failure["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("首版暂未接入")
        );
        assert!(g.pending.lock().unwrap().is_empty());
        let devices = g
            .rpc(&json!({"method":"tools/call","params":{"name":"tablet_devices"}}))
            .await?;
        assert!(
            devices["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"online\":true")
        );
        let new_token = http_pair(&g, &base, "homework-tablet-test").await?;
        assert!(!g.registry.authorize("homework-tablet-test", &token));
        assert!(g.registry.authorize("homework-tablet-test", &new_token));
        // A queued command cannot execute on a socket whose token was rotated.
        assert!(
            g.call(
                "homework-tablet-test",
                "tools/call",
                json!({"name":"self.homework.get_status"})
            )
            .await
            .is_err()
        );
        client.abort();
        server.abort();
        Ok(())
    }
    #[tokio::test]
    async fn notification_uses_existing_homework_tool_and_requires_delivery_confirmation()
    -> Result<()> {
        let sandbox = Sandbox::new();
        let g = Gateway::open(Arc::new(sandbox.config()))?;
        let id = g
            .notices
            .publish("owner", "notify-request", "tablet", "明天带练习册", now())?;
        g.deliver_notices().await?;
        assert!(g.notices.list("owner", now())?.contains("平板离线"));
        let (tx, mut rx) = mpsc::channel(8);
        g.peers.lock().unwrap().insert(
            "tablet".into(),
            Peer {
                generation: "notice-generation".into(),
                tx,
                tools: json!([{"name":"self.notify.send"}]),
            },
        );
        g.notices.retry("tablet")?;
        let gateway = g.clone();
        let delivery = tokio::spawn(async move { gateway.deliver_notices().await });
        let request = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await?
            .unwrap();
        assert_eq!(request["payload"]["params"]["name"], "self.notify.send");
        assert_eq!(
            request["payload"]["params"]["arguments"]["body"],
            "明天带练习册"
        );
        assert!(g.notices.cancel("owner", &id).is_err());
        let rpc_id = request["payload"]["id"].as_u64().unwrap();
        g.pending
            .lock()
            .unwrap()
            .remove(&("tablet".into(), "notice-generation".into(), rpc_id))
            .unwrap()
            .send(json!({"result":text_content("{\"delivered\":true}")}))
            .unwrap();
        delivery.await??;
        assert!(g.notices.list("owner", now())?.contains("已送达"));
        g.deliver_notices().await?;
        assert!(rx.try_recv().is_err());
        Ok(())
    }
    #[test]
    fn notification_ack_matches_current_app_wrapped_result() {
        assert!(notice_delivered(&text_content("{\"delivered\":true}")));
        assert!(!notice_delivered(&text_content("通知已收到")));
        assert!(!notice_delivered(&json!({"isError":true,"delivered":true})));
    }
    #[tokio::test]
    async fn report_capture_collects_two_images_once_and_survives_restart() -> Result<()> {
        let sandbox = Sandbox::new();
        let (g, base, server) = test_server(&sandbox).await?;
        let token = http_pair(&g, &base, "report-tablet").await?;
        let (tx, mut rx) = mpsc::channel(8);
        g.peers.lock().unwrap().insert(
            "report-tablet".into(),
            Peer {
                generation: "report-generation".into(),
                tx,
                tools: json!([{"name":"self.camera.take_photo"}]),
            },
        );
        assert_eq!(g.report_capture("report-one")?["status"], "pending");
        let request = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await?
            .unwrap();
        let question = request["payload"]["params"]["arguments"]["question"]
            .as_str()
            .unwrap();
        let jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0xff, 0xd9];
        let form = reqwest::multipart::Form::new()
            .text("question", question.to_owned())
            .part(
                "image",
                reqwest::multipart::Part::bytes(jpeg.clone())
                    .file_name("photo.jpg")
                    .mime_str("image/jpeg")?,
            )
            .part(
                "screenshot",
                reqwest::multipart::Part::bytes(jpeg)
                    .file_name("screenshot.jpg")
                    .mime_str("image/jpeg")?,
            );
        reqwest::Client::new()
            .post(format!("{base}/tablet/v1/photo-check"))
            .header("Device-Id", "report-tablet")
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await?
            .error_for_status()?;
        let capture = g.report_capture("report-one")?;
        assert_eq!(capture["status"], "ready");
        assert_eq!(capture["images"].as_array().unwrap().len(), 2);
        for image in capture["images"].as_array().unwrap() {
            g.validate_report_image("report-tablet", Path::new(image["path"].as_str().unwrap()))?;
        }
        let id = request["payload"]["id"].as_u64().unwrap();
        g.pending
            .lock()
            .unwrap()
            .remove(&("report-tablet".into(), "report-generation".into(), id))
            .unwrap()
            .send(json!({"result":text_content("已上传")}))
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(g.report_capture("report-one")?["images"], capture["images"]);
        assert!(rx.try_recv().is_err());
        let restarted = Gateway::open(g.cfg.clone())?;
        assert_eq!(
            restarted.report_capture("report-one")?["images"],
            capture["images"]
        );
        server.abort();
        Ok(())
    }
    #[tokio::test]
    async fn report_capture_offline_and_interrupted_requests_are_not_repeated() -> Result<()> {
        let sandbox = Sandbox::new();
        let g = Gateway::open(Arc::new(sandbox.config()))?;
        assert_eq!(g.report_capture("offline-report")?["status"], "failed");
        let path = g.capture_path("interrupted-report");
        g.save_capture(
            &path,
            &json!({"status":"pending","started":now(),"instance":"previous-process","images":[]}),
        )?;
        let restarted = Gateway::open(g.cfg.clone())?;
        let capture = restarted.report_capture("interrupted-report")?;
        assert_eq!(capture["status"], "failed");
        assert!(capture["error"].as_str().unwrap().contains("重启中断"));
        let (tx, mut rx) = mpsc::channel(8);
        restarted.peers.lock().unwrap().insert(
            "later-tablet".into(),
            Peer {
                generation: "later".into(),
                tx,
                tools: json!([{"name":"self.camera.take_photo"}]),
            },
        );
        assert_eq!(
            restarted.report_capture("offline-report")?["status"],
            "failed"
        );
        assert_eq!(
            restarted.report_capture("interrupted-report")?["status"],
            "failed"
        );
        assert!(rx.try_recv().is_err());
        Ok(())
    }
    #[tokio::test]
    async fn report_upload_is_bound_to_device_and_keeps_screenshot_failure() -> Result<()> {
        let sandbox = Sandbox::new();
        let g = Gateway::open(Arc::new(sandbox.config()))?;
        let path = g.capture_path("partial-report");
        let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"partial-report");
        let question = format!("mr-li-report:{id}\n采集");
        g.save_capture(&path,&json!({"status":"pending","started":now(),"instance":g.instance,"device":"tablet-a","question":question,"images":[]}))?;
        let images = json!([{"name":"image","path":"photo.jpg"}]);
        g.record_report_upload("tablet-b", &question, &images, "未授权")?;
        assert_eq!(g.report_capture("partial-report")?["status"], "pending");
        g.record_report_upload("tablet-a", &question, &images, "无障碍截图未授权")?;
        let capture = g.report_capture("partial-report")?;
        assert_eq!(capture["status"], "ready");
        assert_eq!(capture["images"], images);
        assert_eq!(capture["screenshot_error"], "无障碍截图未授权");
        g.record_report_upload("tablet-a", &question, &json!([]), "duplicate")?;
        assert_eq!(g.report_capture("partial-report")?["images"], images);
        Ok(())
    }
    #[tokio::test]
    async fn rejects_legacy_endpoints_and_hello() -> Result<()> {
        let sandbox = Sandbox::new();
        let (g, base, server) = test_server(&sandbox).await?;
        let http = reqwest::Client::new();
        for path in [
            "/xiaozhi/pair",
            "/xiaozhi/v1/",
            "/mcp/vision/snapshot",
            "/mcp/vision/explain",
        ] {
            let response = http
                .post(format!("{base}{path}"))
                .json(&json!({}))
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        let token = http_pair(&g, &base, "test-tablet").await?;
        let mut request =
            format!("{}/tablet/v1/ws", base.replacen("http", "ws", 1)).into_client_request()?;
        request
            .headers_mut()
            .insert("Device-Id", "test-tablet".parse()?);
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse()?);
        let (mut ws, _) = connect_async(request).await?;
        ws.send(ClientMessage::Text(
            json!({"type":"hello","version":1,"transport":"websocket"})
                .to_string()
                .into(),
        ))
        .await?;
        let response = tokio::time::timeout(Duration::from_secs(3), ws.next()).await?;
        assert!(!matches!(response, Some(Ok(ClientMessage::Text(_)))));
        assert!(g.peers.lock().unwrap().is_empty());
        server.abort();
        Ok(())
    }
    #[tokio::test]
    async fn uploads_require_token_and_are_private_to_device() -> Result<()> {
        let sandbox = Sandbox::new();
        let (g, base, server) = test_server(&sandbox).await?;
        let token = http_pair(&g, &base, "tablet-one").await?;
        let other = http_pair(&g, &base, "tablet-two").await?;
        let http = reqwest::Client::new();
        let jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0xff, 0xd9];
        assert_eq!(
            http.post(format!("{base}/tablet/v1/snapshot"))
                .body(jpeg.clone())
                .send()
                .await?
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let response = http
            .post(format!("{base}/tablet/v1/snapshot"))
            .header("Device-Id", "tablet-one")
            .bearer_auth(&token)
            .body(jpeg.clone())
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        let preview = response["preview_url"].as_str().unwrap();
        assert_eq!(
            http.get(format!("{base}{preview}"))
                .header("Device-Id", "tablet-two")
                .bearer_auth(&other)
                .send()
                .await?
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            http.get(format!("{base}{preview}"))
                .header("Device-Id", "tablet-one")
                .bearer_auth(&token)
                .send()
                .await?
                .bytes()
                .await?
                .as_ref(),
            jpeg.as_slice()
        );
        let form = reqwest::multipart::Form::new()
            .part(
                "image",
                reqwest::multipart::Part::bytes(jpeg.clone())
                    .file_name("photo.jpg")
                    .mime_str("image/jpeg")?,
            )
            .part(
                "screenshot",
                reqwest::multipart::Part::bytes(jpeg)
                    .file_name("screenshot.jpg")
                    .mime_str("image/jpeg")?,
            )
            .text("question", "作业完成了吗？");
        let response = http
            .post(format!("{base}/tablet/v1/photo-check"))
            .header("Device-Id", "tablet-one")
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        let response = response["response"].as_str().unwrap();
        assert!(
            response.contains("image:")
                && response.contains("screenshot:")
                && response.contains("作业完成了吗？")
        );
        assert_eq!(
            http.post(format!("{base}/tablet/v1/snapshot"))
                .header("Device-Id", "tablet-one")
                .bearer_auth(&token)
                .body(vec![1; MAX_IMAGE + 1])
                .send()
                .await?
                .status(),
            StatusCode::BAD_REQUEST
        );
        server.abort();
        Ok(())
    }
}
