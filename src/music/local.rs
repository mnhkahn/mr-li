//! Play downloaded songs via the per-user launchd domain, including when the screen is locked.
use super::{album, playlist};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use rand::seq::IndexedRandom;
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const JOB: &str = "com.cyeam.mr-li.music-file";
const PAGE_SIZE: usize = 15;

pub fn command(action: &str, arg: &str, root: &Path, data: &Path) -> Result<String> {
    ensure!(cfg!(target_os = "macos"), "本地音乐播放需要 macOS");
    let root = root.canonicalize().context("无法打开本地音乐目录")?;
    ensure!(root.is_dir(), "本地音乐目录配置无效");
    let _lock = lock_player(data)?;
    let state = data.join("music-file");

    let current = fs::read_to_string(&state).ok().map(PathBuf::from);
    if matches!(action, "status" | "pause" | "next" | "prev")
        || (action == "play" && arg.is_empty())
    {
        if let Some(result) = playlist::request(data, action)? {
            return Ok(result);
        }
    }
    if action == "playlists" {
        return playlist::names(data);
    }
    if action == "playlist" {
        let (queue, skipped) = playlist::load(arg, &root, data)?;
        return start_queue(data, &queue, skipped);
    }

    match action {
        "status" => return status(&root, current.as_deref()),
        "pause" => {
            if pid()?.is_some() {
                control("kill", &["SIGSTOP"])?;
            }
            return status(&root, current.as_deref());
        }
        "play" if arg.is_empty() => {
            if pid()?.is_some() {
                control("kill", &["SIGCONT"])?;
                return status(&root, current.as_deref());
            }
        }
        _ => {}
    }

    let mut songs = Vec::new();
    find_files(&root, &mut songs)?;
    songs.sort_unstable();
    ensure!(!songs.is_empty(), "本地音乐目录里没有可播放的歌曲");
    let selected = match action {
        "artist" => {
            let query = key(arg);
            let matches: Vec<_> = songs
                .iter()
                .filter(|path| key(&artist(&root, path)) == query)
                .collect();
            matches
                .choose(&mut rand::rng())
                .copied()
                .with_context(|| format!("本地资料库没有找到 {arg} 的歌曲"))?
        }
        "random" => songs.choose(&mut rand::rng()).unwrap(),
        "play" if arg.is_empty() => current
            .as_ref()
            .filter(|path| songs.binary_search(path).is_ok())
            .unwrap_or(&songs[0]),
        "play" => {
            let query = key(arg);
            songs
                .iter()
                .find(|p| key(&title(p)) == query)
                .or_else(|| songs.iter().find(|p| key(&title(p)).contains(&query)))
                .with_context(|| format!("本地资料库没有找到歌曲 {arg}"))?
        }
        "next" | "prev" => {
            let from = current
                .as_ref()
                .and_then(|path| songs.binary_search(path).ok());
            let index = match (action, from) {
                ("next", Some(i)) => (i + 1) % songs.len(),
                ("prev", Some(i)) => (i + songs.len() - 1) % songs.len(),
                _ => 0,
            };
            &songs[index]
        }
        "find" | "list" => {
            let query = key(arg);
            let page = if action == "list" {
                arg.parse::<usize>()?
            } else {
                1
            };
            let rows = songs
                .iter()
                .filter(|path| action == "list" || key(&label(&root, path)).contains(&query))
                .skip((page - 1) * PAGE_SIZE)
                .take(PAGE_SIZE)
                .map(|path| format!("• {}", label(&root, path)))
                .collect::<Vec<_>>();
            return Ok(if rows.is_empty() {
                "没有匹配的本地歌曲。".into()
            } else {
                format!("本地音乐 · 第 {page} 页\n{}", rows.join("\n"))
            });
        }
        _ => bail!("未知本地音乐操作"),
    };

    play(selected, &state)?;
    Ok(format!("正在播放 · {}", label(&root, selected)))
}

pub async fn play_album(query: &str, root: &Path, data: &Path, storefront: &str) -> Result<String> {
    ensure!(cfg!(target_os = "macos"), "本地音乐播放需要 macOS");
    let root = root.to_owned();
    let data = data.to_owned();
    let (root, songs, data, lock) = tokio::task::spawn_blocking(move || -> Result<_> {
        let lock = lock_player(&data)?;
        let root = root.canonicalize().context("无法打开本地音乐目录")?;
        let mut songs = Vec::new();
        find_files(&root, &mut songs)?;
        Ok((root, songs, data, lock))
    })
    .await??;
    let queue = album::resolve(query, &root, songs, storefront).await?;
    tokio::task::spawn_blocking(move || {
        let _lock = lock;
        start_queue(&data, &queue, 0)
    })
    .await?
}

fn lock_player(data: &Path) -> Result<fs::File> {
    fs::create_dir_all(data)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data.join("music-file.lock"))?;
    lock.lock_exclusive()?;
    Ok(lock)
}

