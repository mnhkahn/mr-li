//! Session-bound Agent tools backed by the same queue as user slash commands.
use crate::{bridge::Bridge, config::Config, opencode::OpenCode, store::Job};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn data_dir(cfg: &Config) -> Result<PathBuf> {
    Ok(if cfg.data_dir.is_absolute() {
        cfg.data_dir.clone()
    } else {
        std::env::current_dir()?.join(&cfg.data_dir)
    })
}
pub fn mcp_acp(cfg: &Config) -> Result<Value> {
    Ok(
        json!({"name":"teacher","command":std::env::current_exe()?,"args":["builtin-mcp"],"env":[{"name":"MR_LI_DATA_DIR","value":data_dir(cfg)?}]}),
    )
}
fn mcp_local(cfg: &Config) -> Result<Value> {
    Ok(
        json!({"type":"local","command":[std::env::current_exe()?,"builtin-mcp"],"environment":{"MR_LI_DATA_DIR":data_dir(cfg)?},"disabled":false,"timeout":{"startup":20000,"execution":240000}}),
    )
}
pub async fn maintain(cfg: &Config, oc: &OpenCode) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    let mut first = true;
    loop {
        interval.tick().await;
        let result: Result<()> = async {
            let connected = |v: &Value| {
                v.as_array().is_some_and(|a| {
                    a.iter()
                        .any(|s| s["name"] == "teacher" && s["status"]["status"] == "connected")
                })
            };
            if first || !connected(&oc.get("/api/mcp").await?) {
                oc.request(
                    reqwest::Method::PUT,
                    "/api/experimental/mcp/teacher",
                    Some(json!({"config":mcp_local(cfg)?})),
                    Duration::from_secs(25),
                )
                .await?;
                ensure!(
                    connected(&oc.get("/api/mcp").await?),
                    "OpenCode 未连接李老师内置工具"
                );
                tracing::info!("李老师内置工具已接入 OpenCode");
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => first = false,
            Err(e) => tracing::warn!(error=%e,"李老师内置工具接入失败，将重试"),
        }
    }
}

