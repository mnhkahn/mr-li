//! Wire fields and heartbeat/ACK behavior follow larksuite's official Python/Go SDKs.
use crate::{
    config::Config,
    store::{Job, Store},
};
use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, PartialEq, prost::Message)]
pub struct Header {
    #[prost(string, required, tag = "1")]
    pub key: String,
    #[prost(string, required, tag = "2")]
    pub value: String,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct Frame {
    #[prost(uint64, required, tag = "1")]
    pub seq_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub log_id: u64,
    #[prost(int32, required, tag = "3")]
    pub service: i32,
    #[prost(int32, required, tag = "4")]
    pub method: i32,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<Header>,
    #[prost(string, optional, tag = "6")]
    pub payload_encoding: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub payload_type: Option<String>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub payload: Option<Vec<u8>>,
    #[prost(string, optional, tag = "9")]
    pub log_id_new: Option<String>,
}
impl Frame {
    fn header(&self, key: &str) -> &str {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
            .unwrap_or("")
    }
    fn ping(service: i32) -> Self {
        Self {
            service,
            headers: vec![Header {
                key: "type".into(),
                value: "ping".into(),
            }],
            ..Default::default()
        }
    }
}
struct Fragment {
    at: Instant,
    parts: Vec<Option<Vec<u8>>>,
}
#[derive(Default)]
struct Fragments(HashMap<String, Fragment>);
impl Fragments {
    fn push(&mut self, f: &Frame) -> Result<Option<Vec<u8>>> {
        self.0
            .retain(|_, v| v.at.elapsed() < Duration::from_secs(10));
        let total = f.header("sum").parse::<usize>().unwrap_or(1);
        let seq = f.header("seq").parse::<usize>().unwrap_or(0);
        ensure!((1..=256).contains(&total) && seq < total, "无效分片序号");
        let payload = f.payload.clone().unwrap_or_default();
        ensure!(payload.len() <= 1024 * 1024, "飞书帧超限");
        if total == 1 {
            return Ok(Some(payload));
        }
        let id = f.header("message_id");
        ensure!(!id.is_empty(), "分片缺少消息 ID");
        ensure!(
            self.0.len() < 128 || self.0.contains_key(id),
            "分片缓存已满"
        );
        let item = self.0.entry(id.into()).or_insert_with(|| Fragment {
            at: Instant::now(),
            parts: vec![None; total],
        });
        ensure!(item.parts.len() == total, "分片数量不一致");
        item.parts[seq] = Some(payload);
        ensure!(
            item.parts.iter().flatten().map(Vec::len).sum::<usize>() <= 4 * 1024 * 1024,
            "消息超限"
        );
        if item.parts.iter().all(Option::is_some) {
            let item = self.0.remove(id).unwrap();
            Ok(Some(item.parts.into_iter().flatten().flatten().collect()))
        } else {
            Ok(None)
        }
    }
}

pub struct Feishu {
    cfg: Arc<Config>,
    http: reqwest::Client,
    token: Mutex<(String, Instant)>,
    pub bot_id: Mutex<String>,
}
impl Feishu {
    pub fn new(cfg: Arc<Config>) -> Result<Self> {
        Ok(Self {
            cfg,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()?,
            token: Mutex::new((String::new(), Instant::now())),
            bot_id: Mutex::new(String::new()),
        })
    }
    async fn token(&self) -> Result<String> {
        let mut token = self.token.lock().await;
        if token.0.is_empty() || Instant::now() >= token.1 {
            let r: Value = self
                .http
                .post(format!(
                    "{}/open-apis/auth/v3/tenant_access_token/internal",
                    self.cfg.feishu_base_url
                ))
                .json(&json!({"app_id":self.cfg.app_id,"app_secret":self.cfg.app_secret}))
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("飞书 token 网络请求失败"))?
                .error_for_status()
                .map_err(|r| anyhow::anyhow!("飞书 token HTTP {:?}", r.status()))?
                .json()
                .await?;
            ensure!(
                r["code"].as_i64() == Some(0),
                "飞书 token 错误码 {:?}",
                r["code"]
            );
            token.0 = r["tenant_access_token"]
                .as_str()
                .context("token 响应缺失")?
                .into();
            token.1 = Instant::now()
                + Duration::from_secs(r["expire"].as_u64().unwrap_or(7200).saturating_sub(120));
        }
        Ok(token.0.clone())
    }
    pub async fn identify(&self) -> Result<()> {
        let v: Value = self
            .http
            .get(format!(
                "{}/open-apis/bot/v3/info",
                self.cfg.feishu_base_url
            ))
            .bearer_auth(self.token().await?)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("飞书机器人信息请求失败"))?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            v["code"].as_i64() == Some(0),
            "飞书机器人错误码 {:?}",
            v["code"]
        );
        *self.bot_id.lock().await = v["bot"]["open_id"]
            .as_str()
            .context("缺少 bot open_id")?
            .into();
        Ok(())
    }
    pub async fn reply(&self, message: &str, text: &str, uuid: &str, thread: bool) -> Result<()> {
        self.reply_message(message, "text", json!({"text":text}), uuid, thread)
            .await?;
        Ok(())
    }
    pub async fn add_processing_reaction(&self, message: &str) -> Result<String> {
        // Same processing emojis as xiaoli/server/internal/admin/adapters.go.
        let emojis = ["Typing", "OnIt", "THINKING", "OneSecond"];
        let emoji = emojis[rand::random_range(0..emojis.len())];
        let v = self
            .reaction_request(reqwest::Method::POST, message, None, Some(emoji))
            .await?;
        Ok(v["data"]["reaction_id"]
            .as_str()
            .context("飞书未返回表情标识")?
            .into())
    }
    pub async fn remove_processing_reaction(&self, message: &str, reaction: &str) -> Result<()> {
        self.reaction_request(reqwest::Method::DELETE, message, Some(reaction), None)
            .await?;
        Ok(())
    }
    async fn reaction_request(
        &self,
        method: reqwest::Method,
        message: &str,
        reaction: Option<&str>,
        emoji: Option<&str>,
    ) -> Result<Value> {
        ensure!(
            message.starts_with("om_")
                && message
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "无效消息 ID"
        );
        let mut url = reqwest::Url::parse(&format!(
            "{}/open-apis/im/v1/messages/{message}/reactions",
            self.cfg.feishu_base_url
        ))?;
        if let Some(id) = reaction {
            ensure!(!id.is_empty() && id.len() <= 512, "无效表情标识");
            url.path_segments_mut()
                .map_err(|_| anyhow::anyhow!("表情地址无效"))?
                .push(id);
        }
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(self.token().await?)
            .timeout(Duration::from_secs(5));
        if let Some(emoji) = emoji {
            request = request.json(&json!({"reaction_type":{"emoji_type":emoji}}));
        }
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("飞书表情网络请求失败"))?;
        let status = response.status();
        ensure!(status.is_success(), "飞书表情 HTTP {}", status.as_u16());
        let v: Value = response.json().await?;
        let code = v["code"].as_i64();
        if matches!(code, Some(99991663 | 99991668)) {
            self.token.lock().await.0.clear();
        }
        ensure!(
            code == Some(0),
            "飞书表情错误码 {:?}，请检查 im:message.reactions:write_only 权限",
            code
        );
        Ok(v)
    }
    /// Stream local camera media to Feishu rather than buffering a video in memory.
    pub async fn upload_camera_media(
        &self,
        path: &std::path::Path,
        video: bool,
        duration_ms: u64,
    ) -> Result<String> {
        let size = tokio::fs::metadata(path).await?.len();
        let limit = if video {
            30 * 1024 * 1024
        } else {
            10 * 1024 * 1024
        };
        ensure!(
            size > 0 && size < limit,
            "摄像头文件为空或超过飞书上传大小限制"
        );
        let (endpoint, field, key, mime, name) = if video {
            ("files", "file", "file_key", "video/mp4", "camera.mp4")
        } else if path.extension().and_then(|s| s.to_str()) == Some("png") {
            ("images", "image", "image_key", "image/png", "camera.png")
        } else {
            ("images", "image", "image_key", "image/jpeg", "camera.jpg")
        };
        let part = reqwest::multipart::Part::file(path)
            .await?
            .file_name(name)
            .mime_str(mime)?;
        let mut form = reqwest::multipart::Form::new().part(field, part);
        if video {
            form = form
                .text("file_type", "mp4")
                .text("file_name", name)
                .text("duration", duration_ms.to_string());
        } else {
            form = form.text("image_type", "message");
        }
        let response = self
            .http
            .post(format!(
                "{}/open-apis/im/v1/{endpoint}",
                self.cfg.feishu_base_url
            ))
            .bearer_auth(self.token().await?)
            .multipart(form)
            .timeout(Duration::from_secs(90))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("飞书媒体上传网络请求失败"))?;
        let status = response.status();
        ensure!(status.is_success(), "飞书媒体上传 HTTP {}", status.as_u16());
        let v: Value = response.json().await?;
        let code = v["code"].as_i64();
        if matches!(code, Some(99991663 | 99991668)) {
            self.token.lock().await.0.clear();
        }
        ensure!(
            code == Some(0),
            "飞书媒体上传错误码 {:?}（请确认应用已开通 im:resource 上传权限）",
            code
        );
        Ok(v["data"][key]
            .as_str()
            .context("飞书上传未返回媒体标识")?
            .to_owned())
    }
    pub async fn reply_camera_media(
        &self,
        message: &str,
        payload: &Value,
        uuid: &str,
        thread: bool,
    ) -> Result<()> {
        let kind = payload["msg_type"].as_str().context("媒体类型缺失")?;
        ensure!(matches!(kind, "image" | "media" | "post"), "未知媒体类型");
        self.reply_message(message, kind, payload["content"].clone(), uuid, thread)
            .await?;
        Ok(())
    }
    pub async fn reply_card(
        &self,
        message: &str,
        card: &Value,
        uuid: &str,
        thread: bool,
    ) -> Result<String> {
        let v = self
            .reply_message(message, "interactive", card.clone(), uuid, thread)
            .await?;
        Ok(v["data"]["message_id"]
            .as_str()
            .context("卡片发送成功但未返回消息 ID")?
            .into())
    }
    pub async fn update_card(&self, message: &str, card: &Value) -> Result<()> {
        ensure!(
            message.starts_with("om_")
                && message
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "无效消息 ID"
        );
        let v: Value = self
            .http
            .patch(format!(
                "{}/open-apis/im/v1/messages/{message}",
                self.cfg.feishu_base_url
            ))
            .bearer_auth(self.token().await?)
            .json(&json!({"content":card.to_string()}))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("卡片更新请求失败"))?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            v["code"].as_i64() == Some(0),
            "卡片更新错误码 {:?}",
            v["code"]
        );
        Ok(())
    }
    async fn reply_message(
        &self,
        message: &str,
        msg_type: &str,
        content: Value,
        uuid: &str,
        thread: bool,
    ) -> Result<Value> {
        // A chat ID creates a top-level message; a message ID retains the private reply path.
        let group = message.starts_with("oc_");
        ensure!(
            (group || message.starts_with("om_"))
                && message
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "无效消息或群 ID"
        );
        let base = format!("{}/open-apis/im/v1/messages", self.cfg.feishu_base_url);
        let mut payload = json!({"msg_type":msg_type,"content":content.to_string(),"uuid":uuid});
        let request = if group {
            payload["receive_id"] = json!(message);
            self.http
                .post(base)
                .query(&[("receive_id_type", "chat_id")])
        } else {
            payload["reply_in_thread"] = json!(thread);
            self.http.post(format!("{base}/{message}/reply"))
        };
        let v: Value = request
            .bearer_auth(self.token().await?)
            .json(&payload)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("飞书回复网络请求失败"))?
            .error_for_status()?
            .json()
            .await?;
        let code = v["code"].as_i64();
        if matches!(code, Some(99991663 | 99991668)) {
            self.token.lock().await.0.clear();
        }
        ensure!(code == Some(0), "飞书回复错误码 {:?}", code);
        Ok(v)
    }
    pub async fn run(&self, store: Arc<Store>) -> Result<()> {
        self.run_mode(store, None).await
    }
    pub async fn observe(&self, chat: &str, challenge: &str) -> Result<()> {
        tracing::info!("身份识别模式：仅匹配指定会话和确认短语，不执行任务、不自动修改白名单");
        self.run_mode(
            Arc::new(Store::open(std::path::Path::new(":memory:"))?),
            Some((chat, challenge)),
        )
        .await
    }
    async fn run_mode(&self, store: Arc<Store>, observe: Option<(&str, &str)>) -> Result<()> {
        let mut retry = 1u64;
        loop {
            let started = Instant::now();
            if let Err(e) = self.connection(&store, observe).await {
                tracing::warn!(error=%e,"飞书连接断开，准备重连");
            }
            if started.elapsed() > Duration::from_secs(60) {
                retry = 1;
            }
            tokio::time::sleep(
                Duration::from_secs(retry) + Duration::from_millis(rand::random_range(0..1000)),
            )
            .await;
            retry = (retry * 2).min(60);
        }
    }
    async fn connection(&self, store: &Store, observe: Option<(&str, &str)>) -> Result<()> {
        let r: Value = self
            .http
            .post(format!("{}/callback/ws/endpoint", self.cfg.feishu_base_url))
            .json(&json!({"AppID":self.cfg.app_id,"AppSecret":self.cfg.app_secret}))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("飞书长连接发现失败"))?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            r["code"].as_i64() == Some(0),
            "飞书长连接发现错误码 {:?}",
            r["code"]
        );
        let url = r["data"]["URL"].as_str().context("飞书未返回长连接地址")?;
        let parsed = reqwest::Url::parse(url).context("长连接地址无效")?;
        ensure!(parsed.scheme() == "wss", "飞书长连接必须使用 WSS");
        let service: i32 = parsed
            .query_pairs()
            .find(|(k, _)| k == "service_id")
            .context("缺少 service_id")?
            .1
            .parse()?;
        // Never include the signed WebSocket URL in errors/logs.
        let (mut ws, _) = tokio::time::timeout(
            Duration::from_secs(20),
            tokio_tungstenite::connect_async(url),
        )
        .await
        .context("长连接握手超时")?
        .map_err(|_| anyhow::anyhow!("长连接握手失败"))?;
        tracing::info!("飞书 WebSocket 已连接");
        let mut ping_secs = r["data"]["ClientConfig"]["PingInterval"]
            .as_u64()
            .unwrap_or(120)
            .clamp(5, 300);
        let mut last_rx = Instant::now();
        let mut next_ping = Instant::now();
        let mut fragments = Fragments::default();
        loop {
            tokio::select! {
                msg=ws.next()=> {
                    let msg=msg.context("飞书已关闭连接")?.map_err(|_| anyhow::anyhow!("飞书长连接读取失败"))?;
                    last_rx=Instant::now();
                    match msg {
                        Message::Binary(b)=> {
                            let mut f=Frame::decode(b).context("飞书帧解码失败")?;
                            if f.method==0 {
                                if f.header("type")=="pong"
                                    && let Ok(v)=serde_json::from_slice::<Value>(f.payload.as_deref().unwrap_or_default()) {
                                        ping_secs=v["PingInterval"].as_u64().unwrap_or(ping_secs).clamp(5,300);
                                    }
                                continue;
                            }
                            if f.method!=1 || !matches!(f.header("type"),"event"|"card") { continue; }
                            let Some(payload)=fragments.push(&f)? else { continue; };
                            let start=Instant::now();
                            // Only local persistence happens before ACK; HTTP work runs on the worker.
                            let mut callback=false;
                            let result=serde_json::from_slice::<Value>(&payload).context("事件 JSON 无效").and_then(|v| {
                                callback=v["header"]["event_type"]=="card.action.trigger";
                                if let Some((chat,challenge))=observe {
                                    if let Some(id)=observed_sender(&v,chat,challenge){tracing::info!(open_id=id,"已匹配身份确认消息；没有执行任务");}
                                }else if callback {
                                    let job=parse_model_action(&v,&self.cfg,store)?.context("卡片已失效，或你没有操作权限。请重新打开模型选择")?;
                                    store.insert(&job)?;
                                }else if let Some(job)=parse_job(&v,&self.cfg,&self.bot_id.try_lock().map(|s|s.clone()).unwrap_or_default())? {
                                    store.insert(&job)?;
                                }
                                Ok(())
                            });
                            let ack=event_response(&result,callback);
                            if let Err(e)=result {tracing::warn!(error=%e,"消息或卡片操作处理失败");}
                            f.payload=Some(ack.to_string().into_bytes());
                            f.headers.push(Header{key:"biz_rt".into(),value:start.elapsed().as_millis().to_string()});
                            ws.send(Message::Binary(f.encode_to_vec().into())).await.map_err(|_|anyhow::anyhow!("飞书 ACK 发送失败"))?;
                        },
                        Message::Ping(p)=>ws.send(Message::Pong(p)).await.map_err(|_|anyhow::anyhow!("WebSocket pong 失败"))?,
                        Message::Close(_)=>bail!("飞书关闭连接"), _=>{}
                    }
                }
                _=tokio::time::sleep_until(tokio::time::Instant::from_std(next_ping))=> {
                    ensure!(last_rx.elapsed()<Duration::from_secs(ping_secs*2+30),"飞书心跳超时");
                    ws.send(Message::Binary(Frame::ping(service).encode_to_vec().into())).await
                        .map_err(|_|anyhow::anyhow!("飞书心跳发送失败"))?;
                    next_ping=Instant::now()+Duration::from_secs(ping_secs);
                }
            }
        }
    }
}
fn event_response(result: &Result<()>, callback: bool) -> Value {
    if callback {
        let (kind, text) = match result {
            Ok(()) => ("info", "已收到，正在处理"),
            Err(_) => ("error", "卡片已失效或无操作权限"),
        };
        let response = json!({"toast":{"type":kind,"content":text}}).to_string();
        json!({"code":200,"data":base64::engine::general_purpose::STANDARD.encode(response.as_bytes())})
    } else {
        json!({"code":if result.is_ok(){200}else{500}})
    }
}
fn parse_model_action(v: &Value, cfg: &Config, store: &Store) -> Result<Option<Job>> {
    if v["header"]["event_type"] != "card.action.trigger" {
        return Ok(None);
    }
    let e = &v["event"];
    let owner = e["operator"]["open_id"].as_str().unwrap_or("");
    let chat = e["context"]["open_chat_id"].as_str().unwrap_or("");
    let message = e["context"]["open_message_id"].as_str().unwrap_or("");
    if !cfg.allowed.iter().any(|id| id == owner)
        || (!cfg.allowed_chats.is_empty() && !cfg.allowed_chats.iter().any(|id| id == chat))
    {
        return Ok(None);
    }
    let a = &e["action"];
    if a["value"]["action"] == "opencode_form" {
        let token = a["value"]["token"].as_str().context("卡片缺少标识")?;
        let Some(card) = store.form_card(token)? else {
            return Ok(None);
        };
        let key: Value = serde_json::from_str(&card.origin_key)?;
        if key[0] != cfg.workspace.to_string_lossy().as_ref()
            || key[1] != owner
            || key[2] != chat
            || card.message_id != message
        {
            return Ok(None);
        }
        let event_id = v["header"]["event_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("回调缺少事件 ID")?;
        let payload = json!({"token":token,"revision":a["value"]["revision"],"operation":a["value"]["operation"],"input":a["form_value"]});
        ensure!(payload.to_string().len() <= 64 * 1024, "表单答案过长");
        return Ok(Some(Job {
            id: format!("card:{event_id}"),
            key: card.origin_key,
            message_id: message.into(),
            text: format!("/_form {}", payload),
            result: String::new(),
            sent: 0,
        }));
    }
    if a["tag"] != "select_static" || a["value"]["action"] != "select_model" {
        return Ok(None);
    }
    let token = a["value"]["token"].as_str().context("卡片缺少标识")?;
    let Some(card) = store.model_card(token)? else {
        return Ok(None);
    };
    let key: Value = serde_json::from_str(&card.origin_key)?;
    if key[0] != cfg.workspace.to_string_lossy().as_ref()
        || key[1] != owner
        || key[2] != chat
        || card.message_id != message
    {
        return Ok(None);
    }
    let model = a["option"].as_str().context("请选择模型")?;
    ensure!(
        model.len() <= 200
            && model.starts_with("opencode/")
            && model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)),
        "模型选项无效"
    );
    let event_id = v["header"]["event_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("回调缺少事件 ID")?;
    Ok(Some(Job {
        id: format!("card:{event_id}"),
        key: card.origin_key,
        message_id: message.into(),
        text: format!("/_model-select {token} {model}"),
        result: String::new(),
        sent: 0,
    }))
}
fn observed_sender<'a>(v: &'a Value, chat: &str, challenge: &str) -> Option<&'a str> {
    if v["header"]["event_type"] != "im.message.receive_v1"
        || v["event"]["sender"]["sender_type"] != "user"
    {
        return None;
    }
    let message = &v["event"]["message"];
    if message["message_type"] != "text" {
        return None;
    }
    if chat == "p2p" {
        if message["chat_type"] != "p2p" {
            return None;
        }
    } else if message["chat_id"] != chat {
        return None;
    }
    let content: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
    if !content["text"].as_str()?.contains(challenge) {
        return None;
    }
    v["event"]["sender"]["sender_id"]["open_id"]
        .as_str()
        .filter(|id| id.starts_with("ou_"))
}
pub fn parse_job(v: &Value, c: &Config, bot: &str) -> Result<Option<Job>> {
    if v["header"]["event_type"] != "im.message.receive_v1" {
        return Ok(None);
    }
    let e = &v["event"];
    let m = &e["message"];
    if !c.allowed_chats.is_empty()
        && !m["chat_id"]
            .as_str()
            .is_some_and(|id| c.allowed_chats.iter().any(|allowed| allowed == id))
    {
        return Ok(None);
    }
    let sender = e["sender"]["sender_id"]["open_id"].as_str().unwrap_or("");
    if e["sender"]["sender_type"] != "user" || !c.allowed.iter().any(|s| s == sender) {
        return Ok(None);
    }
    if m["message_type"] != "text" {
        return Ok(None);
    }
    let group = m["chat_type"] == "group";
    let mentions = m["mentions"].as_array();
    if group && !mentions.is_some_and(|ms| ms.iter().any(|m| m["id"]["open_id"] == bot)) {
        return Ok(None);
    }
    let content: Value = serde_json::from_str(m["content"].as_str().unwrap_or("{}"))?;
    let mut text = content["text"].as_str().unwrap_or("").to_string();
    if let Some(ms) = mentions {
        for m in ms {
            if m["id"]["open_id"] == bot
                && let Some(k) = m["key"].as_str()
            {
                text = text.replace(k, "");
            }
        }
    }
    let text = match text.trim() {
        "切换模型" | "选择模型" | "模型列表" => "/models",
        other => other,
    }
    .to_string();
    if text.is_empty() {
        return Ok(None);
    }
    ensure!(text.len() <= 64 * 1024, "文本过长");
    let id = m["message_id"]
        .as_str()
        .context("缺少 message_id")?
        .to_string();
    let chat = m["chat_id"].as_str().context("缺少 chat_id")?;
    let root = if group {
        m["root_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(&id)
    } else {
        ""
    };
    // JSON tuple avoids ambiguous separators; workspace separates remapped deployments.
    let key = json!([c.workspace, sender, chat, root]).to_string();
    Ok(Some(Job {
        id: id.clone(),
        message_id: id,
        key,
        text,
        result: String::new(),
        sent: 0,
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn group_messages_create_top_level_text_images_videos_and_cards() -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            for kind in ["text", "image", "media", "interactive"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let end = loop {
                    let mut chunk = [0u8; 2048];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|s| s == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let header = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                assert!(
                    header.starts_with("post /open-apis/im/v1/messages?receive_id_type=chat_id ")
                );
                let size = header
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .map(|n| n.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                while buf.len() < end + size {
                    let mut chunk = [0u8; 2048];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body: Value = serde_json::from_slice(&buf[end..end + size]).unwrap();
                assert_eq!(body["receive_id"], "oc_group");
                assert_eq!(body["msg_type"], kind);
                assert!(body.get("reply_in_thread").is_none());
                assert!(body.get("root_id").is_none());
                assert_eq!(body["uuid"], format!("group-{kind}"));
                assert!(
                    serde_json::from_str::<Value>(body["content"].as_str().unwrap())
                        .unwrap()
                        .is_object()
                );
                let out = json!({"code":0,"data":{"message_id":"om_sent_group"}}).to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",out.len(),out).as_bytes()).await.unwrap();
            }
        });
        let mut cfg = Config::fixture();
        cfg.feishu_base_url = format!("http://{address}");
        let feishu = Feishu::new(Arc::new(cfg))?;
        *feishu.token.lock().await = (
            "group-token".into(),
            Instant::now() + Duration::from_secs(60),
        );
        feishu
            .reply("oc_group", "hello", "group-text", false)
            .await?;
        for kind in ["image", "media"] {
            feishu
                .reply_camera_media(
                    "oc_group",
                    &json!({"msg_type":kind,"content":{"image_key":"img_test"}}),
                    &format!("group-{kind}"),
                    false,
                )
                .await?;
        }
        assert_eq!(
            feishu
                .reply_card(
                    "oc_group",
                    &json!({"elements":[]}),
                    "group-interactive",
                    false
                )
                .await?,
            "om_sent_group"
        );
        server.await?;
        Ok(())
    }
    #[tokio::test]
    async fn processing_reaction_contract_and_escaped_removal() -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            for i in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let (header_end, length) = loop {
                    let mut bytes = [0; 4096];
                    let n = socket.read(&mut bytes).await.unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&bytes[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let h = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                        let length = h
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .map(|l| l.trim().parse::<usize>().unwrap())
                            .unwrap_or(0);
                        break (p + 4, length);
                    }
                };
                while buf.len() < header_end + length {
                    let mut bytes = [0; 4096];
                    let n = socket.read(&mut bytes).await.unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&bytes[..n]);
                }
                let header = String::from_utf8_lossy(&buf[..header_end]);
                assert!(
                    header
                        .to_ascii_lowercase()
                        .contains("authorization: bearer test-reaction-token")
                );
                if i == 0 {
                    assert!(
                        header.starts_with("POST /open-apis/im/v1/messages/om_task/reactions ")
                    );
                    let body: Value = serde_json::from_slice(&buf[header_end..]).unwrap();
                    assert!(
                        ["Typing", "OnIt", "THINKING", "OneSecond"]
                            .contains(&body["reaction_type"]["emoji_type"].as_str().unwrap())
                    );
                } else {
                    assert!(header.starts_with(
                        "DELETE /open-apis/im/v1/messages/om_task/reactions/id%2Fwith%3Fsymbols "
                    ));
                }
                let body = json!({"code":0,"data":{"reaction_id":"id/with?symbols"}}).to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
            }
        });
        let mut cfg = Config::fixture();
        cfg.feishu_base_url = format!("http://{address}");
        let f = Feishu::new(Arc::new(cfg))?;
        *f.token.lock().await = (
            "test-reaction-token".into(),
            Instant::now() + Duration::from_secs(60),
        );
        let id = f.add_processing_reaction("om_task").await?;
        f.remove_processing_reaction("om_task", &id).await?;
        assert!(f.add_processing_reaction("bad/path").await.is_err());
        server.await?;
        Ok(())
    }
    #[tokio::test]
    async fn camera_upload_and_video_reply_use_native_media_contract() -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            for i in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let header_end = loop {
                    let mut bytes = [0; 4096];
                    let n = socket.read(&mut bytes).await.unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&bytes[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let header = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
                assert!(header.contains("authorization: bearer test-camera-token"));
                let length: usize = header
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                while buf.len() < header_end + length {
                    let mut bytes = [0; 4096];
                    let n = socket.read(&mut bytes).await.unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&bytes[..n]);
                }
                let body = String::from_utf8_lossy(&buf[header_end..]);
                let response = match i {
                    0 => {
                        assert!(header.starts_with("post /open-apis/im/v1/images "));
                        assert!(body.contains("name=\"image_type\"\r\n\r\nmessage"));
                        assert!(body.contains("name=\"image\"; filename=\"camera.jpg\""));
                        json!({"code":0,"data":{"image_key":"img_cover"}})
                    }
                    1 => {
                        assert!(header.starts_with("post /open-apis/im/v1/files "));
                        assert!(body.contains("name=\"file_type\"\r\n\r\nmp4"));
                        assert!(body.contains("name=\"duration\"\r\n\r\n10000"));
                        json!({"code":0,"data":{"file_key":"file_video"}})
                    }
                    2 => {
                        assert!(
                            header.starts_with("post /open-apis/im/v1/messages/om_camera/reply ")
                        );
                        let v: Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(v["msg_type"], "media");
                        assert_eq!(v["uuid"], "camera-idempotency");
                        assert_eq!(v["reply_in_thread"], true);
                        let content: Value =
                            serde_json::from_str(v["content"].as_str().unwrap()).unwrap();
                        assert_eq!(
                            content,
                            json!({"file_key":"file_video","image_key":"img_cover"})
                        );
                        json!({"code":0})
                    }
                    _ => {
                        assert!(header.starts_with(
                            "post /open-apis/im/v1/messages?receive_id_type=chat_id "
                        ));
                        let v: Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(v["msg_type"], "post");
                        assert_eq!(v["receive_id"], "oc_report");
                        assert_eq!(v["uuid"], "report-idempotency");
                        let content: Value =
                            serde_json::from_str(v["content"].as_str().unwrap()).unwrap();
                        assert_eq!(
                            content,
                            json!({"zh_cn":{"content":[
                                [{"tag":"text","text":"坐姿汇总"}],
                                [{"tag":"text","text":"20:44 · 未触发提醒"}],
                                [{"tag":"img","image_key":"img_cover"}]
                            ]}})
                        );
                        assert!(v.get("reply_in_thread").is_none());
                        json!({"code":0})
                    }
                }
                .to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).as_bytes()).await.unwrap();
            }
        });
        let mut cfg = Config::fixture();
        cfg.feishu_base_url = format!("http://{address}");
        let feishu = Feishu::new(Arc::new(cfg))?;
        *feishu.token.lock().await = (
            "test-camera-token".into(),
            Instant::now() + Duration::from_secs(60),
        );
        let path = std::env::temp_dir().join(format!("mr-li-upload-{}", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, b"camera media test fixture").await?;
        let result = async {
            let image = feishu.upload_camera_media(&path, false, 0).await?;
            let video = feishu.upload_camera_media(&path, true, 10000).await?;
            feishu
                .reply_camera_media(
                    "om_camera",
                    &json!({"msg_type":"media","content":{"image_key":image,"file_key":video}}),
                    "camera-idempotency",
                    true,
                )
                .await?;
            feishu
                .reply_camera_media(
                    "oc_report",
                    &crate::bridge::image_message(&image, Some("坐姿汇总\n20:44 · 未触发提醒")),
                    "report-idempotency",
                    false,
                )
                .await
        }
        .await;
        tokio::fs::remove_file(&path).await?;
        result?;
        server.await?;
        Ok(())
    }
    #[test]
    fn form_callbacks_bind_owner_and_message_and_keep_typed_input() -> Result<()> {
        let cfg = Config::fixture();
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let key = json!([cfg.workspace, "ou_owner", "oc_chat", "root"]).to_string();
        store.save_form_card("token", &key, "ses_owned", &json!({"revision":0}))?;
        store.attach_form_card("token", "om_form")?;
        let mut v = json!({"header":{"event_type":"card.action.trigger","event_id":"form-event"},"event":{"operator":{"open_id":"ou_owner"},"context":{"open_chat_id":"oc_chat","open_message_id":"om_form"},"action":{"tag":"button","value":{"action":"opencode_form","token":"token","revision":0,"operation":"next"},"form_value":{"answer":["one","two"]}}}});
        let job = parse_model_action(&v, &cfg, &store)?.unwrap();
        assert_eq!(job.key, key);
        assert!(store.insert(&job)?);
        assert!(!store.insert(&job)?);
        let p: Value = serde_json::from_str(job.text.strip_prefix("/_form ").unwrap())?;
        assert_eq!(p["input"]["answer"], json!(["one", "two"]));
        v["event"]["context"]["open_message_id"] = json!("om_forwarded");
        assert!(parse_model_action(&v, &cfg, &store)?.is_none());
        v["event"]["context"]["open_message_id"] = json!("om_form");
        v["event"]["operator"]["open_id"] = json!("ou_intruder");
        assert!(parse_model_action(&v, &cfg, &store)?.is_none());
        Ok(())
    }
    #[test]
    fn card_callbacks_bind_owner_chat_message_and_are_deduplicated() -> Result<()> {
        let mut cfg = Config::fixture();
        cfg.allowed.push("ou_other".into());
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let key = json!([cfg.workspace, "ou_owner", "oc_chat", "root"]).to_string();
        store.register_model_card("nonce", &key)?;
        store.attach_model_card("nonce", "om_card")?;
        let mut v = json!({"header":{"event_type":"card.action.trigger","event_id":"evt1"},"event":{
            "operator":{"open_id":"ou_owner"},"context":{"open_chat_id":"oc_chat","open_message_id":"om_card"},
            "action":{"tag":"select_static","option":"opencode/free","value":{"action":"select_model","token":"nonce"}}}});
        let job = parse_model_action(&v, &cfg, &store)?.unwrap();
        assert_eq!(job.key, key);
        assert!(store.insert(&job)?);
        assert!(!store.insert(&job)?);
        for (field, value) in [
            ("open_chat_id", "oc_forwarded"),
            ("open_message_id", "om_copied"),
        ] {
            let old = v["event"]["context"][field].clone();
            v["event"]["context"][field] = json!(value);
            assert!(parse_model_action(&v, &cfg, &store)?.is_none());
            v["event"]["context"][field] = old;
        }
        v["event"]["operator"]["open_id"] = json!("ou_other");
        assert!(parse_model_action(&v, &cfg, &store)?.is_none());
        let ack = event_response(&Ok(()), true);
        let bytes =
            base64::engine::general_purpose::STANDARD.decode(ack["data"].as_str().unwrap())?;
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes)?["toast"]["type"],
            "info"
        );
        assert_eq!(
            event_response(&Err(anyhow::anyhow!("error")), false)["code"],
            500
        );
        Ok(())
    }
    #[test]
    fn enrollment_requires_chat_and_challenge() {
        let v = json!({"header":{"event_type":"im.message.receive_v1"},"event":{
            "sender":{"sender_type":"user","sender_id":{"open_id":"ou_owner"}},
            "message":{"message_type":"text","chat_type":"group","chat_id":"oc_expected",
                "content":json!({"text":"初始化 challenge-1234"}).to_string()}}});
        assert_eq!(
            observed_sender(&v, "oc_expected", "challenge-1234"),
            Some("ou_owner")
        );
        assert_eq!(observed_sender(&v, "oc_other", "challenge-1234"), None);
        assert_eq!(observed_sender(&v, "oc_expected", "wrong-challenge"), None);
        assert_eq!(observed_sender(&v, "p2p", "challenge-1234"), None);
        let mut cfg = Config::fixture();
        cfg.allowed_chats = vec!["oc_other".into()];
        assert!(parse_job(&v, &cfg, "ou_bot").unwrap().is_none());
    }
    #[test]
    fn sender_mention_and_thread_isolation() -> Result<()> {
        let cfg = Config::fixture();
        let mut v = json!({"header":{"event_type":"im.message.receive_v1"},"event":{
            "sender":{"sender_type":"user","sender_id":{"open_id":"ou_owner"}},
            "message":{"message_type":"text","chat_type":"group","message_id":"om_root","chat_id":"oc_chat",
                "content":json!({"text":"@_user_1 hello"}).to_string(),
                "mentions":[{"key":"@_user_1","id":{"open_id":"ou_bot"}}]}}});
        let root = parse_job(&v, &cfg, "ou_bot")?.unwrap();
        assert_eq!(root.text, "hello");
        v["event"]["message"]["message_id"] = json!("om_reply");
        v["event"]["message"]["root_id"] = json!("om_root");
        assert_eq!(parse_job(&v, &cfg, "ou_bot")?.unwrap().key, root.key);
        v["event"]["sender"]["sender_id"]["open_id"] = json!("ou_intruder");
        assert!(parse_job(&v, &cfg, "ou_bot")?.is_none());
        v["event"]["sender"]["sender_id"]["open_id"] = json!("ou_owner");
        v["event"]["message"]["mentions"] = json!([]);
        assert!(parse_job(&v, &cfg, "ou_bot")?.is_none());
        v["event"]["message"]["chat_type"] = json!("p2p");
        let private = parse_job(&v, &cfg, "ou_bot")?.unwrap();
        v["event"]["message"]["root_id"] = json!("om_another");
        assert_eq!(parse_job(&v, &cfg, "ou_bot")?.unwrap().key, private.key);
        Ok(())
    }
    #[test]
    fn wire_roundtrip_and_out_of_order_fragments() -> Result<()> {
        let f = Frame::ping(123);
        assert_eq!(
            f.encode_to_vec(),
            vec![
                8, 0, 16, 0, 24, 123, 32, 0, 42, 12, 10, 4, b't', b'y', b'p', b'e', 18, 4, b'p',
                b'i', b'n', b'g'
            ]
        );
        assert_eq!(Frame::decode(f.encode_to_vec().as_slice())?, f);
        let mut fragments = Fragments::default();
        let make = |seq: &str, part: &str| Frame {
            method: 1,
            headers: vec![
                Header {
                    key: "sum".into(),
                    value: "2".into(),
                },
                Header {
                    key: "seq".into(),
                    value: seq.into(),
                },
                Header {
                    key: "message_id".into(),
                    value: "m".into(),
                },
            ],
            payload: Some(part.as_bytes().to_vec()),
            ..Default::default()
        };
        assert!(fragments.push(&make("1", "world"))?.is_none());
        assert_eq!(
            fragments.push(&make("0", "hello "))?.unwrap(),
            b"hello world"
        );
        assert!(fragments.push(&make("2", "bad")).is_err());
        Ok(())
    }
}
