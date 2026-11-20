//! Manage the local Mihomo controller without routing controller calls through a proxy.
use anyhow::{Context, Result, bail, ensure};
use futures_util::{StreamExt, stream};
use reqwest::{Client, Method, Url};
use serde_json::{Value, json};
use std::time::Duration;

pub const HELP: &str = "/proxy [status] 查看当前节点\n/proxy list [筛选] 查看节点\n/proxy set <节点名或唯一关键字> 切换节点\n/proxy test [国家/地区或 all] 测试当前节点，或批量测速排名\n/proxy auto [国家/地区] 测速并切换到延迟最低节点\n/proxy countries 查看国家和地区";

struct Controller {
    client: Client,
    url: Url,
    group: String,
    token: String,
}
impl Controller {
    fn load() -> Result<Self> {
        let url = Url::parse(
            &std::env::var("MR_LI_PROXY_CONTROLLER")
                .unwrap_or_else(|_| "http://127.0.0.1:17990".into()),
        )?;
        ensure!(
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "代理控制器必须是本机 HTTP 根地址"
        );
        Ok(Self {
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(12))
                .build()?,
            url,
            group: std::env::var("MR_LI_PROXY_GROUP").unwrap_or_else(|_| "Proxy".into()),
            token: std::env::var("MR_LI_PROXY_SECRET").unwrap_or_default(),
        })
    }
    async fn request(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<Value>,
    ) -> Result<Value> {
        let mut url = self.url.clone();
        url.path_segments_mut().unwrap().clear().extend(segments);
        let mut request = self.client.request(method, url);
        if !self.token.is_empty() {
            request = request.bearer_auth(&self.token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .context("无法连接本机代理控制器，请检查后台内核是否运行")?;
        let status = response.status();
        ensure!(status.is_success(), "代理控制器返回 HTTP {status}");
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        response.json().await.context("代理控制器返回无效 JSON")
    }
    async fn group(&self) -> Result<Value> {
        self.request(Method::GET, &["proxies", &self.group], None)
            .await
    }
    async fn delay(&self, name: &str) -> Result<u64> {
        let mut url = self.url.clone();
        url.path_segments_mut()
            .unwrap()
            .clear()
            .extend(["proxies", name, "delay"]);
        url.query_pairs_mut()
            .append_pair("timeout", "3000")
            .append_pair("url", "https://www.gstatic.com/generate_204");
        let mut request = self.client.get(url);
        if !self.token.is_empty() {
            request = request.bearer_auth(&self.token);
        }
        let value: Value = tokio::time::timeout(Duration::from_secs(4), async {
            let response = request.send().await?;
            ensure!(
                response.status().is_success(),
                "节点无响应：HTTP {}",
                response.status()
            );
            Ok::<Value, anyhow::Error>(response.json().await?)
        })
        .await
        .context("节点测速超时")??;
        value["delay"]
            .as_u64()
            .filter(|d| *d > 0)
            .context("节点测速无有效延迟")
    }
    async fn switch(&self, name: &str) -> Result<()> {
        self.request(
            Method::PUT,
            &["proxies", &self.group],
            Some(json!({"name":name})),
        )
        .await?;
        let observed = self
            .group()
            .await
            .context("切换请求已发送，回读失败；请用 /proxy status 确认")?;
        ensure!(
            observed["now"] == name,
            "切换后当前节点不一致，请用 /proxy status 确认"
        );
        Ok(())
    }
    async fn rank(&self, names: &[Value], filter: &str) -> Result<Vec<Measurement>> {
        let catalog = self.request(Method::GET, &["proxies"], None).await?;
        let candidates: Vec<String> = names
            .iter()
            .filter_map(Value::as_str)
            .filter(|n| matches_filter(n, filter))
            .filter(|n| {
                let p = &catalog["proxies"][*n];
                p.is_object()
                    && p["all"].is_null()
                    && !matches!(
                        p["type"].as_str(),
                        None | Some("Direct" | "Reject" | "RejectDrop" | "Pass" | "Compatible")
                    )
                    && !n.contains("有效期")
                    && !n.contains("剩余")
                    && !n.contains("到期")
            })
            .map(str::to_owned)
            .collect();
        ensure!(
            !candidates.is_empty(),
            "没有匹配的实际代理节点，请用 /proxy list 查询"
        );
        ensure!(
            candidates.len() <= 64,
            "节点超过64个，请指定国家/地区后测速"
        );
        let mut results: Vec<_> = stream::iter(candidates)
            .map(|name| async move {
                let mut samples = Vec::new();
                for _ in 0..3 {
                    if let Ok(ms) = self.delay(&name).await {
                        samples.push(ms);
                    }
                }
                samples.sort_unstable();
                let delay = if samples.len() >= 2 {
                    Some(samples[samples.len() / 2])
                } else {
                    None
                };
                Measurement {
                    name,
                    delay,
                    successes: samples.len(),
                }
            })
            .buffer_unordered(8)
            .collect()
            .await;
        results.sort_by(|a, b| {
            a.delay
                .unwrap_or(u64::MAX)
                .cmp(&b.delay.unwrap_or(u64::MAX))
                .then(a.name.cmp(&b.name))
        });
        Ok(results)
    }
}

struct Measurement {
    name: String,
    delay: Option<u64>,
    successes: usize,
}
const COUNTRIES: &[(&str, &[&str])] = &[
    ("香港", &["香港", "hk", "hong kong", "hongkong", "🇭🇰"]),
    ("日本", &["日本", "jp", "japan", "🇯🇵"]),
    ("新加坡", &["新加坡", "sg", "singapore", "狮城", "🇸🇬"]),
    ("美国", &["美国", "us", "usa", "united states", "🇺🇸"]),
    ("台湾", &["台湾", "台灣", "tw", "taiwan", "🇹🇼"]),
    ("荷兰", &["荷兰", "荷蘭", "nl", "netherlands", "🇳🇱"]),
    ("德国", &["德国", "德國", "de", "germany", "🇩🇪"]),
    (
        "英国",
        &["英国", "英國", "gb", "uk", "united kingdom", "🇬🇧"],
    ),
    ("澳大利亚", &["澳大利亚", "澳洲", "au", "australia", "🇦🇺"]),
    ("爱沙尼亚", &["爱沙尼亚", "ee", "estonia", "🇪🇪"]),
    ("韩国", &["韩国", "韓國", "kr", "korea", "🇰🇷"]),
    ("加拿大", &["加拿大", "ca", "canada", "🇨🇦"]),
    ("法国", &["法国", "法國", "fr", "france", "🇫🇷"]),
    ("俄罗斯", &["俄罗斯", "ru", "russia", "🇷🇺"]),
];
fn alias_matches(name: &str, alias: &str) -> bool {
    if alias.len() == 2 && alias.is_ascii() {
        name.split(|c: char| !c.is_ascii_alphabetic())
            .any(|part| part == alias)
    } else {
        name.contains(alias)
    }
}
fn country(name: &str) -> Option<&'static str> {
    let name = name.to_lowercase();
    COUNTRIES
        .iter()
        .find(|(_, aliases)| aliases.iter().any(|a| alias_matches(&name, a)))
        .map(|(label, _)| *label)
}
fn matches_filter(name: &str, filter: &str) -> bool {
    let filter = filter.trim().to_lowercase();
    if matches!(filter.as_str(), "" | "all" | "全部" | "*") {
        return true;
    }
    if let Some((label, _)) = COUNTRIES
        .iter()
        .find(|(_, aliases)| aliases.contains(&filter.as_str()))
    {
        return country(name) == Some(*label);
    }
    name.to_lowercase().contains(&filter)
}
fn ranking(results: &[Measurement]) -> String {
    let mut lines =
        vec!["测速：Google 204，3次测试的延迟中位数；至少2次成功才参与排名（非下载带宽）".into()];
    for r in results {
        lines.push(match r.delay {
            Some(ms) => format!("{}：{ms} ms（{}/3成功）", r.name, r.successes),
            None => format!("{}：不可用（{}/3成功）", r.name, r.successes),
        });
    }
    lines.join("\n")
}

