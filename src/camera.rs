//! On-demand captures from Homebridge's XiaomiGo2rtc local RTSP bridge.
use crate::config::Config;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Capture {
    Photo,
    Video(u64),
}
impl Capture {
    pub fn parse(text: &str) -> Result<Self> {
        let words: Vec<_> = text.split_whitespace().collect();
        match words.as_slice() {
            ["/photo"] | ["/posture"] => Ok(Self::Photo),
            ["/video"] => Ok(Self::Video(10)),
            ["/video", seconds] => {
                let seconds: u64 = seconds
                    .parse()
                    .context("用法：/video [秒数]，支持 1–30 秒，默认 10 秒")?;
                ensure!((1..=30).contains(&seconds), "录像时长支持 1–30 秒");
                Ok(Self::Video(seconds))
            }
            _ => anyhow::bail!("用法：/photo 获取当前照片；/video [秒数] 从现在录像，默认 10 秒"),
        }
    }
}
pub fn is_capture(text: &str) -> bool {
    matches!(text.split_whitespace().next(), Some("/photo" | "/video")) || text.trim() == "/posture"
}
pub struct Media {
    pub path: PathBuf,
    pub cover: Option<PathBuf>,
    pub duration_ms: u64,
    dir: PathBuf,
}
impl Drop for Media {
    fn drop(&mut self) {
        // Only a private UUID directory created by this operation is ever removed.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn stream_url(cfg: &Config, config: &Value) -> Result<String> {
    let platform = config["platforms"]
        .as_array()
        .and_then(|p| p.iter().find(|p| p["platform"] == "XiaomiGo2rtc"))
        .context("Homebridge 尚未配置 XiaomiGo2rtc 摄像头插件")?;
    let did = if cfg.camera_did.is_empty() {
        let cameras = platform["cameras"]
            .as_array()
            .context("请配置 MR_LI_CAMERA_DID")?;
        ensure!(
            cameras.len() == 1,
            "请用 MR_LI_CAMERA_DID 指定摄像头设备 ID"
        );
        cameras[0]["did"]
            .as_str()
            .context("摄像头设备 ID 必须是字符串")?
    } else {
        &cfg.camera_did
    };
    ensure!(
        !did.is_empty() && did.len() <= 24 && did.bytes().all(|b| b.is_ascii_digit()),
        "摄像头设备 ID 无效"
    );
    let port = platform
        .get("rtspPort")
        .map(|v| v.as_u64().unwrap_or(0))
        .unwrap_or(8554);
    ensure!(
        (1..=65535).contains(&port),
        "Homebridge 摄像头 RTSP 端口无效"
    );
    Ok(format!("rtsp://127.0.0.1:{port}/xiaomi_{did}"))
}
async fn ffmpeg(cfg: &Config, args: &[String], seconds: u64) -> Result<()> {
    let mut cmd = Command::new(&cfg.ffmpeg_bin);
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for key in [
        "FEISHU_APP_SECRET",
        "OPENCODE_SERVER_PASSWORD",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env_remove(key);
    }
    let mut child = cmd
        .spawn()
        .context("无法启动 FFmpeg，请检查 MR_LI_FFMPEG_BIN（macOS 12 需要兼容版本）")?;
    match tokio::time::timeout(Duration::from_secs(seconds), child.wait()).await {
        Ok(status) => ensure!(
            status?.success(),
            "摄像头读取失败，请确认米家账号已连接、摄像头在线，且 Homebridge 摄像头流可播放"
        ),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!("摄像头读取超时，已停止抓取；请检查摄像头网络和米家连接")
        }
    }
    Ok(())
}
fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}
// Xiaomi HEVC can begin with P frames whose reference picture has not arrived.
// Keep dropping frames until an intra frame, then retain every following frame.
// Reset timestamps so video duration excludes the wait for a decodable picture.
const READY_FRAMES: &str = "select='if(eq(pict_type,I),st(0,1),ld(0))',setpts=PTS-STARTPTS";
fn photo_args(input: &str, output: &Path, rtsp: bool) -> Vec<String> {
    let mut args = if rtsp {
        strings(&["-rtsp_transport", "tcp", "-timeout", "20000000"])
    } else {
        Vec::new()
    };
    let filter = if rtsp {
        format!("{READY_FRAMES},scale='min(1280,iw)':-2")
    } else {
        "scale='min(1280,iw)':-2".into()
    };
    args.extend(strings(&[
        "-threads",
        "1",
        "-i",
        input,
        "-map",
        "0:v:0",
        "-frames:v",
        "1",
        "-an",
        "-vf",
        &filter,
        "-threads",
        "1",
        "-c:v",
        "mjpeg",
        "-q:v",
        "3",
        "-f",
        "image2",
    ]));
    args.push(output.to_string_lossy().into_owned());
    args
}
pub async fn capture(cfg: &Config, kind: Capture) -> Result<Media> {
    let data = tokio::fs::read(&cfg.homebridge_config)
        .await
        .context("无法读取 Homebridge 配置，请检查 MR_LI_HOMEBRIDGE_CONFIG")?;
    let config: Value = serde_json::from_slice(&data).context("Homebridge 配置不是有效 JSON")?;
    let input = stream_url(cfg, &config)?;
    std::fs::create_dir_all(&cfg.data_dir)?;
    let dir = cfg
        .data_dir
        .join(format!("camera-{}", uuid::Uuid::new_v4().simple()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let video = matches!(kind, Capture::Video(_));
    let media = Media {
        path: dir.join(if video { "camera.mp4" } else { "camera.jpg" }),
        cover: video.then(|| dir.join("cover.jpg")),
        duration_ms: if let Capture::Video(s) = kind {
            s * 1000
        } else {
            0
        },
        dir,
    };
    match kind {
        Capture::Photo => ffmpeg(cfg, &photo_args(&input, &media.path, true), 40).await?,
        Capture::Video(seconds) => {
            let mut args = strings(&[
                "-rtsp_transport",
                "tcp",
                "-timeout",
                "20000000",
                "-threads",
                "1",
                "-i",
                &input,
                "-map",
                "0:v:0",
                "-t",
                &seconds.to_string(),
                "-an",
                "-vf",
                &format!("{READY_FRAMES},scale='min(960,iw)':-2"),
                "-r",
                "10",
                "-c:v",
                "libx264",
                "-threads",
                "1",
                "-preset",
                "ultrafast",
                "-crf",
                "28",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
                "-fs",
                "25165824",
            ]);
            args.push(media.path.to_string_lossy().into_owned());
            ffmpeg(cfg, &args, seconds + 40).await?;
            ffmpeg(
                cfg,
                &photo_args(
                    &media.path.to_string_lossy(),
                    media.cover.as_ref().unwrap(),
                    false,
                ),
                15,
            )
            .await?;
        }
    }
    ensure!(
        tokio::fs::metadata(&media.path).await?.len() > 0,
        "摄像头未返回有效媒体"
    );
    Ok(media)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn restrict_commands_and_homebridge_stream() -> Result<()> {
        assert_eq!(Capture::parse("/video")?, Capture::Video(10));
        assert_eq!(Capture::parse("/video 30")?, Capture::Video(30));
        for text in [
            "/video 0",
            "/video 31",
            "/video -i bad",
            "/photo http://example.com",
        ] {
            assert!(Capture::parse(text).is_err());
        }
        let mut cfg = Config::fixture();
        cfg.camera_did = "123".into();
        let config = json!({"platforms":[{"platform":"XiaomiGo2rtc","rtspPort":8554}]});
        assert_eq!(
            stream_url(&cfg, &config)?,
            "rtsp://127.0.0.1:8554/xiaomi_123"
        );
        cfg.camera_did = "123/../../bad".into();
        assert!(stream_url(&cfg, &config).is_err());
        assert!(stream_url(&Config::fixture(), &config).is_err());
        Ok(())
    }
}
