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
        // IDs originate from Feishu, but constrain path segments defensively.
        ensure!(
            message.starts_with("om_")
                && message
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "无效消息 ID"
        );
        let v: Value = self
            .http
            .post(format!(
                "{}/open-apis/im/v1/messages/{message}/reply",self.cfg.feishu_base_url
            ))
            .bearer_auth(self.token().await?)
            .json(
                &json!({"msg_type":msg_type,"content":content.to_string(),"uuid":uuid,"reply_in_thread":thread}),
            )
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
            Ok(()) => ("info", "已收到，正在切换模型"),
            Err(_) => ("error", "卡片已失效或无操作权限，请重新打开模型选择"),
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