fn start_queue(data: &Path, queue: &playlist::Queue, skipped: usize) -> Result<String> {
    let state = data.join("music-file");
    let worker = std::env::current_exe()?;
    let spec = data.canonicalize()?.join("music-playlist.json");
    stop_job(data)?;
    fs::write(&spec, serde_json::to_vec(queue)?)?;
    bootstrap(
        &state,
        &[
            worker.to_str().context("无法读取程序路径")?,
            "music-playlist-worker",
            spec.to_str().context("无法读取歌单播放器路径")?,
        ],
    )?;
    for _ in 0..25 {
        if let Some(result) = playlist::request(data, "status")? {
            return Ok(if skipped == 0 {
                result
            } else {
                format!("{result}（跳过 {skipped} 首不可用歌曲）")
            });
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    stop_job(data)?;
    bail!("本地歌单播放器启动超时");
}

fn find_files(dir: &Path, songs: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if kind.is_dir() {
            find_files(&path, songs)?;
        } else if kind.is_file() && audio_file(&path) {
            songs.push(path);
        }
    }
    Ok(())
}

pub fn audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "mp3" | "m4a" | "aac" | "wav" | "aif" | "aiff" | "flac" | "caf"
            )
        })
}

fn key(text: &str) -> String {
    text.trim().to_lowercase()
}

fn artist(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .and_then(|path| path.parent()?.components().next())
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn title(path: &Path) -> String {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let rest = stem.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-');
    if let Some(title) = rest.strip_prefix(' ') {
        title.trim_start().into()
    } else {
        stem.into_owned()
    }
}

pub fn label(root: &Path, path: &Path) -> String {
    let title = crate::commands::preview(&title(path), 120);
    let artist = crate::commands::preview(&artist(root, path), 100);
    if artist.is_empty() {
        title
    } else {
        format!("{title} — {artist}")
    }
}

fn launchctl() -> Command {
    Command::new("/bin/launchctl")
}

fn domain() -> String {
    format!("user/{}", unsafe { libc::geteuid() })
}

fn control(action: &str, args: &[&str]) -> Result<Option<String>> {
    let service = format!("{}/{JOB}", domain());
    let out = launchctl().arg(action).args(args).arg(service).output()?;
    if out.status.code() == Some(113) || out.status.code() == Some(3) {
        return Ok(None);
    }
    ensure!(
        out.status.success(),
        "无法{action}命令行播放器：{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

fn pid() -> Result<Option<u32>> {
    Ok(control("print", &[])?.and_then(|service| {
        service
            .lines()
            .find_map(|line| line.trim().strip_prefix("pid = ")?.parse().ok())
    }))
}

fn status(root: &Path, current: Option<&Path>) -> Result<String> {
    let Some(pid) = pid()? else {
        return Ok("已停止".into());
    };
    let out = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "state="])
        .output()?;
    if !out.status.success() {
        return Ok("已停止".into());
    }
    let state = if String::from_utf8_lossy(&out.stdout)
        .trim_start()
        .starts_with('T')
    {
        "已暂停"
    } else {
        "正在播放"
    };
    Ok(match current {
        Some(path) => format!("{state} · {}", label(root, path)),
        None => state.into(),
    })
}

fn stop_job(data: &Path) -> Result<()> {
    let _ = playlist::request(data, "stop");
    if pid()?.is_some() {
        // A paused afplay cannot handle the usual termination signal until resumed.
        control("kill", &["SIGKILL"])?;
    }
    control("bootout", &[])?;
    playlist::remove_socket(data)
}

fn bootstrap(state: &Path, args: &[&str]) -> Result<()> {
    let plist = state.with_extension("plist");
    let args = args
        .iter()
        .map(|arg| {
            let escaped = arg
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            format!("<string>{escaped}</string>")
        })
        .collect::<String>();
    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>Label</key><string>{JOB}</string>
  <key>ProgramArguments</key><array>{args}</array>
  <key>LimitLoadToSessionType</key><string>Background</string>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#
    );
    fs::write(&plist, contents)?;
    let out = launchctl()
        .arg("bootstrap")
        .arg(domain())
        .arg(&plist)
        .output()?;
    ensure!(
        out.status.success(),
        "无法启动命令行播放器：{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

fn play(path: &Path, state: &Path) -> Result<()> {
    let track = path.to_str().context("歌曲文件名无法读取")?;
    stop_job(state.parent().context("音乐数据目录无效")?)?;
    bootstrap(state, &["/usr/bin/afplay", track])?;
    std::thread::sleep(Duration::from_millis(500));
    ensure!(pid()?.is_some(), "播放器未能打开该歌曲文件");
    let temp = state.with_extension("tmp");
    fs::write(&temp, track)?;
    fs::rename(temp, state)?;
    Ok(())
}
