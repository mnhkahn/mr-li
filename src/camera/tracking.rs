use super::{
    Capture, Media, ffmpeg_command, media_dir, platform, port, read_config, stream_name,
    stream_url, strings,
};
use crate::config::Config;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, SystemTime},
};
use tokio::process::{Child, Command};

const MAX_STEPS: usize = 12;
const MOVE_WINDOW: Duration = Duration::from_secs(65);
const SETTLE: Duration = Duration::from_secs(2);
const CENTER: [f32; 2] = [0.50, 0.48];
const DEAD_ZONE: [f32; 2] = [0.10, 0.12];

#[derive(Debug, PartialEq, Eq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}
impl Direction {
    fn axis(&self) -> usize {
        match self {
            Self::Left | Self::Right => 0,
            Self::Up | Self::Down => 1,
        }
    }

    fn payload(&self, camera: &Value) -> String {
        let (key, normal, inverted) = match self {
            Self::Left => ("ptzLeft", 1, 2),
            Self::Right => ("ptzRight", 2, 1),
            Self::Up => ("ptzUp", 3, 4),
            Self::Down => ("ptzDown", 4, 3),
        };
        camera[key].as_str().filter(|value| !value.is_empty()).map_or_else(
            || json!({"operation": if camera["ptzInvert"] == true { inverted } else { normal }}).to_string(),
            str::to_owned,
        )
    }
}

#[derive(Deserialize)]
struct Detection {
    image_size: [u32; 2],
    people: Vec<[f32; 5]>,
}
impl Detection {
    fn target(&self) -> Result<Result<[f32; 2], &'static str>> {
        let [width, height] = self.image_size.map(|n| n as f32);
        ensure!(width > 0. && height > 0., "人体检测返回的画面尺寸无效");
        Ok(match self.people.as_slice() {
            [] => Err("画面中未找到人体"),
            [[left, top, right, bottom, _]]
                if (right - left) >= width * 0.025 && (bottom - top) >= height * 0.065 =>
            {
                Ok([
                    (left + right) / (2. * width),
                    (top + (bottom - top) * 0.35) / height,
                ])
            }
            [_] => Err("人体目标太小，无法可靠定位"),
            _ => Err("画面中有多个人，无法确认作业目标"),
        })
    }
}

fn direction(point: [f32; 2]) -> Option<Direction> {
    let x = (point[0] - CENTER[0]) / DEAD_ZONE[0];
    let y = (point[1] - CENTER[1]) / DEAD_ZONE[1];
    if x.abs().max(y.abs()) <= 1. {
        None
    } else if x.abs() >= y.abs() {
        Some(if x < 0. {
            Direction::Left
        } else {
            Direction::Right
        })
    } else {
        Some(if y < 0. {
            Direction::Up
        } else {
            Direction::Down
        })
    }
}

async fn locate(cfg: &Config, detector: &Path, image: &Path) -> Result<Detection> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(&cfg.pose_bin)
            .arg("--detect-only")
            .arg("--image")
            .arg(image)
            .arg("--detector")
            .arg(detector)
            .env_remove("FEISHU_APP_SECRET")
            .env_remove("OPENCODE_SERVER_PASSWORD")
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .context("本地人体检测超时")?
    .context("无法启动本地人体检测程序")?;
    ensure!(
        output.status.success(),
        "本地人体检测失败，请检查姿态程序版本和 YOLOX 模型"
    );
    serde_json::from_slice(&output.stdout).context("本地人体检测返回无效结果")
}

