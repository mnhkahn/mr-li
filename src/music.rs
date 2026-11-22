mod local;

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Url};
use serde::Deserialize;
use std::{path::Path, time::Duration};

pub const STOREFRONT_KEY: &str = "MR_LI_MUSIC_STOREFRONT";
pub const DEFAULT_STOREFRONT: &str = "HK";

pub const MEDIA_DIR_KEY: &str = "MR_LI_MUSIC_MEDIA_DIR";

pub fn local_setting(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .or_else(|| {
            let path = std::env::var("MR_LI_ENV").unwrap_or_else(|_| ".env".into());
            dotenvy::from_path_iter(path).ok()?.find_map(|entry| {
                let (name, value) = entry.ok()?;
                (name == key).then_some(value)
            })
        })
        .unwrap_or_else(|| default.into())
}

const USAGE: &str = "/music status 查看播放状态\n/music play [歌曲名或资料库ID] 开始或恢复播放，也可点播\n/music artist <歌手> 随机播放资料库中这位歌手的一首歌\n/music random 随机播放资料库中的一首歌\n/music pause 暂停\n/music next 下一首\n/music prev 上一首\n/music search <关键词> 在线搜歌\n/music list [页码] 列出资料库歌曲（每页 15 首）\n/music find <关键词> 搜索资料库\n/music open <Apple Music 歌曲链接> 在音乐 App 打开歌曲页";

pub fn voice_command(text: &str) -> Option<String> {
    let text = text
        .trim()
        .trim_end_matches(['。', '！', '!', '？', '?'])
        .trim_end();
    if matches!(
        text,
        "给我唱首歌"
            | "给我唱一首歌"
            | "唱首歌"
            | "唱一首歌"
            | "随便放一首"
            | "随便放一首歌"
            | "随机播放一首歌"
    ) {
        return Some("random".into());
    }
    let artist = ["播放一首", "放一首", "给我播放一首", "给我放一首"]
        .into_iter()
        .find_map(|prefix| text.strip_prefix(prefix))?;
    let artist = artist
        .strip_suffix("的歌曲")
        .or_else(|| artist.strip_suffix("的歌"))?
        .trim();
    if artist.is_empty()
        || artist.chars().count() > 40
        || artist
            .chars()
            .any(|c| !(c.is_alphanumeric() || matches!(c, ' ' | '·' | '-' | '\'')))
    {
        return None;
    }
    Some(format!("artist {artist}"))
}

pub async fn command(args: &str, storefront: &str, media_dir: &str, data: &Path) -> Result<String> {
    let (action, arg) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let arg = arg.trim();
    ensure!(
        arg.chars().count() <= 1024 && !arg.chars().any(char::is_control),
        "音乐命令参数过长或含控制字符"
    );

    let player = |action: &str, arg: &str| {
        let action = action.to_owned();
        let arg = arg.to_owned();
        async move {
            if media_dir.is_empty() || action == "open" {
                return music_app(&action, &arg).await;
            }
            let root = Path::new(media_dir).to_path_buf();
            let data = data.to_path_buf();
            tokio::task::spawn_blocking(move || local::command(&action, &arg, &root, &data)).await?
        }
    };
    match action {
        "" | "help" if arg.is_empty() => Ok(USAGE.into()),
        "search" | "find" | "artist" => {
            ensure!(
                (1..=200).contains(&arg.chars().count()),
                "请输入 1–200 字的搜索关键词"
            );
            if action == "search" {
                search(arg, storefront).await
            } else {
                player(action, arg).await
            }
        }
        "status" | "pause" | "next" | "prev" | "random" if arg.is_empty() => {
            player(action, "").await
        }
        "play" if !media_dir.is_empty() => player(action, arg).await,
        "play" => {
            ensure!(
                arg.is_empty() || (arg.len() == 16 && arg.bytes().all(|c| c.is_ascii_hexdigit())),
                "请使用 /music list 或 /music find 返回的 16 位资料库歌曲 ID"
            );
            player(action, &arg.to_ascii_uppercase()).await
        }
        "list" => {
            let page: u32 = if arg.is_empty() {
                1
            } else {
                arg.parse().context("页码必须为正整数")?
            };
            ensure!((1..=1_000_000).contains(&page), "页码须在 1–1000000 之间");
            player(action, &page.to_string()).await
        }
        "open" => {
            let url = song_url(arg).context("请提供有效的 Apple Music 歌曲链接")?;
            player(action, &url.as_str().replacen("https:", "music:", 1)).await
        }
        _ => bail!("{USAGE}"),
    }
}