pub fn tools() -> Value {
    let mut tools = json!([
        {"name":"teacher_speak","description":"通过 Mac mini 喇叭说话，或停止当前会话语音。沿用现有音色；text 最多500字。超时先查任务，不要重复播报。","inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["say","stop"]},"text":{"type":"string","minLength":1,"maxLength":500}},"required":["action"]}},
        {"name":"teacher_camera","description":"摄像头拍照或录制1–30秒视频，结果直接发到发起请求的飞书聊天。返回状态不代表你已看过画面，不得编造画面内容。","inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["photo","video"]},"seconds":{"type":"integer","minimum":1,"maximum":30}},"required":["action"]}},
        {"name":"teacher_file","description":"将Agent工作目录内已有的普通文件作为附件发送到当前飞书聊天，支持PDF、Word、表格、压缩包等。先生成并保存文件，再传相对工作目录的路径或目录内绝对路径；可用file_name设置接收者看到的文件名。单文件非空且不超过30MB，目录需先压缩。上传完成不代表已发送，应检查file_delivery或用teacher_query查询原任务；重试沿用operation_id。","inputSchema":{"type":"object","properties":{"path":{"type":"string","minLength":1,"maxLength":4096},"file_name":{"type":"string","minLength":1,"maxLength":200}},"required":["path"]}},
        {"name":"teacher_image","description":"将Agent工作目录内已有的PNG、JPEG、GIF、WebP图片以原生图片消息发送到当前飞书聊天。先生成并保存文件，再传相对工作目录的路径或目录内绝对路径；可用file_name设置接收者看到的文件名。图片必须非空且小于10MB；其他格式请用teacher_file作为附件发送。上传完成不代表已发送，应检查file_delivery或用teacher_query查询原任务；重试沿用operation_id。","inputSchema":{"type":"object","properties":{"path":{"type":"string","minLength":1,"maxLength":4096},"file_name":{"type":"string","minLength":1,"maxLength":200}},"required":["path"]}},
        {"name":"teacher_posture","description":"单次坐姿检测并发送标注图，或开启/停止当前聊天的定时坐姿监测。定时监测仅每日北京时间17:00–21:30，默认每60秒采样、每10分钟汇总；interval_seconds支持30–3600。","inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["check","start","stop"]},"interval_seconds":{"type":"integer","minimum":30,"maximum":3600}},"required":["action"]}},
        {"name":"teacher_notice","description":"给学习平板首页发送文字通知，或取消当前用户在当前聊天尚未送达的通知。保存/排队不代表送达；用teacher_query notices查询确认。","inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["send","cancel"]},"text":{"type":"string","minLength":1,"maxLength":2000},"device_id":{"type":"string"},"notice_id":{"type":"string"}},"required":["action"]}},
        {"name":"teacher_proxy","description":"查询Mac mini代理节点、按国家/地区筛选、测试延迟或按用户要求切换。status/countries/list为查询；test无country测当前节点，有country批量测3次并排名（all为全部）；auto按country测速并切换最低延迟节点，未指定国家时比较全部节点。延迟不是下载带宽，国家按节点名称识别。set需要node唯一名称或关键字。仅在用户要求切换时使用set/auto。","inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["status","list","countries","test","set","auto"]},"country":{"type":"string","minLength":1,"maxLength":80},"node":{"type":"string","minLength":1,"maxLength":200}},"required":["action"]}},
        {"name":"teacher_query","description":"只读查询当前会话任务、状态、设备入口、坐姿定时监测、通知送达、技能或MCP连接。task需要task_id。不能访问其他用户/会话的任务或通知。","inputSchema":{"type":"object","properties":{"kind":{"type":"string","enum":["tasks","task","status","channel","cron","posture","notices","skills","mcp","help"]},"task_id":{"type":"string"}},"required":["kind"]},"annotations":{"readOnlyHint":true}}
    ]);
    for tool in tools.as_array_mut().unwrap() {
        let query = tool["name"] == "teacher_query";
        tool["inputSchema"]["additionalProperties"] = json!(false);
        tool["inputSchema"]["properties"]["request_token"] = json!({"type":"string","description":"仅使用当前请求中提供的上下文token，不复用历史值，不向用户展示"});
        tool["inputSchema"]["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("request_token"));
        if !query {
            tool["inputSchema"]["properties"]["operation_id"] = json!({"type":"string","description":"本次用户请求内此操作的唯一标识，例如speak-1。同一操作重试必须沿用原标识，不能换标识重复执行","minLength":1,"maxLength":80});
            tool["inputSchema"]["required"]
                .as_array_mut()
                .unwrap()
                .push(json!("operation_id"));
            tool["annotations"] =
                json!({"readOnlyHint":false,"idempotentHint":true,"destructiveHint":false});
        }
    }
    tools
}
fn argument<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args[name]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .with_context(|| format!("缺少参数 {name}"))
}
fn identifier(args: &Value, name: &str) -> Result<String> {
    let value = argument(args, name)?;
    ensure!(
        value.len() <= 200
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-:.".contains(c)),
        "标识格式无效：{name}"
    );
    Ok(value.into())
}
fn number(args: &Value, name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let n = match args.get(name) {
        None => default,
        Some(v) => v.as_u64().context("参数必须是非负整数")?,
    };
    ensure!((min..=max).contains(&n), "{name} 支持 {min}–{max}");
    Ok(n)
}
fn validate(name: &str, args: &Value) -> Result<()> {
    let tools = tools();
    let tool = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .context("未知内置工具")?;
    let obj = args.as_object().context("工具参数必须是对象")?;
    let schema = &tool["inputSchema"];
    for key in schema["required"].as_array().unwrap() {
        argument(args, key.as_str().unwrap())?;
    }
    for (key, value) in obj {
        let prop = schema["properties"].get(key).context("未知工具参数")?;
        if let Some(options) = prop["enum"].as_array() {
            ensure!(options.contains(value), "参数选项无效：{key}");
        }
        match prop["type"].as_str() {
            Some("string") => {
                let s = value.as_str().context("参数必须是文字")?;
                let n = s.chars().count() as u64;
                ensure!(
                    !s.contains('\0')
                        && prop["minLength"].as_u64().is_none_or(|min| n >= min)
                        && prop["maxLength"].as_u64().is_none_or(|max| n <= max),
                    "文字长度无效：{key}"
                );
            }
            Some("integer") => {
                number(
                    args,
                    key,
                    0,
                    prop["minimum"].as_u64().unwrap_or(0),
                    prop["maximum"].as_u64().unwrap_or(u64::MAX),
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}
async fn command(bridge: &Bridge, name: &str, args: &Value) -> Result<String> {
    let action = args["action"].as_str().unwrap_or("");
    Ok(match name {
        "teacher_image" => {
            crate::files::image_command(argument(args, "path")?, args["file_name"].as_str())?
        }
        "teacher_file" => {
            crate::files::command(argument(args, "path")?, args["file_name"].as_str())?
        }
        "teacher_query" => match argument(args, "kind")? {
            "task" => format!("/task {}", identifier(args, "task_id")?),
            "cron" => "/cron list".into(),
            "posture" => "/posture status".into(),
            "notices" => "/notify list".into(),
            kind @ ("tasks" | "status" | "channel" | "skills" | "mcp" | "help") => {
                format!("/{kind}")
            }
            _ => bail!("未知查询类型"),
        },
        "teacher_proxy" => {
            let country = args["country"].as_str().unwrap_or("").trim();
            let node = args["node"].as_str().unwrap_or("").trim();
            ensure!(
                !country.chars().chain(node.chars()).any(char::is_control),
                "代理参数不能含控制字符"
            );
            ensure!(
                country.is_empty() || matches!(action, "list" | "test" | "auto"),
                "此操作不支持country参数"
            );
            ensure!(node.is_empty() || action == "set", "node仅用于set操作");
            if action == "set" {
                ensure!(!node.is_empty(), "set需要node参数");
            }
            format!(
                "/proxy {action} {}",
                if action == "set" { node } else { country }
            )
            .trim_end()
            .to_owned()
        }
        "teacher_speak" => match action {
            "stop" => "/speak stop".into(),
            "say" => {
                let text = argument(args, "text")?.trim();
                ensure!(text != "stop", "播报文字不能是保留命令 stop");
                format!("/speak {text}")
            }
            _ => bail!("未知语音操作"),
        },
        "teacher_camera" => match action {
            "photo" => "/photo".into(),
            "video" => format!("/video {}", number(args, "seconds", 10, 1, 30)?),
            _ => bail!("未知摄像头操作"),
        },
        "teacher_posture" => match action {
            "check" => "/posture".into(),
            "start" => format!(
                "/posture start {}",
                number(args, "interval_seconds", 60, 30, 3600)?
            ),
            "stop" => "/posture stop".into(),
            _ => bail!("未知坐姿操作"),
        },
        "teacher_notice" => match action {
            "cancel" => format!("/notify cancel {}", identifier(args, "notice_id")?),
            "send" => {
                let text = argument(args, "text")?;
                let tablet = bridge.tablet.as_ref().context("平板网关未启用")?;
                let devices = tablet.devices_status()?;
                let id = if args.get("device_id").is_some() {
                    identifier(args, "device_id")?
                } else {
                    ensure!(devices.len() == 1, "请指定已配对平板的 device_id");
                    devices[0]["id"].as_str().context("设备ID缺失")?.into()
                };
                ensure!(devices.iter().any(|d| d["id"] == id), "设备尚未配对");
                format!("/notify send --device {id} {text}")
            }
            _ => bail!("未知通知操作"),
        },
        _ => bail!("未知内置工具"),
    })
}
fn origin(bridge: &Bridge, token: &str) -> Result<Job> {
    let j = bridge
        .store
        .builtin_origin(token)?
        .context("工具上下文已失效，请使用本次请求的 request_token")?;
    let key: Vec<String> = serde_json::from_str(&j.key)?;
    ensure!(
        key.len() == 4
            && key[0] == bridge.cfg.workspace.to_string_lossy()
            && bridge.cfg.allowed.contains(&key[1]),
        "用户或工作目录无权限"
    );
    ensure!(
        key[3].is_empty()
            || bridge.cfg.allowed_chats.is_empty()
            || bridge.cfg.allowed_chats.contains(&key[2]),
        "聊天无权限"
    );
    Ok(j)
}
fn operation_job(parent: &Job, operation: &str, text: String) -> Job {
    // Stable within the original user request, including when a transport call is retried.
    let id = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        json!([parent.id, operation]).to_string().as_bytes(),
    );
    Job {
        id: format!("builtin:{id}"),
        key: parent.key.clone(),
        message_id: parent.message_id.clone(),
        text,
        result: String::new(),
        sent: 0,
    }
}
async fn call(bridge: &Bridge, name: &str, args: &Value) -> Result<Value> {
    validate(name, args)?;
    let token = argument(args, "request_token")?;
    let parent = origin(bridge, token)?;
    let text = command(bridge, name, args).await?;
    if name == "teacher_query"
        || (name == "teacher_proxy" && !matches!(args["action"].as_str(), Some("set" | "auto")))
    {
        let mut j = parent.clone();
        j.text = text;
        let result = bridge.command(&j, Some(&parent)).await?;
        return Ok(json!({"text":result}));
    }
    let j = operation_job(&parent, argument(args, "operation_id")?, text);
    if let Some(existing) = bridge.store.command_task(&j.key, &j.id)? {
        ensure!(
            existing["text"] == j.text,
            "operation_id 已用于不同操作，请勿混用"
        );
    } else {
        bridge.store.insert(&j)?;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(195);
    loop {
        let task = bridge
            .store
            .command_task(&j.key, &j.id)?
            .context("任务不存在")?;
        let state = task["status"].as_str().unwrap_or("");
        if state == "done" || state == "replying" {
            let text = task["result"].as_str().unwrap_or("");
            ensure!(
                ![
                    "操作失败：",
                    "播报失败：",
                    "摄像头操作失败：",
                    "文件发送失败：",
                    "任务失败："
                ]
                .iter()
                .any(|s| text.starts_with(s)),
                "{text}"
            );
            if task["kind"] == "file_attachment" {
                let file: Value = serde_json::from_str(text)?;
                return Ok(
                    json!({"task_id":j.id,"execution":"completed","file_delivery":if state=="done" {"sent"} else {"pending"},"message_type":file["msg_type"],"file_name":file["file_name"],"size":file["size"],"message":"文件或图片通过飞书原生消息发送到当前聊天。pending只表示已上传并等待发送，不得宣称已送达；请查询原任务，不要重复发送。"}),
                );
            }
            if task["kind"] == "camera_media" {
                let media: Value = serde_json::from_str(text)?;
                let mut notes = Vec::new();
                if let Some(rows) = media["content"]["zh_cn"]["content"].as_array() {
                    for row in rows {
                        if let Some(tags) = row.as_array() {
                            for tag in tags {
                                if let Some(t) = tag["text"].as_str() {
                                    notes.push(t);
                                }
                            }
                        }
                    }
                }
                return Ok(
                    json!({"task_id":j.id,"execution":"completed","media_delivery":if state=="done" {"sent"} else {"pending"},"note":notes.join("\n"),"message":"图片或视频通过原命令的发送流程发到当前聊天；未向Agent提供画面，不得编造画面内容。"}),
                );
            }
            let cancelled = [
                "语音播报已停止。",
                "语音任务已取消。",
                "摄像头任务已停止。",
                "文件上传已停止。",
                "任务已由 /stop 取消。",
                "来源任务已结束，操作已取消。",
            ]
            .contains(&text);
            return Ok(
                json!({"task_id":j.id,"execution":if cancelled {"cancelled"} else {"completed"},"text":text}),
            );
        }
        if origin(bridge, token).is_err() {
            if state == "queued" {
                bridge.store.finish(&j.id, "来源任务已结束，操作已取消。")?;
            }
            bail!("来源任务已结束；已开始的操作状态请查看任务 {id}", id = j.id);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(
                json!({"task_id":j.id,"execution":state,"message":"等待超时，结果未确认。请查询此任务，不要重新执行；重试必须沿用 operation_id。"}),
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
fn content(result: Result<Value>) -> Value {
    match result {
        Ok(v) => json!({"content":[{"type":"text","text":v.to_string()}],"isError":false}),
        Err(e) => json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
    }
}
struct Endpoint {
    bridge: Arc<Bridge>,
    secret: String,
}
pub struct Server {
    listener: tokio::net::TcpListener,
    state: Arc<Endpoint>,
}
impl Server {
    pub async fn bind(bridge: Arc<Bridge>) -> Result<Self> {
        let dir = data_dir(&bridge.cfg)?.join("builtin");
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let secret = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let endpoint =
            json!({"url":format!("http://{}/rpc",listener.local_addr()?),"secret":secret});
        let temp = dir.join("endpoint.next");
        std::fs::write(&temp, endpoint.to_string())?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(temp, dir.join("endpoint.json"))?;
        Ok(Self {
            listener,
            state: Arc::new(Endpoint { bridge, secret }),
        })
    }
    pub async fn serve(self) -> Result<()> {
        let app = Router::new()
            .route("/rpc", post(http_call))
            .layer(DefaultBodyLimit::max(32 * 1024))
            .with_state(self.state);
        axum::serve(self.listener, app).await?;
        Ok(())
    }
}
async fn http_call(
    State(state): State<Arc<Endpoint>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let expected = format!("Bearer {}", state.secret);
    let actual = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if actual.len() != expected.len()
        || actual
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |a, (b, c)| a | (b ^ c))
            != 0
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(content(
        call(
            &state.bridge,
            request["name"].as_str().unwrap_or(""),
            &request["arguments"],
        )
        .await,
    )))
}
async fn remote_call(dir: &std::path::Path, params: &Value) -> Result<Value> {
    let endpoint: Value = serde_json::from_slice(
        &std::fs::read(dir.join("builtin/endpoint.json")).context("李老师桥接尚未启动")?,
    )?;
    let url = reqwest::Url::parse(argument(&endpoint, "url")?)?;
    ensure!(
        url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/rpc"
            && url.query().is_none()
            && url.fragment().is_none(),
        "内置工具地址无效"
    );
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()?
        .post(url)
        .bearer_auth(argument(&endpoint, "secret")?)
        .json(params)
        .timeout(Duration::from_secs(205))
        .send()
        .await
        .context("内置工具连接失败；副作用结果未知，请查询任务，勿重复执行")?
        .error_for_status()?
        .json()
        .await?;
    Ok(response)
}
pub async fn stdio() -> Result<()> {
    let dir = PathBuf::from(std::env::var("MR_LI_DATA_DIR").context("缺少 MR_LI_DATA_DIR")?);
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    loop {
        let mut line = Vec::new();
        let count = tokio::io::AsyncReadExt::take(&mut input, 32 * 1024 + 1)
            .read_until(b'\n', &mut line)
            .await?;
        if count == 0 {
            break;
        }
        ensure!(line.len() <= 32 * 1024, "MCP请求过大");
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
        let id = &request["id"];
        let result = match request["method"].as_str().unwrap_or("") {
            "initialize" => {
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mr-li-teacher","version":env!("CARGO_PKG_VERSION")}})
            }
            "ping" => json!({}),
            "tools/list" => json!({"tools":tools()}),
            "tools/call" => remote_call(&dir, &request["params"])
                .await
                .unwrap_or_else(|e| content(Err(e))),
            _ => {
                let reply = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unknown method"}});
                output.write_all(format!("{reply}\n").as_bytes()).await?;
                output.flush().await?;
                continue;
            }
        };
        let reply = json!({"jsonrpc":"2.0","id":id,"result":result});
        output.write_all(format!("{reply}\n").as_bytes()).await?;
        output.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::AgentHub, feishu::Feishu, store::Store};
    struct Fixture {
        dir: PathBuf,
        bridge: Arc<Bridge>,
        parent: Job,
        token: String,
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let dir = std::env::temp_dir().join(format!("mr-li-builtin-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir)?;
            let mut cfg = Config::fixture();
            cfg.data_dir = dir.clone();
            let cfg = Arc::new(cfg);
            let store = Arc::new(Store::open(std::path::Path::new(":memory:"))?);
            let parent = Job {
                id: "parent-request".into(),
                key: json!([cfg.workspace, "ou_owner", "oc_chat", "root"]).to_string(),
                message_id: "om_parent".into(),
                text: "请说话".into(),
                result: String::new(),
                sent: 0,
            };
            store.insert(&parent)?;
            store.status(&parent.id, "running")?;
            let token = store.builtin_context(&parent.id)?;
            let bridge = Arc::new(Bridge {
                cfg: cfg.clone(),
                store,
                agents: Arc::new(AgentHub::new(cfg.clone())?),
                feishu: Arc::new(Feishu::new(cfg)?),
                tablet: None,
            });
            Ok(Self {
                dir,
                bridge,
                parent,
                token,
            })
        }
        fn args(&self) -> Value {
            json!({"request_token":self.token,"operation_id":"say-1","action":"say","text":"你好\n/stop; $(touch /tmp/do-not-run)"})
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    #[tokio::test]
    async fn queued_actions_bind_origin_deduplicate_and_keep_literal_text() -> Result<()> {
        let f = Fixture::new()?;
        let args = f.args();
        let bridge = f.bridge.clone();
        let a = args.clone();
        let running = tokio::spawn(async move { call(&bridge, "teacher_speak", &a).await });
        let child = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(j) = f.bridge.store.jobs("queued")?.first() {
                    break Ok::<_, anyhow::Error>(j.clone());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert_eq!(child.key, f.parent.key);
        assert_eq!(child.message_id, f.parent.message_id);
        assert_eq!(
            child.text,
            format!("/speak {}", args["text"].as_str().unwrap())
        );
        f.bridge.store.status(&child.id, "running")?;
        f.bridge
            .store
            .finish(&child.id, "已通过 Mac mini 连接的喇叭播报。")?;
        assert_eq!(running.await??["execution"], "completed");
        assert!(
            f.bridge.store.jobs("replying")?.is_empty(),
            "tool text must not create duplicate Feishu replies"
        );
        assert_eq!(
            call(&f.bridge, "teacher_speak", &args).await?["task_id"],
            child.id
        );
        assert!(f.bridge.store.jobs("queued")?.is_empty());
        let mut changed = args.clone();
        changed["text"] = json!("另一句话");
        assert!(
            call(&f.bridge, "teacher_speak", &changed)
                .await
                .unwrap_err()
                .to_string()
                .contains("不同操作")
        );
        f.bridge.store.finish(&f.parent.id, "finished")?;
        assert!(call(&f.bridge, "teacher_speak", &args).await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn queries_are_scoped_and_expired_or_unlisted_origins_are_rejected() -> Result<()> {
        let f = Fixture::new()?;
        let other = Job {
            id: "other-task".into(),
            key: "other-scope".into(),
            text: "private-other-task".into(),
            ..f.parent.clone()
        };
        f.bridge.store.insert(&other)?;
        let result = call(
            &f.bridge,
            "teacher_query",
            &json!({"request_token":f.token,"kind":"tasks"}),
        )
        .await?;
        assert!(!result.to_string().contains("private-other-task"));
        assert!(
            call(
                &f.bridge,
                "teacher_query",
                &json!({"request_token":f.token,"kind":"task","task_id":"other-task"})
            )
            .await
            .is_err()
        );
        assert!(
            call(
                &f.bridge,
                "teacher_query",
                &json!({"request_token":"forged","kind":"status"})
            )
            .await
            .is_err()
        );
        let bad = Job {
            id: "unlisted".into(),
            key: json!([f.bridge.cfg.workspace, "ou_intruder", "oc_chat", "root"]).to_string(),
            ..f.parent.clone()
        };
        f.bridge.store.insert(&bad)?;
        f.bridge.store.status(&bad.id, "running")?;
        let token = f.bridge.store.builtin_context(&bad.id)?;
        assert!(origin(&f.bridge, &token).is_err());
        f.bridge.store.revoke_builtin_context(&f.token)?;
        assert!(origin(&f.bridge, &f.token).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn tools_validate_parameters_and_never_expose_approval_or_session_control() -> Result<()>
    {
        let f = Fixture::new()?;
        for (name, extra, expected) in [
            ("teacher_camera", json!({"action":"photo"}), "/photo"),
            ("teacher_proxy", json!({"action":"status"}), "/proxy status"),
            (
                "teacher_proxy",
                json!({"action":"list","country":"JP"}),
                "/proxy list JP",
            ),
            (
                "teacher_proxy",
                json!({"action":"test","country":"日本"}),
                "/proxy test 日本",
            ),
            (
                "teacher_proxy",
                json!({"action":"auto","country":"香港"}),
                "/proxy auto 香港",
            ),
            (
                "teacher_proxy",
                json!({"action":"set","node":"HK-1"}),
                "/proxy set HK-1",
            ),
            (
                "teacher_camera",
                json!({"action":"video","seconds":30}),
                "/video 30",
            ),
            (
                "teacher_posture",
                json!({"action":"start","interval_seconds":45}),
                "/posture start 45",
            ),
            ("teacher_posture", json!({"action":"stop"}), "/posture stop"),
            ("teacher_posture", json!({"action":"check"}), "/posture"),
            ("teacher_speak", json!({"action":"stop"}), "/speak stop"),
            (
                "teacher_notice",
                json!({"action":"cancel","notice_id":"notice-123"}),
                "/notify cancel notice-123",
            ),
        ] {
            let mut args = extra;
            args["request_token"] = json!(f.token);
            args["operation_id"] = json!("operation");
            validate(name, &args)?;
            assert_eq!(command(&f.bridge, name, &args).await?, expected);
        }
        for (name, extra) in [
            ("teacher_camera", json!({"action":"video","seconds":31})),
            ("teacher_camera", json!({"action":"video","seconds":1.5})),
            (
                "teacher_posture",
                json!({"action":"start","interval_seconds":29}),
            ),
            (
                "teacher_speak",
                json!({"action":"say","text":"字".repeat(501)}),
            ),
            (
                "teacher_speak",
                json!({"action":"say","text":"hello","chat_id":"other"}),
            ),
            (
                "teacher_notice",
                json!({"action":"send","text":"a".repeat(2001)}),
            ),
            ("teacher_query", json!({"kind":"approve"})),
        ] {
            let mut args = extra;
            args["request_token"] = json!(f.token);
            if name != "teacher_query" {
                args["operation_id"] = json!("op");
            }
            assert!(validate(name, &args).is_err());
        }
        assert!(
            command(
                &f.bridge,
                "teacher_notice",
                &json!({"action":"cancel","notice_id":"x\n/approve y"})
            )
            .await
            .is_err()
        );
        for args in [
            json!({"action":"set"}),
            json!({"action":"status","country":"JP"}),
            json!({"action":"test","node":"HK-1"}),
            json!({"action":"set","node":"HK-1\n/stop"}),
        ] {
            assert!(command(&f.bridge, "teacher_proxy", &args).await.is_err());
        }
        assert_eq!(tools().as_array().unwrap().len(), 8);
        assert!(!tools().to_string().contains("teacher_approve"));
        let config = mcp_acp(&f.bridge.cfg)?;
        assert!(!config.to_string().contains(&f.bridge.cfg.app_secret));
        Ok(())
    }
    #[tokio::test]
    async fn file_tool_deduplicates_and_reports_upload_separately_from_delivery() -> Result<()> {
        attachment_tool("teacher_file").await
    }
    #[tokio::test]
    async fn image_tool_deduplicates_and_reports_upload_separately_from_delivery() -> Result<()> {
        attachment_tool("teacher_image").await
    }
    async fn attachment_tool(tool: &str) -> Result<()> {
        let f = Fixture::new()?;
        let args = json!({"request_token":f.token,"operation_id":"file-1","path":"成果/report 1.pdf","file_name":"中文报告.pdf"});
        validate(tool, &args)?;
        let text = command(&f.bridge, tool, &args).await?;
        let request = crate::files::parse(&text)?;
        assert_eq!(request.path, "成果/report 1.pdf");
        assert_eq!(request.image, tool == "teacher_image");
        let j = operation_job(&f.parent, "file-1", text);
        f.bridge.store.insert(&j)?;
        f.bridge.store.finish_output(&j.id, "file_attachment", &json!({"msg_type":"file","content":{"file_key":"private-upload-key"},"file_name":"中文报告.pdf","size":123}))?;
        let result = call(&f.bridge, tool, &args).await?;
        assert_eq!(result["file_delivery"], "pending");
        assert_eq!(result["file_name"], "中文报告.pdf");
        assert!(!result.to_string().contains("private-upload-key"));
        f.bridge.store.status(&j.id, "done")?;
        let sent = call(&f.bridge, tool, &args).await?;
        assert_eq!(sent["task_id"], j.id);
        assert_eq!(sent["file_delivery"], "sent");
        let mut changed = args.clone();
        changed["path"] = json!("another.txt");
        assert!(call(&f.bridge, tool, &changed).await.is_err());
        let mut forged = args.clone();
        forged["chat_id"] = json!("other-chat");
        assert!(validate(tool, &forged).is_err());
        f.bridge.store.finish(&f.parent.id, "finished")?;
        assert!(call(&f.bridge, tool, &args).await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn media_status_does_not_claim_delivery_or_visual_inspection() -> Result<()> {
        let f = Fixture::new()?;
        let args = json!({"request_token":f.token,"operation_id":"photo-1","action":"photo"});
        let j = operation_job(&f.parent, "photo-1", "/photo".into());
        f.bridge.store.insert(&j)?;
        f.bridge.store.finish_output(
            &j.id,
            "camera_media",
            &json!({"msg_type":"image","content":{"image_key":"img_uploaded"}}),
        )?;
        let result = call(&f.bridge, "teacher_camera", &args).await?;
        assert_eq!(result["media_delivery"], "pending");
        assert!(!result.to_string().contains("img_uploaded"));
        f.bridge.store.status(&j.id, "done")?;
        assert_eq!(
            call(&f.bridge, "teacher_camera", &args).await?["media_delivery"],
            "sent"
        );
        assert!(f.bridge.store.jobs("queued")?.is_empty());
        Ok(())
    }
    #[tokio::test]
    async fn notice_tool_preserves_reserved_words_and_uses_owned_queue() -> Result<()> {
        let f = Fixture::new()?;
        let tablet = crate::tablet::Gateway::open(f.bridge.cfg.clone())?;
        let db = rusqlite::Connection::open(f.dir.join("tablet/teacher-devices-v1.sqlite3"))?;
        db.execute(
            "INSERT INTO devices(id,name,token) VALUES ('tablet-1','test','fake-test-token')",
            [],
        )?;
        let b = Bridge {
            cfg: f.bridge.cfg.clone(),
            store: f.bridge.store.clone(),
            feishu: f.bridge.feishu.clone(),
            agents: f.bridge.agents.clone(),
            tablet: Some(tablet),
        };
        let args = json!({"action":"send","text":"list\n/stop"});
        let cmd = command(&b, "teacher_notice", &args).await?;
        assert_eq!(cmd, "/notify send --device tablet-1 list\n/stop");
        let j = operation_job(&f.parent, "notice-1", cmd);
        let result = b.command(&j, Some(&f.parent)).await?;
        assert!(result.contains("等待平板确认送达"));
        let mut query = f.parent.clone();
        query.text = "/notify list".into();
        let listing = b.command(&query, Some(&f.parent)).await?;
        assert!(listing.contains("list"));
        query.key = json!([b.cfg.workspace, "ou_other", "oc_chat", "root"]).to_string();
        assert!(!b.command(&query, Some(&query)).await?.contains("notice-"));
        Ok(())
    }
    #[tokio::test]
    async fn loopback_endpoint_authenticates_and_queries_current_context() -> Result<()> {
        let f = Fixture::new()?;
        let server = Server::bind(f.bridge.clone()).await?;
        let url = format!("http://{}/rpc", server.listener.local_addr()?);
        let task = tokio::spawn(server.serve());
        let request =
            json!({"name":"teacher_query","arguments":{"request_token":f.token,"kind":"status"}});
        let http = reqwest::Client::builder().no_proxy().build()?;
        assert_eq!(
            http.post(&url).json(&request).send().await?.status(),
            StatusCode::UNAUTHORIZED
        );
        let result = remote_call(&f.dir, &request).await?;
        assert_eq!(result["isError"], false);
        assert!(result.to_string().contains("执行中：true"));
        let mode = std::fs::metadata(f.dir.join("builtin/endpoint.json"))?
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        task.abort();
        let _ = task.await;
        Ok(())
    }
}