fn select<'a>(names: &'a [Value], query: &str) -> Result<&'a str> {
    ensure!(!query.is_empty(), "请提供节点名，例如 /proxy set HK-1");
    if let Some(name) = names.iter().filter_map(Value::as_str).find(|n| *n == query) {
        return Ok(name);
    }
    let query = query.to_lowercase();
    let matches: Vec<_> = names
        .iter()
        .filter_map(Value::as_str)
        .filter(|n| n.to_lowercase().contains(&query))
        .collect();
    match matches.as_slice() {
        [name] => Ok(name),
        [] => bail!("没有匹配节点，请用 /proxy list 查询"),
        _ => bail!("匹配多个节点，请指定更完整名称：\n{}", matches.join("\n")),
    }
}

pub async fn command(text: &str) -> Result<String> {
    command_with(Controller::load()?, text).await
}
async fn command_with(controller: Controller, text: &str) -> Result<String> {
    let (action, query) = text
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((text.trim(), ""));
    let query = query.trim();
    if action == "help" {
        return Ok(HELP.into());
    }
    ensure!(
        matches!(
            action,
            "" | "status" | "list" | "set" | "test" | "auto" | "countries"
        ),
        "{HELP}"
    );
    ensure!(
        matches!(action, "list" | "set" | "test" | "auto") || query.is_empty(),
        "{HELP}"
    );
    let group = controller.group().await?;
    let current = group["now"]
        .as_str()
        .context("该代理组不支持查询当前节点")?;
    let names = group["all"].as_array().context("该代理组没有可选节点")?;
    match action {
        "list" => {
            let rows: Vec<_> = names
                .iter()
                .filter_map(Value::as_str)
                .filter(|n| matches_filter(n, query))
                .map(|n| format!("{} {n}", if n == current { "●" } else { "○" }))
                .collect();
            Ok(format!("代理组：{}\n{}", controller.group, rows.join("\n")))
        }
        "set" => {
            ensure!(group["type"] == "Selector", "该代理组不支持手动切换");
            let name = select(names, query)?;
            controller.switch(name).await?;
            Ok(format!("代理组：{}\n已切换：{name}", controller.group))
        }
        "test" if query.is_empty() => {
            let delay = controller.delay(current).await?;
            Ok(format!(
                "当前节点：{current}\n延迟：{delay} ms（Google 204 连通测试，非下载带宽）"
            ))
        }
        "test" | "auto" => {
            if action == "auto" {
                ensure!(group["type"] == "Selector", "该代理组不支持手动切换");
            }
            let results = controller.rank(names, query).await?;
            let report = ranking(&results);
            if action == "auto" {
                let best = results
                    .iter()
                    .find(|r| r.delay.is_some())
                    .with_context(|| format!("{report}\n全部节点测速失败，保留原节点"))?;
                controller.switch(&best.name).await?;
                Ok(format!("{report}\n已切换：{}", best.name))
            } else {
                Ok(report)
            }
        }
        "countries" => {
            let mut counts = std::collections::BTreeMap::new();
            for n in names.iter().filter_map(Value::as_str) {
                if let Some(label) = country(n) {
                    *counts.entry(label).or_insert(0usize) += 1;
                }
            }
            Ok(format!(
                "国家/地区（按节点名称识别）：\n{}",
                counts
                    .into_iter()
                    .map(|(c, n)| format!("{c}：{n}个"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        }
        _ => Ok(format!("代理组：{}\n当前节点：{current}", controller.group)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolve_node_without_accidentally_selecting_ambiguous_matches() {
        let nodes = vec![json!("中国香港-HK-1"), json!("中国香港-HK-2"), json!("HK")];
        assert_eq!(select(&nodes, "hk-1").unwrap(), "中国香港-HK-1");
        assert_eq!(select(&nodes, "HK").unwrap(), "HK");
        assert!(select(&nodes, "香港").is_err());
        assert!(select(&nodes, "").is_err());
        assert!(select(&nodes, "missing").is_err());
    }
    #[test]
    fn country_aliases_do_not_confuse_codes_with_substrings() {
        for filter in ["日本", "JP", "Japan", "🇯🇵"] {
            assert!(matches_filter("日本-TY-1", filter));
        }
        assert!(matches_filter("🇭🇰 Hong Kong 01", "香港"));
        assert!(matches_filter("HK1", "hk"));
        assert!(!matches_filter("Australia-AU-1", "US"));
        assert!(matches_filter("德国-DE-1", "Germany"));
        assert!(!matches_filter("Auto", "AU"));
    }
    #[tokio::test]
    async fn batch_ranking_skips_groups_and_auto_switches_only_to_healthy_country_nodes()
    -> Result<()> {
        use axum::{Json, Router, extract::Path, http::StatusCode, routing::get};
        use std::sync::{Arc, Mutex};
        let selected = Arc::new(Mutex::new("HK-1".to_owned()));
        let read = selected.clone();
        let write = selected.clone();
        let app = Router::new()
            .route("/proxies", get(|| async { Json(json!({"proxies": {
                "HK-1":{"type":"Shadowsocks"}, "JP-fast":{"type":"Shadowsocks"},
                "JP-slow":{"type":"Shadowsocks"}, "US-dead":{"type":"Shadowsocks"},
                "Auto":{"type":"URLTest","all":["HK-1"]}, "DIRECT":{"type":"Direct"},
                "有效期2030":{"type":"Shadowsocks"}
            }})) }))
            .route("/proxies/Proxy", get(move || {
                let current = read.lock().unwrap().clone();
                async move { Json(json!({"type":"Selector","now":current,"all":["HK-1","JP-fast","JP-slow","US-dead","Auto","DIRECT","有效期2030"]})) }
            }).put(move |Json(value): Json<Value>| {
                *write.lock().unwrap() = value["name"].as_str().unwrap().into();
                async { StatusCode::NO_CONTENT }
            }))
            .route("/proxies/{name}/delay", get(|Path(name): Path<String>| async move {
                match name.as_str() {
                    "HK-1" => (StatusCode::OK, Json(json!({"delay":80}))),
                    "JP-fast" => (StatusCode::OK, Json(json!({"delay":20}))),
                    "JP-slow" => (StatusCode::OK, Json(json!({"delay":90}))),
                    "US-dead" => (StatusCode::GATEWAY_TIMEOUT, Json(json!({"message":"timeout"}))),
                    _ => panic!("non-node tested: {name}"),
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = Url::parse(&format!("http://{}", listener.local_addr()?))?;
        let task = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        let controller = || Controller {
            client: Client::builder().no_proxy().build().unwrap(),
            url: url.clone(),
            group: "Proxy".into(),
            token: String::new(),
        };
        let report = command_with(controller(), "test all").await?;
        assert!(report.find("JP-fast：20").unwrap() < report.find("HK-1：80").unwrap());
        assert!(report.contains("US-dead：不可用"));
        assert!(!report.contains("Auto："));
        assert_eq!(*selected.lock().unwrap(), "HK-1");
        let result = command_with(controller(), "auto Japan").await?;
        assert!(result.contains("已切换：JP-fast"));
        assert_eq!(*selected.lock().unwrap(), "JP-fast");
        assert!(command_with(controller(), "auto US").await.is_err());
        assert_eq!(*selected.lock().unwrap(), "JP-fast");
        assert!(command_with(controller(), "set JP").await.is_err());
        assert_eq!(*selected.lock().unwrap(), "JP-fast");
        task.abort();
        Ok(())
    }
}