fn song_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("music.apple.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let mut path = url.path_segments()?;
    storefront_code(path.next()?).ok()?;
    let kind = path.next()?;
    let id = path.next_back()?;
    if !digits(id) || path.count() > 1 {
        return None;
    }
    let mut song_ids = url.query_pairs().filter(|(key, _)| key == "i");
    match kind {
        "album"
            if song_ids.next().is_some_and(|(_, id)| digits(&id)) && song_ids.next().is_none() =>
        {
            Some(url)
        }
        "song" if song_ids.next().is_none() => Some(url),
        _ => None,
    }
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn storefront_code(region: &str) -> Result<String> {
    ensure!(
        region.len() == 2 && region.bytes().all(|byte| byte.is_ascii_alphabetic()),
        "MR_LI_MUSIC_STOREFRONT 应为两位地区代码，例如 HK、TW 或 US"
    );
    Ok(region.to_ascii_uppercase())
}

#[derive(Deserialize)]
struct SearchResponse {
    results: Vec<Song>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Song {
    track_name: String,
    artist_name: String,
    track_view_url: String,
}

async fn search(query: &str, storefront: &str) -> Result<String> {
    let region = storefront_code(storefront)?;
    let response: SearchResponse = Client::builder()
        .timeout(Duration::from_secs(12))
        .build()?
        .get("https://itunes.apple.com/search")
        .query(&[
            ("term", query),
            ("country", &region),
            ("media", "music"),
            ("entity", "song"),
            ("limit", "8"),
        ])
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("Apple 在线搜歌请求失败")?
        .json()
        .await
        .context("无法读取 Apple 在线搜歌结果")?;

    let mut rows = response
        .results
        .iter()
        .take(8)
        .filter_map(|song| {
            let url = song_url(&song.track_view_url)?;
            let name = crate::commands::preview(&song.track_name, 120);
            let artist = crate::commands::preview(&song.artist_name, 100);
            Some(format!("• {name} — {artist}\n{url}"))
        })
        .peekable();
    if rows.peek().is_none() {
        return Ok(format!("Apple 在线搜索（{region}）没有匹配歌曲。"));
    }
    Ok(format!(
        "Apple 在线搜索（{region}）\n{}\n用 /music open <链接> 在 Mac mini 打开歌曲页。",
        rows.collect::<Vec<_>>().join("\n")
    ))
}

async fn music_app(action: &str, arg: &str) -> Result<String> {
    ensure!(cfg!(target_os = "macos"), "此操作需要 macOS 的音乐 App");
    static PLAYER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = PLAYER.lock().await;
    let mut command = tokio::process::Command::new("/usr/bin/osascript");
    command
        .kill_on_drop(true)
        .args(["-e", include_str!("music.applescript"), "--", action, arg]);
    let output = tokio::time::timeout(Duration::from_secs(18), command.output())
        .await
        .context("音乐 App 控制超时，请检查 Mac mini 桌面登录状态和“自动化”授权")?
        .context("无法启动 macOS 音乐控制脚本")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("(-1743)") || error.contains("(-1712)") || error.contains("(-600)") {
            bail!(
                "无法连接音乐 App，请检查 Mac mini 桌面登录状态和“自动化”授权（{}）",
                error.trim()
            );
        }
        if action == "play" && !arg.is_empty() && error.contains("(-1728)") {
            bail!("资料库中找不到此歌曲 ID，请重新运行 /music list 或 /music find");
        }
        bail!("音乐 App 操作失败：{}", error.trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::{song_url, voice_command};

    #[test]
    fn song_voice_requests_match_without_redirecting_video_or_other_tasks() {
        for text in ["给我唱首歌。", "给我唱一首歌", "随便放一首！"] {
            assert_eq!(voice_command(text).as_deref(), Some("random"));
        }
        assert_eq!(
            voice_command("播放一首周杰伦的歌曲。").as_deref(),
            Some("artist 周杰伦")
        );
        for text in ["暂停视频播放", "给我唱首歌然后暂停视频", "播放一首的歌"]
        {
            assert_eq!(voice_command(text), None);
        }
    }

    #[test]
    fn opening_music_links_requires_apple_song_urls() {
        assert!(
            song_url("https://music.apple.com/hk/album/晴天/1721464893?i=1721464906&uo=4")
                .is_some()
        );
        assert!(song_url("https://music.apple.com/us/song/name/123").is_some());
        for invalid in [
            "https://music.apple.com/hk/album/name/123",
            "https://music.apple.com/hk/album/name/123?i=123&i=456",
            "https://music.apple.com/hk/playlist/pl.test?i=123",
            "https://music.apple.com.evil.test/hk/song/name/123",
            "https://user@music.apple.com/hk/song/name/123",
            "music://music.apple.com/hk/song/name/123",
        ] {
            assert!(song_url(invalid).is_none(), "{invalid}");
        }
    }
}
