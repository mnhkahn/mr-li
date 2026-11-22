use anyhow::{Context, Result, bail};
use std::{env, path::PathBuf, time::Duration};
#[derive(Clone)]
pub struct Config {
    pub music_storefront: String,
    pub home_assistant: Option<crate::home_assistant::Settings>,
    pub tablet_listen: String,
    pub tablet_public_url: String,
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
    pub homework_cli: String,
    pub model: Option<String>,
    pub agent: String,
    pub task_timeout: Duration,
    pub opencode_bin: String,
    pub antigravity_bin: String,
    pub kilo_bin: String,
    pub trae_bin: String,
    pub cline_bin: String,
    pub cline_model: String,
    pub free_only: bool,
    pub direct_network: bool,
    pub homebridge_config: PathBuf,
    pub camera_did: String,
    pub ffmpeg_bin: String,
    pub posture_auto_frame: bool,
    pub pose_bin: String,
    pub pose_model: PathBuf,
    pub pose_detector: Option<PathBuf>,
    pub pose_roi: String,
    pub photo_archive_dir: PathBuf,
    pub speech_program: String,
    pub speak_voice: String,
    pub speak_audio_device: String,
    pub posture_speech: bool,
    pub posture_voice: String,
    pub posture_audio_device: String,
    pub voice: crate::voice::Settings,
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
            music_storefront: get(
                crate::music::STOREFRONT_KEY,
                crate::music::DEFAULT_STOREFRONT,
            ),
            voice: crate::voice::Settings::from_settings(get),
            home_assistant: crate::home_assistant::Settings::from_settings(get, &env_path)?,
            tablet_listen: get("MR_LI_TABLET_LISTEN", "127.0.0.1:8787"),
            tablet_public_url: get("MR_LI_TABLET_PUBLIC_URL", ""),
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
            homework_cli: env::current_exe()
                .ok()
                .map(|path| path.with_file_name("cyeam-homework-status"))
                .filter(|path| path.is_file())
                .map(|path| path.to_string_lossy().into_owned())
                .map_or_else(
                    || get("MR_LI_HOMEWORK_CLI", "cyeam"),
                    |path| get("MR_LI_HOMEWORK_CLI", &path),
                ),
            model,
            agent: get("OPENCODE_AGENT", "build"),
            task_timeout: Duration::from_secs(timeout),
            opencode_bin: get("OPENCODE_BIN", "opencode"),
            antigravity_bin: get("MR_LI_ANTIGRAVITY_BIN", "agy"),
            kilo_bin: get("MR_LI_KILO_BIN", "kilo"),
            trae_bin: get("MR_LI_TRAE_BIN", "traecli"),
            cline_bin: get("MR_LI_CLINE_BIN", "cline"),
            cline_model: get("MR_LI_CLINE_MODEL", ""),
            homebridge_config: PathBuf::from(get(
                "MR_LI_HOMEBRIDGE_CONFIG",
                &format!(
                    "{}/.homebridge-server/config.json",
                    env::var("HOME").unwrap_or_default()
                ),
            )),
            camera_did: get("MR_LI_CAMERA_DID", ""),
            ffmpeg_bin: get("MR_LI_FFMPEG_BIN", "ffmpeg"),
            posture_auto_frame: get("MR_LI_POSTURE_AUTO_FRAME", "true")
                .parse()
                .context("MR_LI_POSTURE_AUTO_FRAME 必须是 true 或 false")?,
            pose_bin: get("MR_LI_POSE_BIN", "bin/mr-li-pose"),
            pose_model: PathBuf::from(get(
                "MR_LI_POSE_MODEL",
                ".data/pose-models/rtmpose-m-halpe26.onnx",
            )),
            pose_detector: Some(get(
                "MR_LI_POSE_DETECTOR",
                ".data/pose-models/yolox-tiny-person.onnx",
            ))
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
            pose_roi: get("MR_LI_POSE_ROI", ""),
            photo_archive_dir: PathBuf::from(get("MR_LI_PHOTO_ARCHIVE_DIR", "photos")),
            speech_program: get("MR_LI_SPEECH_PROGRAM", "/usr/bin/say"),
            speak_voice: get(
                "MR_LI_SPEAK_VOICE",
                &get("MR_LI_POSTURE_VOICE", "Ting-Ting"),
            ),
            speak_audio_device: get(
                "MR_LI_SPEAK_AUDIO_DEVICE",
                &get("MR_LI_POSTURE_AUDIO_DEVICE", "Built-in Output"),
            ),
            posture_speech: match get("MR_LI_POSTURE_SPEECH", "true").as_str() {
                "true" => true,
                "false" => false,
                _ => bail!("MR_LI_POSTURE_SPEECH 必须是 true 或 false"),
            },
            posture_voice: get("MR_LI_POSTURE_VOICE", "Ting-Ting"),
            posture_audio_device: get("MR_LI_POSTURE_AUDIO_DEVICE", "Built-in Output"),
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
            voice: crate::voice::Settings::from_settings(|_, d| d.into()),
            home_assistant: None,
            music_storefront: crate::music::DEFAULT_STOREFRONT.into(),
            tablet_listen: "127.0.0.1:8787".into(),
            tablet_public_url: String::new(),
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
            homework_cli: "cyeam".into(),
            model: None,
            agent: "build".into(),
            task_timeout: Duration::from_secs(30),
            opencode_bin: "opencode".into(),
            antigravity_bin: "agy".into(),
            kilo_bin: "kilo".into(),
            trae_bin: "traecli".into(),
            cline_bin: "cline".into(),
            cline_model: String::new(),
            free_only: true,
            direct_network: true,
            homebridge_config: std::env::temp_dir().join("mr-li-no-homebridge.json"),
            camera_did: String::new(),
            ffmpeg_bin: "ffmpeg".into(),
            posture_auto_frame: false,
            pose_bin: "mr-li-pose".into(),
            pose_model: PathBuf::from("rtmpose-m-halpe26.onnx"),
            pose_detector: None,
            pose_roi: String::new(),
            photo_archive_dir: std::env::temp_dir().join("mr-li-test-photo-archive"),
            speech_program: "/usr/bin/say".into(),
            speak_voice: "Ting-Ting".into(),
            speak_audio_device: "Built-in Output".into(),
            posture_speech: false,
            posture_voice: "Ting-Ting".into(),
            posture_audio_device: "Built-in Output".into(),
        }
    }
}
