//! OpenCode V2.3 API. The installed macOS compatibility launcher remains the runtime.
use crate::config::Config;
use anyhow::{Context, Result, ensure};
use reqwest::Method;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Debug)]
pub struct AbortUnconfirmed;
impl std::fmt::Display for AbortUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("任务中止尚未确认，等待服务恢复后再处理新任务")
    }
}
impl std::error::Error for AbortUnconfirmed {}
pub struct OpenCode {
    cfg: Arc<Config>,
    http: reqwest::Client,
}
impl OpenCode {
    pub fn new(cfg: Arc<Config>) -> Result<Self> {
        Ok(Self {
            cfg,
            http: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(5))
                .build()?,
        })
    }
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let mut r = self
            .http
            .request(method, format!("{}{}", self.cfg.opencode_url, path))
            .basic_auth(&self.cfg.username, Some(&self.cfg.password))
            .timeout(timeout);
        if matches!(
            path,
            "/api/model"
                | "/api/permission/request"
                | "/api/form"
                | "/api/config"
                | "/api/mcp"
                | "/api/skill"
                | "/api/experimental/mcp/homework"
                | "/api/experimental/mcp/teacher"
        ) {
            r = r.query(&[(
                "location[directory]",
                self.cfg.workspace.to_string_lossy().as_ref(),
            )]);
        }
        if let Some(body) = body {
            r = r.json(&body);
        }
        let mut response = r
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("OpenCode V2 请求失败或超时"))?;
        ensure!(
            response.status().is_success(),
            "OpenCode V2 HTTP {}",
            response.status()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.context("OpenCode V2 响应读取失败")? {
            ensure!(
                bytes.len() + chunk.len() <= 32 * 1024 * 1024,
                "OpenCode V2 响应过大"
            );
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).context("OpenCode V2 响应格式不兼容")
    }
    pub async fn get(&self, path: &str) -> Result<Value> {
        let v = self
            .request(Method::GET, path, None, Duration::from_secs(20))
            .await?;
        Ok(v.get("data").cloned().unwrap_or(v))
    }
    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let v = self
            .request(Method::POST, path, Some(body), Duration::from_secs(20))
            .await?;
        Ok(v.get("data").cloned().unwrap_or(v))
    }
    pub async fn delete(&self, path: &str) -> Result<()> {
        self.request(Method::DELETE, path, None, Duration::from_secs(15))
            .await?;
        Ok(())
    }
    /// Explicit user policy for this unattended bridge: approve owned action requests.
    /// Do not approve other sessions or fabricate answers to forms.
    pub async fn approve_pending(&self, sid: &str) -> Result<usize> {
        let pending = self.get(&format!("/api/session/{sid}/permission")).await?;
        let mut count = 0;
        for request in pending.as_array().context("OpenCode 权限请求格式不兼容")? {
            if request["sessionID"] != sid {
                continue;
            }
            let id = request["id"].as_str().context("权限请求缺少 ID")?;
            ensure!(
                !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "权限请求 ID 无效"
            );
            // The installed 2.3.0 API uses `decision`, verified against its payload validator.
            self.post(
                &format!("/api/session/{sid}/permission/{id}/reply"),
                json!({"decision":"once"}),
            )
            .await?;
            count += 1;
        }
        Ok(count)
    }
    pub async fn health(&self) -> Result<String> {
        let v = self.get("/api/info").await?;
        let version = v["version"].as_str().context("OpenCode V2 未返回版本")?;
        ensure!(
            version.starts_with("2.3."),
            "需要 OpenCode V2.3.x，当前为 {version}；请使用已适配系统的 V2 启动器"
        );
        Ok(version.into())
    }
    pub async fn catalog(&self) -> Result<Value> {
        normalize_catalog(&self.get("/api/model").await?)
    }
    pub async fn create(&self, selected: Option<&str>) -> Result<String> {
        let mut body = json!({"title":"李老师 · 飞书","agent":self.cfg.agent,"location":{"directory":self.cfg.workspace}});
        if let Some(model) = selected.or(self.cfg.model.as_deref()) {
            body["model"] = model_ref(model)?;
        }
        let v = self.post("/api/session", body).await?;
        Ok(v["id"]
            .as_str()
            .context("OpenCode V2 未返回会话 ID")?
            .into())
    }
    pub async fn abort(&self, id: &str) -> Result<()> {
        self.post(&format!("/api/session/{id}/interrupt"), json!({}))
            .await?;
        Ok(())
    }
    async fn abort_checked(&self, id: &str) -> Result<()> {
        self.abort(id).await.map_err(|_| AbortUnconfirmed.into())
    }
    pub async fn prompt_with_files(
        &self,
        id: &str,
        text: &str,
        selected: Option<&str>,
        files: &[Value],
    ) -> Result<String> {
        self.prompt_with_progress(id, text, selected, files, None)
            .await
    }
    pub async fn prompt_with_progress(
        &self,
        id: &str,
        text: &str,
        selected: Option<&str>,
        files: &[Value],
        mut progress: Option<&mut crate::agent::ProgressObserver<'_>>,
    ) -> Result<String> {
        let selected = selected
            .or(self.cfg.model.as_deref())
            .context("请选择模型")?;
        if self.cfg.free_only {
            ensure_free_model(&self.catalog().await?, selected)?;
        }
        self.post(
            &format!("/api/session/{id}/model"),
            json!({"model":model_ref(selected)?}),
        )
        .await?;
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let admitted = self
            .post(&format!("/api/session/{id}/prompt"), {
                let mut body = json!({"id":message_id,"text":text});
                if !files.is_empty() {
                    body["files"] = json!(files);
                }
                body
            })
            .await;
        let admitted = match admitted {
            Ok(v) => v,
            Err(e) => {
                self.abort_checked(id).await?;
                return Err(e);
            }
        };
        let admitted_at = admitted["time"]["created"].as_f64().unwrap_or(f64::MAX);
        let deadline = tokio::time::Instant::now() + self.cfg.task_timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                self.abort_checked(id).await?;
                anyhow::bail!("任务超时，已请求停止");
            }
            let messages = self
                .get(&format!("/api/session/{id}/message?order=desc&limit=200"))
                .await;
            let messages = match messages {
                Ok(v) => v,
                Err(e) => {
                    self.abort_checked(id).await?;
                    return Err(e);
                }
            };
            if let Some(result) = round_result(&messages, &message_id, admitted_at) {
                if result.is_err() {
                    self.abort_checked(id).await?;
                }
                return result;
            }
            if let Some(observer) = progress.as_deref_mut()
                && let Some(newer) = round_messages(&messages, &message_id, admitted_at)
            {
                for message in newer.iter().rev().filter(|m| m["type"] == "assistant") {
                    observer(&assistant_text(message));
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
fn model_ref(selected: &str) -> Result<Value> {
    let (provider, id) = selected
        .split_once('/')
        .context("模型格式应为 provider/model")?;
    ensure!(!provider.is_empty() && !id.is_empty(), "模型格式无效");
    Ok(json!({"providerID":provider,"id":id}))
}
fn normalize_catalog(models: &Value) -> Result<Value> {
    let mut providers = serde_json::Map::new();
    for m in models.as_array().context("OpenCode V2 模型目录格式无效")? {
        let Some(provider) = m["providerID"].as_str() else {
            continue;
        };
        let Some(id) = m["id"].as_str() else { continue };
        if m["enabled"] == false {
            continue;
        }
        let free = m["cost"].as_array().is_some_and(|tiers| {
            !tiers.is_empty()
                && tiers.iter().all(|tier| {
                    tier["input"].as_f64() == Some(0.0)
                        && tier["output"].as_f64() == Some(0.0)
                        && tier["cache"]["read"].as_f64() == Some(0.0)
                        && tier["cache"]["write"].as_f64() == Some(0.0)
                })
        });
        let entry = providers
            .entry(provider.to_owned())
            .or_insert_with(|| json!({"id":provider,"models":{}}));
        entry["models"][id] = json!({"id":id,"name":m["name"],"status":m["status"],"cost":{"input":if free{0}else{1},"output":if free{0}else{1}}});
    }
    Ok(json!({"all":providers.into_values().collect::<Vec<_>>()}))
}
fn ensure_free_model(catalog: &Value, selected: &str) -> Result<()> {
    let (provider, id) = selected
        .split_once('/')
        .context("模型格式应为 provider/model")?;
    ensure!(
        provider == "opencode",
        "当前仅允许 OpenCode 官方免费模型，不会切换付费账号"
    );
    let model = catalog["all"]
        .as_array()
        .and_then(|ps| ps.iter().find(|p| p["id"] == provider))
        .and_then(|p| p["models"].get(id))
        .context("指定免费模型已不可用，请重新选择")?;
    ensure!(
        model["status"] != "deprecated"
            && model["cost"]["input"].as_f64() == Some(0.0)
            && model["cost"]["output"].as_f64() == Some(0.0),
        "模型未被标记为免费，已停止调用"
    );
    Ok(())
}
fn upstream_error(error: &Value) -> anyhow::Error {
    if error["status"] == 429 || error["type"] == "provider.free-limit" {
        anyhow::anyhow!(
            "OpenCode 上游拒绝或限流了本次免费请求（HTTP 429）；这不等于账号额度耗尽。已停止本次任务，未切换付费模型"
        )
    } else {
        anyhow::anyhow!(
            "OpenCode 执行失败：{}",
            error["message"]
                .as_str()
                .unwrap_or("请查看运行日志")
                .chars()
                .take(500)
                .collect::<String>()
        )
    }
}
fn round_messages<'a>(
    messages: &'a Value,
    message_id: &str,
    admitted_at: f64,
) -> Option<&'a [Value]> {
    let all = messages.as_array()?;
    if let Some(index) = all.iter().position(|m| m["id"] == message_id) {
        Some(&all[..index])
    } else if all.len() >= 200
        && all
            .iter()
            .all(|m| m["time"]["created"].as_f64().unwrap_or(0.0) >= admitted_at)
    {
        Some(all.as_slice())
    } else {
        None
    }
}
fn assistant_text(message: &Value) -> String {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
fn round_result(messages: &Value, message_id: &str, admitted_at: f64) -> Option<Result<String>> {
    let newer = round_messages(messages, message_id, admitted_at)?;
    if let Some(idle) = newer.iter().find(|m| m["type"] == "idle") {
        return Some(match idle["outcome"].as_str() {
            Some("succeeded") => Ok(newer
                .iter()
                .filter(|m| m["type"] == "assistant")
                .map(assistant_text)
                .find(|text| !text.is_empty())
                .unwrap_or_else(|| "任务结束，没有文本输出。".into())),
            Some("interrupted") => Ok("任务已停止。".into()),
            _ => Err(upstream_error(&idle["error"])),
        });
    }
    for m in newer.iter().filter(|m| m["type"] == "assistant") {
        let error = if !m["retry"]["error"].is_null() {
            &m["retry"]["error"]
        } else {
            &m["error"]
        };
        if error["status"] == 429 || error["type"] == "provider.free-limit" {
            return Some(Err(upstream_error(error)));
        }
    }
    None
}

/// Preserve the previous /answer [[...]] syntax while supporting V2 typed forms.
pub fn form_answers(form: &Value, input: Value) -> Result<Value> {
    if input.is_object() {
        return Ok(input);
    }
    let answers: Vec<Vec<String>> =
        serde_json::from_value(input).context("答案需为字段对象或二维字符串数组")?;
    let fields = form["fields"]
        .as_array()
        .context("OpenCode 表单缺少字段")?
        .iter()
        .filter(|f| f["hidden"] != true)
        .collect::<Vec<_>>();
    ensure!(
        answers.len() == fields.len(),
        "请按问题顺序提供 {} 组答案",
        fields.len()
    );
    let mut out = serde_json::Map::new();
    for (field, answer) in fields.into_iter().zip(answers) {
        let key = field["key"].as_str().context("表单字段缺少标识")?;
        let first = answer.first().map(String::as_str).unwrap_or("");
        let value = match field["type"].as_str() {
            Some("multiselect") => json!(answer),
            Some("boolean") => json!(
                first
                    .parse::<bool>()
                    .context("布尔答案请使用 true 或 false")?
            ),
            Some("integer") => json!(first.parse::<i64>().context("请输入整数")?),
            Some("number") => json!(first.parse::<f64>().context("请输入数字")?),
            _ => json!(first),
        };
        out.insert(key.into(), value);
    }
    Ok(Value::Object(out))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn v2_request_uses_selected_model_and_prompt_contract() -> Result<()> {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let server = std::thread::spawn(move || {
            let mut message = String::new();
            let mut saw_progress = false;
            loop {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0u8; 4096];
                let end = loop {
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(i) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let header = String::from_utf8_lossy(&bytes[..end]);
                let route = header.lines().next().unwrap().to_string();
                let len = header
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < end + len {
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                }
                let body: Value = if len == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[end..end + len]).unwrap()
                };
                let reading = route.starts_with("GET /api/session/ses_test/message?");
                let done = reading && saw_progress;
                let response = if route.starts_with("GET /api/model?") {
                    json!({"data":[{"providerID":"opencode","id":"chosen","cost":[{"input":0,"output":0,"cache":{"read":0,"write":0}}]}]})
                } else if route.starts_with("POST /api/session/ses_test/model ") {
                    assert_eq!(
                        body,
                        json!({"model":{"providerID":"opencode","id":"chosen"}})
                    );
                    Value::Null
                } else if route.starts_with("POST /api/session/ses_test/prompt ") {
                    assert_eq!(body["text"], "hello");
                    assert_eq!(body["files"][0]["uri"], "data:image/png;base64,dGVzdA==");
                    assert_eq!(body["files"][0]["mime"], "image/png");
                    assert!(body.get("parts").is_none());
                    message = body["id"].as_str().unwrap().to_string();
                    json!({"data":{"id":message,"time":{"created":1}}})
                } else if done {
                    json!({"data":[{"type":"idle","outcome":"succeeded"},{"type":"assistant","content":[{"type":"text","text":"done"}]},{"type":"user","id":message}]})
                } else if reading {
                    saw_progress = true;
                    json!({"data":[{"type":"assistant","content":[{"type":"reasoning","text":"private"},{"type":"text","text":"<voice-progress>正在查找。</voice-progress>"}]},{"type":"user","id":message},{"type":"assistant","content":[{"type":"text","text":"previous turn"}]}]})
                } else {
                    panic!("Unexpected V2 route: {route}")
                };
                let text = if response.is_null() {
                    String::new()
                } else {
                    response.to_string()
                };
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",text.len(),text).unwrap();
                if done {
                    break;
                }
            }
        });
        let mut cfg = Config::fixture();
        cfg.opencode_url = format!("http://{addr}");
        let oc = OpenCode::new(Arc::new(cfg))?;
        let mut observed = Vec::new();
        let mut observer = |text: &str| observed.push(text.to_owned());
        assert_eq!(
            oc.prompt_with_progress(
                "ses_test",
                "hello",
                Some("opencode/chosen"),
                &[json!({"uri":"data:image/png;base64,dGVzdA==","mime":"image/png"})],
                Some(&mut observer),
            )
            .await?,
            "done"
        );
        assert_eq!(observed, ["<voice-progress>正在查找。</voice-progress>"]);
        server.join().unwrap();
        Ok(())
    }
    #[tokio::test]
    async fn auto_approval_only_replies_to_owned_permissions_with_v23_payload() -> Result<()> {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || {
            for index in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut data = Vec::new();
                let mut buf = [0u8; 2048];
                let end = loop {
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buf[..n]);
                    if let Some(i) = data.windows(4).position(|b| b == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&data[..end]);
                let len = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|n| n.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let route = headers.lines().next().unwrap().to_string();
                while data.len() < end + len {
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buf[..n]);
                }
                let body = if index == 0 {
                    assert!(route.starts_with("GET /api/session/ses_owned/permission "));
                    json!({"data":[{"sessionID":"ses_foreign","id":"per_foreign"},{"sessionID":"ses_owned","id":"per_owned"}]}).to_string()
                } else {
                    assert!(
                        route
                            .starts_with("POST /api/session/ses_owned/permission/per_owned/reply ")
                    );
                    assert_eq!(
                        serde_json::from_slice::<Value>(&data[end..end + len]).unwrap(),
                        json!({"decision":"once"})
                    );
                    String::new()
                };
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
        });
        let mut cfg = Config::fixture();
        cfg.opencode_url = format!("http://{address}");
        assert_eq!(
            OpenCode::new(Arc::new(cfg))?
                .approve_pending("ses_owned")
                .await?,
            1
        );
        server.join().unwrap();
        Ok(())
    }
    #[test]
    fn free_catalog_checks_every_tier_and_cache_price() {
        let v = json!([
            {"providerID":"opencode","id":"free","cost":[{"input":0,"output":0,"cache":{"read":0,"write":0}}]},
            {"providerID":"opencode","id":"paid-cache","cost":[{"input":0,"output":0,"cache":{"read":1,"write":0}}]},
            {"providerID":"opencode","id":"unknown","cost":[]}]);
        let c = normalize_catalog(&v).unwrap();
        assert!(ensure_free_model(&c, "opencode/free").is_ok());
        for id in [
            "opencode/paid-cache",
            "opencode/unknown",
            "opencode/missing",
            "other/free",
        ] {
            assert!(ensure_free_model(&c, id).is_err());
        }
    }
    #[test]
    fn round_parser_uses_current_prompt_and_never_reasoning() {
        let v = json!([
            {"type":"idle","outcome":"succeeded"},
            {"type":"assistant","content":[{"type":"reasoning","text":"private"},{"type":"text","text":"answer"}]},
            {"id":"msg_new","type":"user"},
            {"type":"idle","outcome":"failed","error":{"message":"old error"}}]);
        assert_eq!(round_result(&v, "msg_new", 0.0).unwrap().unwrap(), "answer");
        assert!(round_result(&v, "msg_missing", 0.0).is_none());
    }
    #[test]
    fn rate_limit_is_not_reported_as_exhausted_credits() {
        let v = json!([{"type":"assistant","retry":{"error":{"type":"provider.free-limit","status":429}}},{"type":"user","id":"msg_new"}]);
        let text = round_result(&v, "msg_new", 0.0)
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(text.contains("不等于账号额度耗尽"));
    }
    #[test]
    fn typed_form_mapping() {
        let f = json!({"fields":[{"key":"text","type":"string"},{"key":"many","type":"multiselect"},{"key":"ok","type":"boolean"}]});
        assert_eq!(
            form_answers(&f, json!([["hello"], ["A", "B"], ["true"]])).unwrap(),
            json!({"text":"hello","many":["A","B"],"ok":true})
        );
        assert!(form_answers(&f, json!([["one"]])).is_err());
    }
}
