//! Home Assistant discovery/control, shared by slash commands, CLI and stdio MCP.
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

#[derive(Clone)]
pub struct Settings {
    url: reqwest::Url,
    token: String,
    pub env_file: PathBuf,
}
impl Settings {
    pub fn from_settings(
        get: impl Fn(&str, &str) -> String,
        env_file: &str,
    ) -> Result<Option<Self>> {
        let url = get("XIAOLI_HA_URL", "");
        let mut token = get("XIAOLI_HA_TOKEN", "");
        if token.trim().is_empty() {
            let file = get("XIAOLI_HA_TOKEN_FILE", "");
            if !file.is_empty() {
                token = std::fs::read_to_string(file)
                    .map_err(|_| anyhow::anyhow!("无法读取 HA 令牌文件"))?;
            }
        }
        if url.trim().is_empty() && token.trim().is_empty() {
            return Ok(None);
        }
        ensure!(
            !url.trim().is_empty() && !token.trim().is_empty(),
            "请同时配置 XIAOLI_HA_URL 和 XIAOLI_HA_TOKEN（或 XIAOLI_HA_TOKEN_FILE）"
        );
        let mut url =
            reqwest::Url::parse(url.trim()).map_err(|_| anyhow::anyhow!("HA 地址格式无效"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none(),
            "HA 地址必须是无内嵌凭证的 HTTP(S) 地址"
        );
        let path = format!("{}/api/websocket", url.path().trim_end_matches('/'));
        url.set_path(&path);
        url.set_query(None);
        url.set_fragment(None);
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme).unwrap();
        let env_file = PathBuf::from(env_file);
        let env_file = if env_file.is_absolute() {
            env_file
        } else {
            std::env::current_dir()?.join(env_file)
        };
        Ok(Some(Self {
            url,
            token: token.trim().into(),
            env_file,
        }))
    }
    pub fn load() -> Result<Option<Self>> {
        let path = std::env::var("MR_LI_ENV").unwrap_or_else(|_| ".env".into());
        let values = if std::path::Path::new(&path).exists() {
            dotenvy::from_path_iter(&path)
                .context("无法读取 HA 环境配置")?
                .collect::<std::result::Result<std::collections::HashMap<_, _>, _>>()
                .map_err(|_| anyhow::anyhow!("HA 环境配置语法错误"))?
        } else {
            Default::default()
        };
        Self::from_settings(
            |k, d| {
                std::env::var(k)
                    .ok()
                    .or_else(|| values.get(k).cloned())
                    .unwrap_or_else(|| d.into())
            },
            &path,
        )
    }
    pub fn mcp_acp(&self) -> Result<Value> {
        Ok(
            json!({"name":"home_assistant","command":std::env::current_exe()?,"args":["ha-mcp"],"env":[{"name":"MR_LI_ENV","value":self.env_file}]}),
        )
    }
    pub fn mcp_local(&self) -> Result<Value> {
        Ok(
            json!({"type":"local","command":[std::env::current_exe()?,"ha-mcp"],"environment":{"MR_LI_ENV":self.env_file},"disabled":false,"timeout":{"startup":20000}}),
        )
    }
    pub async fn maintain(&self, oc: &crate::opencode::OpenCode) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        let mut first = true;
        loop {
            interval.tick().await;
            if let Err(e) = self.register(oc, first).await {
                tracing::warn!(error=%e,"HA MCP 接入 OpenCode 失败，将重试")
            } else {
                first = false
            }
        }
    }
    async fn register(&self, oc: &crate::opencode::OpenCode, force: bool) -> Result<()> {
        let connected = |status: &Value| {
            status.as_array().is_some_and(|servers| {
                servers
                    .iter()
                    .any(|s| s["name"] == "home_assistant" && s["status"]["status"] == "connected")
            })
        };
        if force || !connected(&oc.get("/api/mcp").await?) {
            oc.request(
                reqwest::Method::PUT,
                "/api/experimental/mcp/home_assistant",
                Some(json!({"config":self.mcp_local()?})),
                Duration::from_secs(25),
            )
            .await?;
            ensure!(
                connected(&oc.get("/api/mcp").await?),
                "OpenCode 未连接 HA MCP"
            );
            tracing::info!("Home Assistant MCP 已接入 OpenCode");
        }
        Ok(())
    }
    pub async fn run(&self, name: &str, args: Value) -> Result<Value> {
        let control = name == "ha_control";
        let mut result = tokio::time::timeout(Duration::from_secs(25), self.execute(name, args))
            .await
            .map_err(|_| {
                anyhow::anyhow!(if control {
                    "HA 控制超时，执行结果可能未知；不要自动重试"
                } else {
                    "HA 查询超时"
                })
            })??;
        if control {
            let readback = async {
                let mut conn = Connection::open(self).await?;
                let states = conn.call("get_states", json!({})).await?;
                states
                    .as_array()
                    .and_then(|s| s.iter().find(|s| s["entity_id"] == result["entity_id"]))
                    .cloned()
                    .context("回读实体不存在")
            };
            match tokio::time::timeout(Duration::from_secs(4), readback).await {
                Ok(Ok(state)) => result["observed_state"] = state,
                _ => {
                    result["state_readback_error"] =
                        json!("控制已接受，但回读状态失败；不要自动重试")
                }
            }
        }
        Ok(result)
    }
    async fn execute(&self, name: &str, args: Value) -> Result<Value> {
        ensure!(
            matches!(name, "ha_devices" | "ha_commands" | "ha_control"),
            "未知 HA 工具"
        );
        let control = name == "ha_control";
        let keys = match name {
            "ha_devices" => vec!["query"],
            "ha_commands" => vec!["entity_id", "device_id"],
            _ => vec!["entity_id", "command", "data"],
        };
        ensure!(
            args.as_object()
                .is_some_and(|m| m.keys().all(|k| keys.contains(&k.as_str()))),
            "HA 参数格式错误或含未知字段"
        );
        for key in &keys {
            if *key != "data"
                && let Some(value) = args.get(*key)
            {
                ensure!(value.is_string(), "HA 名称和标识参数必须是字符串")
            }
        }
        let entity = args["entity_id"].as_str().unwrap_or("");
        let device = args["device_id"].as_str().unwrap_or("");
        if name == "ha_commands" {
            ensure!(
                entity.is_empty() != device.is_empty(),
                "请提供 entity_id 或 device_id 中的一项"
            )
        }
        if control {
            ensure!(
                identifier(entity) && identifier(args["command"].as_str().unwrap_or("")),
                "entity_id 和 command 必须是准确的 domain.name"
            )
        }
        let mut conn = Connection::open(self).await?;
        let states = conn.call("get_states", json!({})).await?;
        let states = states.as_array().context("HA 状态格式不兼容")?;
        if control {
            return control_entity(&mut conn, &args, states).await;
        }
        let devices = conn.call("config/device_registry/list", json!({})).await?;
        let entities = conn.call("config/entity_registry/list", json!({})).await?;
        let areas = conn.call("config/area_registry/list", json!({})).await?;
        let inventory = inventory(states, &devices, &entities, &areas)?;
        if name == "ha_devices" {
            let q = args["query"].as_str().unwrap_or("").trim().to_lowercase();
            let filter = |key: &str| {
                inventory[key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|v| q.is_empty() || v.to_string().to_lowercase().contains(&q))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            return Ok(
                json!({"devices":filter("devices"),"standalone_entities":filter("standalone_entities")}),
            );
        }
        let selected = inventory["devices"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|d| {
                d["entities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(move |e| (d["device_id"].as_str().unwrap_or(""), e))
            })
            .chain(
                inventory["standalone_entities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| ("", e)),
            )
            .filter(|(d, e)| {
                (!device.is_empty() && device == *d)
                    || (!entity.is_empty() && e["entity_id"] == entity)
            })
            .map(|(_, e)| e.clone())
            .collect::<Vec<_>>();
        ensure!(!selected.is_empty(), "未找到有效实体，请先查询 ha_devices");
        let services = conn.call("get_services", json!({})).await?;
        Ok(
            json!({"entities":selected.iter().map(|e|json!({"entity":e,"commands":commands(e,&services)})).collect::<Vec<_>>()}),
        )
    }
}
struct Connection {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    id: u64,
}
impl Connection {
    async fn open(cfg: &Settings) -> Result<Self> {
        let config = WebSocketConfig::default()
            .max_message_size(Some(8 * 1024 * 1024))
            .max_frame_size(Some(8 * 1024 * 1024));
        let (ws, _) =
            tokio_tungstenite::connect_async_with_config(cfg.url.as_str(), Some(config), false)
                .await
                .map_err(|_| anyhow::anyhow!("无法连接 HA，请检查地址和服务状态"))?;
        let mut c = Self { ws, id: 0 };
        ensure!(
            c.read().await?["type"] == "auth_required",
            "HA 认证握手失败"
        );
        c.send(json!({"type":"auth","access_token":cfg.token}))
            .await?;
        ensure!(
            c.read().await?["type"] == "auth_ok",
            "HA 认证失败，请检查 XIAOLI_HA_TOKEN"
        );
        Ok(c)
    }
    async fn read(&mut self) -> Result<Value> {
        while let Some(m) = self.ws.next().await {
            match m.map_err(|_| anyhow::anyhow!("HA 连接中断；控制结果可能未知，不要自动重试"))?
            {
                Message::Text(s) => {
                    return serde_json::from_str(&s)
                        .map_err(|_| anyhow::anyhow!("HA 响应格式错误"));
                }
                Message::Ping(p) => self
                    .ws
                    .send(Message::Pong(p))
                    .await
                    .map_err(|_| anyhow::anyhow!("HA 连接中断"))?,
                Message::Close(_) => break,
                _ => {}
            }
        }
        bail!("HA 连接关闭；控制结果可能未知，不要自动重试")
    }
    async fn send(&mut self, v: Value) -> Result<()> {
        self.ws
            .send(Message::Text(v.to_string().into()))
            .await
            .map_err(|_| anyhow::anyhow!("HA 发送失败；控制结果可能未知，不要自动重试"))
    }
    async fn call(&mut self, kind: &str, mut params: Value) -> Result<Value> {
        self.id += 1;
        params["id"] = json!(self.id);
        params["type"] = json!(kind);
        self.send(params).await?;
        loop {
            let v = self.read().await?;
            if v["type"] != "result" || v["id"] != self.id {
                continue;
            }
            if v["success"] != true {
                let code = v["error"]["code"]
                    .as_str()
                    .filter(|s| {
                        !s.is_empty()
                            && s.len() < 80
                            && s.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                    })
                    .unwrap_or("request_failed");
                bail!("HA {kind} 请求失败（{code}）")
            }
            return Ok(v["result"].clone());
        }
    }
}
fn identifier(s: &str) -> bool {
    s.split_once('.').is_some_and(|(a, b)| {
        !a.is_empty()
            && !b.is_empty()
            && a.chars()
                .chain(b.chars())
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    })
}
fn list(v: &Value) -> Vec<&Value> {
    v.as_array()
        .map(|v| v.iter().collect())
        .unwrap_or_else(|| vec![v])
}
fn contains(want: &Value, actual: &Value) -> bool {
    list(want).contains(&actual)
}
fn matches_filter(state: &Value, filter: &Value) -> bool {
    let Some(filter) = filter.as_object() else {
        return false;
    };
    filter.iter().all(|(key, want)| match key.as_str() {
        "domain" => contains(
            want,
            &json!(
                state["entity_id"]
                    .as_str()
                    .unwrap_or("")
                    .split('.')
                    .next()
                    .unwrap_or("")
            ),
        ),
        "device_class" => contains(want, &state["attributes"]["device_class"]),
        "supported_features" => {
            let actual = state["attributes"]["supported_features"]
                .as_u64()
                .unwrap_or(0);
            list(want).iter().any(|option| {
                list(option)
                    .iter()
                    .all(|bit| bit.as_u64().is_some_and(|b| b > 0 && actual & b == b))
            })
        }
        "attribute" => want.as_object().is_some_and(|m| {
            m.iter().all(|(key, want)| {
                list(&state["attributes"][key])
                    .iter()
                    .any(|actual| contains(want, actual))
            })
        }),
        _ => false,
    })
}
fn target_matches(state: &Value, target: &Value, cross_domain: bool) -> bool {
    let Some(target) = target.as_object() else {
        return false;
    };
    if let Some(filters) = target.get("entity") {
        list(filters)
            .iter()
            .any(|f| (!cross_domain || f.get("domain").is_some()) && matches_filter(state, f))
    } else {
        !cross_domain && target.is_empty()
    }
}
fn commands(state: &Value, services: &Value) -> Value {
    let mut result = BTreeMap::new();
    let domain = state["entity_id"]
        .as_str()
        .unwrap_or("")
        .split('.')
        .next()
        .unwrap_or("");
    if let Some(services) = services.as_object() {
        for (service_domain, items) in services {
            if let Some(items) = items.as_object() {
                for (name, service) in items {
                    if !target_matches(state, &service["target"], service_domain != domain) {
                        continue;
                    }
                    let mut service = service.clone();
                    let fields = service["fields"]
                        .as_object()
                        .map(|fields| {
                            fields
                                .iter()
                                .filter(|(_, field)| {
                                    field.get("filter").is_none_or(|f| matches_filter(state, f))
                                })
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<serde_json::Map<_, _>>()
                        })
                        .unwrap_or_default();
                    service["fields"] = json!(fields);
                    result.insert(format!("{service_domain}.{name}"), service);
                }
            }
        }
    }
    json!(result)
}
fn inventory(states: &[Value], devices: &Value, entities: &Value, areas: &Value) -> Result<Value> {
    let areas = areas.as_array().context("HA 房间格式错误")?;
    let devices = devices.as_array().context("HA 设备格式错误")?;
    let registry = entities.as_array().context("HA 实体格式错误")?;
    let area_name = |id: &Value| {
        areas
            .iter()
            .find(|a| a["area_id"] == *id)
            .map(|a| a["name"].clone())
            .unwrap_or(Value::Null)
    };
    let mut output = BTreeMap::<String, Value>::new();
    for d in devices {
        if !d["disabled_by"].is_null() {
            continue;
        }
        let id = d["id"].as_str().context("HA 设备 ID 缺失")?;
        output.insert(id.into(),json!({"device_id":id,"name":d.get("name_by_user").filter(|n|n.as_str().is_some_and(|n|!n.is_empty())).unwrap_or(&d["name"]),"manufacturer":d["manufacturer"],"model":d["model"],"area":area_name(&d["area_id"]),"types":[],"entities":[]}));
    }
    let mut other = vec![];
    for state in states {
        let r = registry
            .iter()
            .find(|e| e["entity_id"] == state["entity_id"]);
        if r.is_some_and(|r| !r["disabled_by"].is_null()) {
            continue;
        }
        let mut e = state.clone();
        let domain = e["entity_id"]
            .as_str()
            .unwrap_or("")
            .split('.')
            .next()
            .unwrap_or("")
            .to_owned();
        e["type"] = json!(domain);
        let device = r.and_then(|r| r["device_id"].as_str());
        let area = r.map(|r| area_name(&r["area_id"])).unwrap_or(Value::Null);
        if let Some(d) = device.and_then(|id| output.get_mut(id)) {
            e["area"] = if area.is_null() {
                d["area"].clone()
            } else {
                area
            };
            let types = d["types"].as_array_mut().unwrap();
            if !types.contains(&json!(domain)) {
                types.push(json!(domain));
                types.sort_by_key(Value::to_string)
            }
            d["entities"].as_array_mut().unwrap().push(e);
        } else {
            if device.is_some_and(|id| {
                devices
                    .iter()
                    .any(|d| d["id"] == id && !d["disabled_by"].is_null())
            }) {
                continue;
            }
            e["area"] = area;
            other.push(e)
        }
    }
    for d in output.values_mut() {
        d["entities"]
            .as_array_mut()
            .unwrap()
            .sort_by_key(|e| e["entity_id"].to_string())
    }
    other.sort_by_key(|e| e["entity_id"].to_string());
    Ok(json!({"devices":output.into_values().collect::<Vec<_>>(),"standalone_entities":other}))
}
fn validate_data(data: &Value, fields: &Value) -> Result<()> {
    let data = data.as_object().context("data 必须是 JSON 对象")?;
    for (key, value) in data {
        ensure!(
            !["entity_id", "device_id", "area_id", "label_id", "target"].contains(&key.as_str()),
            "data 不允许覆盖控制目标"
        );
        let field = fields
            .get(key)
            .context("包含不支持的参数，请先查询 ha_commands")?;
        if let Some(number) = field["selector"].get("number") {
            let n = value.as_f64().context("数值参数格式错误")?;
            ensure!(
                number["min"].as_f64().is_none_or(|min| n >= min)
                    && number["max"].as_f64().is_none_or(|max| n <= max),
                "数值参数超出范围"
            );
        }
        if field["selector"].get("boolean").is_some() {
            ensure!(value.is_boolean(), "布尔参数格式错误")
        }
    }
    if let Some(fields) = fields.as_object() {
        for (key, field) in fields {
            if field["required"] == true {
                ensure!(
                    data.get(key).is_some_and(|v| !v.is_null()),
                    "缺少必填参数，请先查询 ha_commands"
                )
            }
        }
    }
    Ok(())
}
async fn control_entity(conn: &mut Connection, args: &Value, states: &[Value]) -> Result<Value> {
    let id = args["entity_id"].as_str().unwrap();
    let command = args["command"].as_str().unwrap();
    let before = states
        .iter()
        .find(|e| e["entity_id"] == id)
        .context("实体不存在，请先查询 ha_devices")?;
    ensure!(
        !matches!(before["state"].as_str(), Some("unavailable" | "unknown")),
        "实体不可用，未发送控制命令"
    );
    let services = conn.call("get_services", json!({})).await?;
    let supported = commands(before, &services);
    let service = supported
        .get(command)
        .context("该实体不支持此命令，请查询 ha_commands")?;
    let data = args.get("data").cloned().unwrap_or_else(|| json!({}));
    validate_data(&data, &service["fields"])?;
    let (domain, name) = command.split_once('.').unwrap();
    let mut params =
        json!({"domain":domain,"service":name,"target":{"entity_id":id},"service_data":data});
    if service.get("response").is_some() {
        params["return_response"] = json!(true)
    }
    let response = conn.call("call_service", params).await?;
    let mut result = json!({"accepted":true,"entity_id":id,"command":command,"before":before,"physical_effect_confirmed":false});
    if let Some(response) = response.get("response") {
        result["service_response"] = response.clone()
    }
    Ok(result)
}

pub fn tools() -> Value {
    json!([
     {"name":"ha_devices","description":"查询 Home Assistant 物理设备、房间、类型、关联实体、状态与属性。query 按名称/类型/房间/ID 筛选；控制前先查准确实体。","inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"additionalProperties":false},"annotations":{"readOnlyHint":true}},
     {"name":"ha_commands","description":"查询指定实体或物理设备的实际可用操作和参数。entity_id/device_id 二选一。只读传感器没有操作；不要编造命令。","inputSchema":{"type":"object","properties":{"entity_id":{"type":"string"},"device_id":{"type":"string"}},"additionalProperties":false},"annotations":{"readOnlyHint":true}},
     {"name":"ha_control","description":"按用户要求控制一个准确的 HA 实体。先用 ha_devices/ha_commands 查实体、命令和参数；有歧义先澄清。accepted 只表示 HA 接受，observed_state 是即时回读，不能证明物理动作。超时/断线结果未知时不要自动重试。","inputSchema":{"type":"object","properties":{"entity_id":{"type":"string"},"command":{"type":"string","description":"实际返回的 domain.service"},"data":{"type":"object","description":"仅操作参数，不允许 target/entity_id/device_id/area_id/label_id"}},"required":["entity_id","command"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true}}
    ])
}
pub async fn rpc(settings: &Settings, r: &Value) -> Value {
    let id = &r["id"];
    let result = match r["method"].as_str().unwrap_or("") {
        "initialize" => {
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mr-li-home-assistant","version":env!("CARGO_PKG_VERSION")}})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":tools()}),
        "tools/call" => match settings
            .run(
                r["params"]["name"].as_str().unwrap_or(""),
                r["params"]
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            )
            .await
        {
            Ok(v) => {
                json!({"content":[{"type":"text","text":v.to_string()}],"structuredContent":v,"isError":false})
            }
            Err(e) => json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
        },
        _ => {
            return json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unknown method"}});
        }
    };
    json!({"jsonrpc":"2.0","id":id,"result":result})
}
pub async fn stdio() -> Result<()> {
    let cfg = Settings::load()?.context("请配置 HA 地址和令牌后启用 ha-mcp")?;
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    loop {
        let mut line = Vec::new();
        let count = tokio::io::AsyncReadExt::take(&mut input, 1024 * 1024 + 1)
            .read_until(b'\n', &mut line)
            .await?;
        if count == 0 {
            break;
        }
        ensure!(line.len() <= 1024 * 1024, "MCP 请求过大");
        let request: Value = match serde_json::from_slice(&line) {
            Ok(v) => v,
            Err(_) => {
                output.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"Parse error\"}}\n").await?;
                output.flush().await?;
                continue;
            }
        };
        if request.get("id").is_none() {
            continue;
        }
        let reply = rpc(&cfg, &request).await;
        output.write_all(format!("{reply}\n").as_bytes()).await?;
        output.flush().await?;
    }
    Ok(())
}
pub fn parse_command(text: &str) -> Result<(&'static str, Value)> {
    let (action, rest) = text
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((text.trim(), ""));
    let rest = rest.trim();
    match action {
        "devices" => Ok(("ha_devices", json!({"query":rest}))),
        "commands" => {
            ensure!(
                !rest.is_empty() && !rest.chars().any(char::is_whitespace),
                "用法：/ha commands <实体ID或设备ID>"
            );
            Ok((
                "ha_commands",
                if rest.contains('.') {
                    json!({"entity_id":rest})
                } else {
                    json!({"device_id":rest})
                },
            ))
        }
        "control" => {
            let (entity, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let rest = rest.trim_start();
            let (command, data) = rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
            let data = data.trim();
            ensure!(
                identifier(entity) && identifier(command),
                "用法：/ha control <实体ID> <domain.service> [JSON参数]"
            );
            let data: Value = serde_json::from_str(data)
                .map_err(|_| anyhow::anyhow!("操作参数必须是 JSON 对象"))?;
            Ok((
                "ha_control",
                json!({"entity_id":entity,"command":command,"data":data}),
            ))
        }
        _ => bail!(
            "/ha devices [名称/房间/类型]\n/ha commands <实体ID或设备ID>\n/ha control <实体ID> <domain.service> [JSON参数]"
        ),
    }
}
pub async fn command(settings: Option<&Settings>, text: &str) -> Result<Value> {
    let cfg = settings.context("Home Assistant 未配置，请设置 XIAOLI_HA_URL 和 XIAOLI_HA_TOKEN")?;
    let (name, args) = parse_command(text)?;
    cfg.run(name, args).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    struct Fake {
        states: Value,
        calls: Vec<Value>,
        auth_fail: bool,
        service_fail: bool,
        drop_control: bool,
        readback_fail: bool,
        accepted: bool,
    }
    fn fixture() -> Fake {
        Fake {
            states: json!([
                {"entity_id":"light.living","state":"off","attributes":{"friendly_name":"客厅灯","supported_color_modes":["brightness"]}},
                {"entity_id":"sensor.temperature","state":"22","attributes":{"device_class":"temperature"}},
                {"entity_id":"media_player.projector","state":"on","attributes":{"supported_features":4}},
                {"entity_id":"light.offline","state":"unavailable","attributes":{}}
            ]),
            calls: vec![],
            auth_fail: false,
            service_fail: false,
            drop_control: false,
            readback_fail: false,
            accepted: false,
        }
    }
    fn services() -> Value {
        json!({
            "light":{"turn_on":{"target":{"entity":{"domain":"light"}},"fields":{"brightness_pct":{"selector":{"number":{"min":0,"max":100}},"filter":{"attribute":{"supported_color_modes":["brightness","hs"]}}},"rgb_color":{"filter":{"attribute":{"supported_color_modes":["rgb"]}}}}},"turn_off":{"target":{},"fields":{}}},
            "media_player":{"volume_set":{"target":{"entity":{"domain":"media_player","supported_features":[4]}},"fields":{"volume_level":{"required":true,"selector":{"number":{"min":0,"max":1}}}}},"media_play":{"target":{"entity":{"supported_features":[16384]}},"fields":{}},"toggle":{"target":{"entity":{"supported_features":[[128,256]]}},"fields":{}}},
            "custom":{"set_level":{"target":{"entity":{"domain":"light"}},"fields":{"level":{"required":true}},"response":{"optional":false}}},
            "homeassistant":{"restart":{"fields":{}}}
        })
    }
    async fn server(fake: Fake) -> (Settings, Arc<Mutex<Fake>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Mutex::new(fake));
        let state = shared.clone();
        let handle = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                    ws.send(Message::Text(
                        json!({"type":"auth_required"}).to_string().into(),
                    ))
                    .await
                    .unwrap();
                    let auth = ws.next().await.unwrap().unwrap().into_text().unwrap();
                    let auth: Value = serde_json::from_str(&auth).unwrap();
                    assert_eq!(auth["access_token"], "private-token");
                    let fail = state.lock().unwrap().auth_fail;
                    ws.send(Message::Text(json!({"type":if fail{"auth_invalid"}else{"auth_ok"},"message":"private-token"}).to_string().into())).await.unwrap();
                    if fail {
                        return;
                    }
                    while let Some(Ok(Message::Text(raw))) = ws.next().await {
                        let m: Value = serde_json::from_str(&raw).unwrap();
                        let (result, error, close) = {
                            let mut s = state.lock().unwrap();
                            s.calls.push(m.clone());
                            match m["type"].as_str().unwrap() {
                                "get_states" => {
                                    (s.states.clone(), false, s.accepted && s.readback_fail)
                                }
                                "get_services" => (services(), false, false),
                                "config/device_registry/list" => (
                                    json!([{"id":"d1","name":"original","name_by_user":"客厅设备","manufacturer":"Xiaomi","area_id":"living"}]),
                                    false,
                                    false,
                                ),
                                "config/entity_registry/list" => (
                                    json!([{"entity_id":"light.living","device_id":"d1"},{"entity_id":"sensor.temperature","device_id":"d1","area_id":"bedroom"}]),
                                    false,
                                    false,
                                ),
                                "config/area_registry/list" => (
                                    json!([{"area_id":"living","name":"客厅"},{"area_id":"bedroom","name":"卧室"}]),
                                    false,
                                    false,
                                ),
                                "call_service" => {
                                    s.accepted = true;
                                    s.states[0]["state"] = json!("on");
                                    (
                                        json!({"response":{"result":"ok"}}),
                                        s.service_fail,
                                        s.drop_control,
                                    )
                                }
                                other => panic!("unexpected {other}"),
                            }
                        };
                        if close {
                            break;
                        }
                        if ws.send(Message::Text(json!({"id":m["id"],"type":"result","success":!error,"result":result,"error":{"code":"service_validation_error","message":"private-token"}}).to_string().into())).await.is_err(){break}
                    }
                });
            }
        });
        let settings = Settings::from_settings(
            |k, _| match k {
                "XIAOLI_HA_URL" => format!("http://127.0.0.1:{port}/?external_auth=1"),
                "XIAOLI_HA_TOKEN" => "private-token".into(),
                _ => String::new(),
            },
            ".env",
        )
        .unwrap()
        .unwrap();
        (settings, shared, handle)
    }
    #[tokio::test]
    async fn inventory_commands_and_control() -> Result<()> {
        let (c, s, h) = server(fixture()).await;
        let v = c.run("ha_devices", json!({})).await?;
        assert_eq!(v["devices"][0]["name"], "客厅设备");
        assert_eq!(v["devices"][0]["types"], json!(["light", "sensor"]));
        assert_eq!(v["devices"][0]["entities"][0]["area"], "客厅");
        assert_eq!(v["devices"][0]["entities"][1]["area"], "卧室");
        assert_eq!(v["standalone_entities"].as_array().unwrap().len(), 2);
        let v = c.run("ha_devices", json!({"query":"XIAOMI"})).await?;
        assert_eq!(v["standalone_entities"], json!([]));
        assert_eq!(v["devices"].as_array().unwrap().len(), 1);
        let v = c.run("ha_commands", json!({"device_id":"d1"})).await?;
        assert_eq!(v["entities"].as_array().unwrap().len(), 2);
        let cmd = &v["entities"][0]["commands"];
        assert!(cmd["light.turn_on"]["fields"].get("rgb_color").is_none());
        assert!(cmd.get("custom.set_level").is_some());
        assert_eq!(v["entities"][1]["commands"], json!({}));
        let v = c
            .run("ha_commands", json!({"entity_id":"media_player.projector"}))
            .await?;
        assert!(
            v["entities"][0]["commands"]
                .get("media_player.volume_set")
                .is_some()
        );
        assert!(
            v["entities"][0]["commands"]
                .get("media_player.media_play")
                .is_none()
        );
        assert!(
            v["entities"][0]["commands"]
                .get("media_player.toggle")
                .is_none()
        );
        let v=c.run("ha_control",json!({"entity_id":"light.living","command":"light.turn_on","data":{"brightness_pct":40}})).await?;
        assert_eq!(v["accepted"], true);
        assert_eq!(v["observed_state"]["state"], "on");
        assert_eq!(v["physical_effect_confirmed"], false);
        let s = s.lock().unwrap();
        let calls = s
            .calls
            .iter()
            .filter(|m| m["type"] == "call_service")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["target"], json!({"entity_id":"light.living"}));
        h.abort();
        Ok(())
    }
    #[tokio::test]
    async fn invalid_controls_have_no_side_effects() -> Result<()> {
        let (c, s, h) = server(fixture()).await;
        for args in [
            json!({"entity_id":"all","command":"light.turn_on"}),
            json!({"entity_id":"light.fake","command":"light.turn_on"}),
            json!({"entity_id":"light.offline","command":"light.turn_on"}),
            json!({"entity_id":"light.living","command":"homeassistant.restart"}),
            json!({"entity_id":"media_player.projector","command":"media_player.media_play"}),
            json!({"entity_id":"media_player.projector","command":"media_player.volume_set"}),
            json!({"entity_id":"light.living","command":"light.turn_on","data":{"entity_id":"all"}}),
            json!({"entity_id":"light.living","command":"light.turn_on","data":{"rgb_color":[1,2,3]}}),
            json!({"entity_id":"light.living","command":"light.turn_on","data":{"brightness_pct":101}}),
            json!({"entity_id":"light.living","command":"light.turn_on","data":{"brightness_pct":"40"}}),
            json!({"entity_id":"light.living","command":"light.turn_on","target":{"area_id":"all"}}),
        ] {
            assert!(c.run("ha_control", args).await.is_err())
        }
        assert!(
            !s.lock()
                .unwrap()
                .calls
                .iter()
                .any(|m| m["type"] == "call_service")
        );
        h.abort();
        Ok(())
    }
    #[tokio::test]
    async fn errors_hide_secrets_and_do_not_retry() -> Result<()> {
        for mode in ["auth", "service", "drop", "readback"] {
            let mut f = fixture();
            f.auth_fail = mode == "auth";
            f.service_fail = mode == "service";
            f.drop_control = mode == "drop";
            f.readback_fail = mode == "readback";
            let (c, s, h) = server(f).await;
            let result = c
                .run(
                    "ha_control",
                    json!({"entity_id":"light.living","command":"light.turn_on"}),
                )
                .await;
            if mode == "readback" {
                let v = result?;
                assert_eq!(v["accepted"], true);
                assert!(v.get("state_readback_error").is_some())
            } else {
                let error = result.unwrap_err().to_string();
                assert!(!error.contains("private-token"));
                if mode == "drop" {
                    assert!(error.contains("未知"))
                }
            }
            assert_eq!(
                s.lock()
                    .unwrap()
                    .calls
                    .iter()
                    .filter(|m| m["type"] == "call_service")
                    .count(),
                usize::from(mode != "auth")
            );
            h.abort();
        }
        Ok(())
    }
    #[tokio::test]
    async fn mcp_exposes_tools_and_returns_service_response() -> Result<()> {
        let (c, s, h) = server(fixture()).await;
        let v = rpc(&c, &json!({"id":1,"method":"tools/list"})).await;
        assert_eq!(v["result"]["tools"].as_array().unwrap().len(), 3);
        assert!(!v.to_string().contains("private-token"));
        let v=rpc(&c,&json!({"id":2,"method":"tools/call","params":{"name":"ha_control","arguments":{"entity_id":"light.living","command":"custom.set_level","data":{"level":5}}}})).await;
        assert_eq!(v["result"]["isError"], false);
        assert_eq!(
            v["result"]["structuredContent"]["service_response"],
            json!({"result":"ok"})
        );
        assert_eq!(
            s.lock()
                .unwrap()
                .calls
                .iter()
                .find(|c| c["type"] == "call_service")
                .unwrap()["return_response"],
            true
        );
        h.abort();
        Ok(())
    }
    #[test]
    fn config_and_command_parser() {
        assert!(
            Settings::from_settings(|_, _| String::new(), ".env")
                .unwrap()
                .is_none()
        );
        for url in ["https://user:secret@example.com", "ftp://example.com"] {
            assert!(
                Settings::from_settings(
                    |key, _| if key == "XIAOLI_HA_URL" {
                        url.into()
                    } else if key == "XIAOLI_HA_TOKEN" {
                        "secret".into()
                    } else {
                        String::new()
                    },
                    ".env"
                )
                .is_err()
            )
        }
        let (_, v) =
            parse_command("control light.living light.turn_on {\"brightness_pct\":40}").unwrap();
        assert_eq!(v["data"]["brightness_pct"], 40);
        assert!(parse_command("commands").is_err());
        assert!(parse_command("control all light.turn_on").is_err());
        let (name, args) = parse_command("devices 客厅").unwrap();
        assert_eq!(name, "ha_devices");
        assert_eq!(args["query"], "客厅");
    }
}