async fn fresh_frame(
    child: &mut Child,
    latest: &Path,
    dest: &Path,
    after: SystemTime,
    timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(timeout, async {
        loop {
            ensure!(child.try_wait()?.is_none(), "摄像头视频流已断开");
            if let Ok(meta) = tokio::fs::metadata(latest).await
                && meta.modified()? >= after
            {
                // FFmpeg writes via rename, so the detector always sees a complete JPEG.
                tokio::fs::copy(latest, dest).await?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("等待摄像头新画面超时")?
}

fn motor_ack(response: &Value) -> Result<()> {
    let reply = response["reply"].as_str().unwrap_or_default();
    let payload = reply
        .find('{')
        .and_then(|start| serde_json::from_str::<Value>(&reply[start..]).ok());
    ensure!(
        payload.is_some_and(|p| p["ret"].as_i64() == Some(0)),
        "摄像头未确认云台动作"
    );
    Ok(())
}

pub async fn reframe(cfg: &Config) -> Result<(Media, String)> {
    let detector = cfg
        .pose_detector
        .as_deref()
        .filter(|p| p.is_file())
        .context("请先安装本地 YOLOX 人体检测模型")?;
    let config = read_config(cfg).await?;
    let platform = platform(&config)?;
    let source = stream_name(cfg, platform)?;
    let did = source.trim_start_matches("xiaomi_");
    let camera = platform["cameras"]
        .as_array()
        .and_then(|cameras| cameras.iter().find(|camera| camera["did"] == did))
        .unwrap_or(&Value::Null);
    let api_port = port(platform, "apiPort", 1984)?;
    let url = format!("http://127.0.0.1:{api_port}/api/xiaomi/command");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(4))
        .build()?;
    let input = stream_url(cfg, &config)?;
    let media = media_dir(cfg, Capture::Photo)?;
    let latest = media.dir.join("live.jpg");
    let mut child = ffmpeg_command(cfg)
        .args(strings(&[
            "-rtsp_transport", "tcp", "-timeout", "20000000", "-threads", "1",
            "-i", &input, "-map", "0:v:0", "-an",
            "-vf", "select='if(eq(pict_type,I),st(0,1),ld(0))*if(isnan(prev_selected_t),1,gte(t-prev_selected_t,0.5))',scale='min(1280,iw)':-2",
            "-vsync", "vfr", "-threads", "1", "-c:v", "mjpeg", "-q:v", "3", "-f", "image2",
            "-update", "1", "-atomic_writing", "1",
        ]))
        .arg(&latest)
        .spawn().context("无法启动摄像头跟踪取帧")?;
    let deadline = tokio::time::Instant::now() + MOVE_WINDOW;
    fresh_frame(
        &mut child,
        &latest,
        &media.path,
        SystemTime::UNIX_EPOCH,
        Duration::from_secs(22),
    )
    .await?;

    let mut previous: Option<(Direction, [f32; 2])> = None;
    let mut stalled = 0;
    let mut steps = 0;
    let outcome = loop {
        let point = match locate(cfg, detector, &media.path).await?.target()? {
            Ok(point) => point,
            Err(reason) => break reason,
        };
        let Some(next) = direction(point) else {
            break "人体已在构图中心区域";
        };
        if steps == MAX_STEPS || tokio::time::Instant::now() >= deadline {
            break "已达到单次调整上限，人体仍偏离中心";
        }
        if let Some((last, old)) = &previous {
            let axis = last.axis();
            let progress = (old[axis] - CENTER[axis]).abs() - (point[axis] - CENTER[axis]).abs();
            if progress < -0.02
                || (next.axis() == axis && next != *last)
                || (0..2).any(|i| (point[i] - old[i]).abs() > 0.28)
            {
                break "目标位置变化异常，已停止调整";
            }
            stalled = if progress < 0.005 { stalled + 1 } else { 0 };
            if stalled >= 2 {
                break "连续两步未见目标靠近中心，已停止调整";
            }
        }
        let payload = next.payload(camera);
        let request = client.post(&url).query(&[
            ("src", source.as_str()),
            ("cmd", "0x112"),
            ("data", payload.as_str()),
        ]);
        let request = match (
            platform["uiUsername"].as_str(),
            platform["uiPassword"].as_str(),
        ) {
            (Some(user), Some(password)) => request.basic_auth(user, Some(password)),
            _ => request,
        };
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("云台请求失败，动作结果未知"))?;
        ensure!(
            response.status().is_success(),
            "云台接口返回 HTTP {}，已停止调整",
            response.status().as_u16()
        );
        let response: Value = response
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("云台返回无效响应"))?;
        motor_ack(&response)?;
        steps += 1;
        previous = Some((next, point));
        tokio::time::sleep(SETTLE).await;
        fresh_frame(
            &mut child,
            &latest,
            &media.path,
            SystemTime::now(),
            Duration::from_secs(4),
        )
        .await?;
    };
    let _ = child.kill().await;
    Ok((media, format!("{outcome}。云台已确认 {steps} 步。")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_uses_image_coordinates_and_a_center_dead_zone() {
        assert_eq!(direction([0.30, 0.48]), Some(Direction::Left));
        assert_eq!(direction([0.70, 0.48]), Some(Direction::Right));
        assert_eq!(direction([0.5, 0.25]), Some(Direction::Up));
        assert_eq!(direction([0.5, 0.75]), Some(Direction::Down));
        assert_eq!(direction([0.57, 0.55]), None);
        assert_eq!(
            Direction::Up.payload(&json!({"ptzInvert":true})),
            r#"{"operation":4}"#
        );
        assert_eq!(
            Direction::Right.payload(&json!({"ptzRight":"custom"})),
            "custom"
        );
        assert!(motor_ack(&json!({"reply":"\u{0}\u{1}\u{13}{\"ret\":0}"})).is_ok());
        assert!(motor_ack(&json!({"reply":"{\"ret\":-1}"})).is_err());
        assert!(motor_ack(&json!({"reply":""})).is_err());
    }

    #[test]
    fn tracking_requires_one_usable_person() -> Result<()> {
        let mut frame = Detection {
            image_size: [1000, 500],
            people: vec![],
        };
        assert!(frame.target()?.is_err());
        let person = [100., 150., 300., 350., 0.9];
        frame.people.push(person);
        assert_eq!(frame.target()?, Ok([0.20, 0.44]));
        frame.people.push([700., 180., 900., 370., 0.95]);
        assert!(frame.target()?.is_err());
        frame.people = vec![[100., 100., 105., 105., 0.95]];
        assert!(frame.target()?.is_err());
        Ok(())
    }
}
