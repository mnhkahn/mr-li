use anyhow::{Context, Result, bail};
use std::{env, path::PathBuf, time::Duration};
#[derive(Clone)]
pub struct Config {
    pub app_id: String,
    pub app_secret: String,
    pub feishu_base_url: String,
    pub allowed: Vec<String>,
    pub allowed_chats: Vec<String>,
    pub opencode_url: String,
    pub password: String,
    pub username: String,
    pub workspace: PathBuf,
    pub data_dir: PathBuf,
    pub model: Option<String>,
    pub agent: String,
    pub task_timeout: Duration,
    pub opencode_bin: String,
    pub free_only: bool,
    pub direct_network: bool,
    pub homebridge_config: PathBuf,
    pub camera_did: String,
    pub ffmpeg_bin: String,
}
impl Config {
    pub fn load(enrollment: bool) -> Result<Self> {
        let env_path = env::var("MR_LI_ENV").unwrap_or_else(|_| ".env".into());
        let values = if std::path::Path::new(&env_path).exists() {
            dotenvy::from_path_iter(&env_path)
                .context("无法读取环境配置")?
                .collect::<std::result::Result<std::collections::HashMap<_, _>, _>>()
                .map_err(|_| anyhow::anyhow!("环境配置语法错误，请检查 .env"))?
        } else {
            std::collections::HashMap::new()
        };
        let get = |k: &str, d: &str| {
            env::var(k)
                .ok()
                .or_else(|| values.get(k).cloned())
                .unwrap_or_else(|| d.into())
        };
        let allowed: Vec<String> = get("FEISHU_ALLOWED_OPEN_IDS", "")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if allowed.is_empty() && !enrollment {
            bail!("请配置 FEISHU_ALLOWED_OPEN_IDS（逗号分隔的用户 open_id）");
        }
        let app_secret = get("FEISHU_APP_SECRET", "");
        let feishu_base_url = match get("FEISHU_DOMAIN", "feishu").as_str() {
            "feishu" => "https://open.feishu.cn",
            "lark" => "https://open.larksuite.com",
            _ => bail!("FEISHU_DOMAIN 仅支持 feishu 或 lark"),
        }
        .to_string();
        let password = get("OPENCODE_SERVER_PASSWORD", "");
        if app_secret.is_empty() || password.is_empty() {
            bail!("密钥不能为空");
        }
        let workspace = PathBuf::from(get("MR_LI_WORKSPACE", ""));
        if !workspace.is_absolute() || !workspace.is_dir() {
            bail!("MR_LI_WORKSPACE 必须是存在的绝对目录");
        }
        let opencode_url = get("OPENCODE_URL", "http://127.0.0.1:4096");
        let u = reqwest::Url::parse(&opencode_url)?;
        if u.scheme() != "http"
            || !matches!(u.host_str(), Some("127.0.0.1" | "localhost"))
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.path() != "/"
        {
            bail!("OPENCODE_URL 仅支持本机 HTTP 根地址，凭证请独立配置");
        }
        let timeout: u64 = get("MR_LI_TASK_TIMEOUT_SECONDS", "1800").parse()?;
        if timeout < 10 {
            bail!("任务超时至少为 10 秒");
        }
        let model = Some(get("OPENCODE_MODEL", "opencode/glm-5.3-flash-free"))
            .filter(|s| !s.is_empty())
            .or_else(|| Some("opencode/glm-5.3-flash-free".into()));
        if let Some(m) = &model {
            anyhow::ensure!(
                m.split_once('/').is_some(),
                "OPENCODE_MODEL 格式应为 provider/model"
            );
        }
        Ok(Self {
            app_id: get("FEISHU_APP_ID", "cli_aa9b39cf2f789eef"),
            app_secret,
            feishu_base_url,
            allowed,
            allowed_chats: get("FEISHU_ALLOWED_CHAT_IDS", "")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            opencode_url: opencode_url.trim_end_matches('/').into(),
            password,
            username: get("OPENCODE_SERVER_USERNAME", "opencode"),
            workspace,
            data_dir: PathBuf::from(get("MR_LI_DATA_DIR", ".data")),
            model,
            agent: get("OPENCODE_AGENT", "build"),
            task_timeout: Duration::from_secs(timeout),
            opencode_bin: get("OPENCODE_BIN", "opencode"),
            homebridge_config: PathBuf::from(get(
                "MR_LI_HOMEBRIDGE_CONFIG",
                &format!(
                    "{}/.homebridge-server/config.json",
                    env::var("HOME").unwrap_or_default()
                ),
            )),
            camera_did: get("MR_LI_CAMERA_DID", ""),
            ffmpeg_bin: get("MR_LI_FFMPEG_BIN", "ffmpeg"),
            free_only: match get("MR_LI_FREE_ONLY", "true").as_str() {
                "true" => true,
                "false" => false,
                _ => bail!("MR_LI_FREE_ONLY 必须是 true 或 false"),
            },
            direct_network: match get("MR_LI_DIRECT_NETWORK", "true").as_str() {
                "true" => true,
                "false" => false,
                _ => bail!("MR_LI_DIRECT_NETWORK 必须是 true 或 false"),
            },
        })
    }
}

#[cfg(test)]
impl Config {
    pub fn fixture() -> Self {
        Self {
            app_id: "app".into(),
            app_secret: "test-only".into(),
            feishu_base_url: "https://open.feishu.cn".into(),
            allowed: vec!["ou_owner".into()],
            allowed_chats: vec![],
            opencode_url: "http://127.0.0.1:4096".into(),
            password: "test-only".into(),
            username: "opencode".into(),
            workspace: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            model: None,
            agent: "build".into(),
            task_timeout: Duration::from_secs(30),
            opencode_bin: "opencode".into(),
            free_only: true,
            direct_network: true,
            homebridge_config: std::env::temp_dir().join("mr-li-no-homebridge.json"),
            camera_did: String::new(),
            ffmpeg_bin: "ffmpeg".into(),
        }
    }
}
