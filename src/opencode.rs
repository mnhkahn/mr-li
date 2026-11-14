use crate::config::Config;
use anyhow::{Context, Result, ensure};
use reqwest::Method;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Debug)]
pub struct AbortUnconfirmed;
impl std::fmt::Display for AbortUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("任务失败且中止未确认，暂停处理并等待服务恢复")
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
            .query(&[("directory", self.cfg.workspace.to_string_lossy().as_ref())])
            .basic_auth(&self.cfg.username, Some(&self.cfg.password))
            .timeout(timeout);
        if let Some(body) = body {
            r = r.json(&body);
        }
        let mut r = r
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("OpenCode 请求失败或超时"))?;
        let status = r.status();
        ensure!(status.is_success(), "OpenCode HTTP {status}");
        let mut b = Vec::new();
        while let Some(chunk) = r.chunk().await.context("OpenCode 响应读取失败")? {
            ensure!(
                b.len() + chunk.len() <= 8 * 1024 * 1024,
                "OpenCode 响应超过 8 MiB"
            );
            b.extend_from_slice(&chunk);
        }
        if b.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&b).context("OpenCode 响应格式不兼容")
    }
    pub async fn get(&self, path: &str) -> Result<Value> {
        self.request(Method::GET, path, None, Duration::from_secs(10))
            .await
    }
    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.request(Method::POST, path, Some(body), Duration::from_secs(15))
            .await
    }
    pub async fn health(&self) -> Result<String> {
        let v = self.get("/global/health").await?;
        ensure!(v["healthy"] == true, "OpenCode 未就绪");
        let version = v["version"].as_str().context("OpenCode 未返回版本")?;
        ensure!(
            version == "1.19.0",
            "需要 OpenCode 1.19.0，当前为 {version}；请按部署说明安装固定版本"
        );
        Ok(version.into())
    }
    pub async fn create(&self) -> Result<String> {
        let v = self
            .post("/session", json!({"title":"李老师 · 飞书"}))
            .await?;
        Ok(v["id"]
            .as_str()
            .context("OpenCode 未返回 session id")?
            .into())
    }
    pub async fn abort(&self, id: &str) -> Result<()> {
        self.post(&format!("/session/{id}/abort"), json!({}))
            .await?;
        Ok(())
    }
    pub async fn prompt(&self, id: &str, text: &str) -> Result<String> {
        let mut body = json!({"agent":self.cfg.agent,"system":"你是李老师，通过飞书与用户交流。使用中文，回答简洁。需要用户授权或补充信息时使用权限和提问机制。", "parts":[{"type":"text","text":text}]});
        if let Some(m) = &self.cfg.model
            && let Some((provider, model)) = m.split_once('/')
        {
            body["model"] = json!({"providerID":provider,"modelID":model});
        }
        let result = self
            .request(
                Method::POST,
                &format!("/session/{id}/message"),
                Some(body),
                self.cfg.task_timeout,
            )
            .await;
        match result {
            Ok(v) => extract_answer(&v),
            Err(e) => {
                if self.abort(id).await.is_err() {
                    return Err(AbortUnconfirmed.into());
                }
                Err(e)
            }
        }
    }
}
pub fn extract_answer(v: &Value) -> Result<String> {
    if !v["info"]["error"].is_null() {
        anyhow::bail!("OpenCode 执行失败（请查看 OpenCode 日志）")
    }
    let text = v["parts"]
        .as_array()
        .context("OpenCode 未返回消息 parts")?
        .iter()
        .filter(|p| p["type"] == "text" && p["ignored"] != true)
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(if text.is_empty() {
        "任务结束，没有文本输出。可继续追问，或用 /status 查看会话。".into()
    } else {
        text
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn answer_excludes_reasoning_and_tools() {
        assert_eq!(extract_answer(&json!({"info":{},"parts":[{"type":"reasoning","text":"private"},{"type":"tool","text":"tool"},{"type":"text","text":"完成"}]})).unwrap(),"完成");
        assert!(extract_answer(&json!({"info":{"error":{"name":"error"}},"parts":[]})).is_err());
    }
}
