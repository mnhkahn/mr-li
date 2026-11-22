//! M3U playlist handling and the launchd worker that advances the afplay queue.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead, BufReader, ErrorKind, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

const SOCKET: &str = "playlist.sock";
pub const DIRECTORY: &str = "music-playlists";

#[derive(Serialize, Deserialize)]
pub struct Queue {
    pub name: String,
    pub root: PathBuf,
    pub tracks: Vec<PathBuf>,
}

fn playlist_name(requested: &str) -> &str {
    match requested {
        "我的最爱歌曲" | "最爱歌曲" | "喜爱歌曲" | "最愛歌曲" | "喜愛歌曲" | "我的最愛" => {
            "我的最爱"
        }
        name => name,
    }
}

fn files(dir: &Path) -> Result<Vec<PathBuf>> {
    let rows = match fs::read_dir(dir) {
        Ok(rows) => rows,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("无法打开本地歌单目录"),
    };
    let mut paths = Vec::new();
    for entry in rows {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("m3u8") || ext.eq_ignore_ascii_case("m3u")
                })
        {
            paths.push(entry.path());
        }
    }
    paths.sort_unstable();
    Ok(paths)
}

pub fn names(data: &Path) -> Result<String> {
    let mut names = files(&data.join(DIRECTORY))?
        .iter()
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_owned))
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return Ok("未找到本地歌单。".into());
    }
    Ok(format!("本地歌单\n{}", names.join("\n")))
}

pub fn load(requested: &str, root: &Path, data: &Path) -> Result<(Queue, usize)> {
    let dir = data.join(DIRECTORY);
    let candidates = files(&dir)?;
    let requested = playlist_name(requested);
    let path = candidates
        .iter()
        .find(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case(requested))
        })
        .with_context(|| {
            format!("没有找到本地歌单「{requested}」。用 /music playlists 查看可用歌单。")
        })?;
    let text = fs::read_to_string(path).context("无法读取 UTF-8 歌单")?;
    let entries = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let mut tracks = Vec::new();
    let mut skipped = 0;
    for line in entries {
        let track = root.join(line);
        match track.canonicalize() {
            Ok(path)
                if path.starts_with(root) && path.is_file() && super::local::audio_file(&path) =>
            {
                tracks.push(path);
            }
            _ => skipped += 1,
        }
    }
    ensure!(
        !tracks.is_empty(),
        "歌单「{requested}」没有可播放的本地歌曲"
    );
    Ok((
        Queue {
            name: requested.into(),
            root: root.into(),
            tracks,
        },
        skipped,
    ))
}

fn socket(data: &Path) -> PathBuf {
    data.join(SOCKET)
}

pub fn request(data: &Path, action: &str) -> Result<Option<String>> {
    let mut connection = match UnixStream::connect(socket(data)) {
        Ok(connection) => connection,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error).context("无法连接本地歌单播放器"),
    };
    connection.set_read_timeout(Some(Duration::from_secs(2)))?;
    writeln!(connection, "{action}")?;
    let mut result = String::new();
    BufReader::new(connection).read_line(&mut result)?;
    ensure!(!result.is_empty(), "本地歌单播放器未返回结果");
    let result = result.trim_end();
    if let Some(error) = result.strip_prefix("错误：") {
        bail!("{error}");
    }
    Ok(Some(result.to_owned()))
}

pub fn remove_socket(data: &Path) -> Result<()> {
    match fs::remove_file(socket(data)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("无法清理歌单播放器套接字"),
    }
}

struct Player {
    queue: Queue,
    index: usize,
    child: Option<Child>,
    paused: bool,
}

impl Player {
    fn describe(&self) -> String {
        let state = match (&self.child, self.paused) {
            (None, _) => "歌单播放完毕",
            (Some(_), true) => "已暂停",
            (Some(_), false) => "正在播放",
        };
        format!(
            "{state} · {} · {}/{} · {}",
            crate::commands::preview(&self.queue.name, 100),
            self.index + 1,
            self.queue.tracks.len(),
            super::local::label(&self.queue.root, &self.queue.tracks[self.index])
        )
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.paused = false;
    }

    fn play_track(&mut self, start: usize) {
        self.stop();
        for index in start..self.queue.tracks.len() {
            self.index = index;
            match Command::new("/usr/bin/afplay")
                .arg(&self.queue.tracks[index])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    self.child = Some(child);
                    return;
                }
                Err(error) => tracing::warn!(%error, "无法启动歌单中的音频文件"),
            }
        }
    }

    fn advance(&mut self) -> Result<()> {
        if let Some(child) = &mut self.child
            && child.try_wait()?.is_some()
        {
            self.child = None;
            self.play_track(self.index + 1);
        }
        Ok(())
    }

    fn signal(&mut self, signal: i32, paused: bool) -> Result<()> {
        if let Some(child) = &self.child {
            let status = unsafe { libc::kill(child.id() as i32, signal) };
            if status != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return self.advance();
                }
                return Err(error).context("无法控制歌单播放器");
            }
            self.paused = paused;
        }
        Ok(())
    }

    fn command(&mut self, action: &str) -> Result<String> {
        self.advance()?;
        match action {
            "status" => {}
            "pause" => self.signal(libc::SIGSTOP, true)?,
            "play" if self.child.is_none() => self.play_track(0),
            "play" => self.signal(libc::SIGCONT, false)?,
            "next" if self.index + 1 < self.queue.tracks.len() => self.play_track(self.index + 1),
            "prev" if self.index > 0 => self.play_track(self.index - 1),
            "next" => return Ok(format!("已是歌单最后一首。{}", self.describe())),
            "prev" => return Ok(format!("已是歌单第一首。{}", self.describe())),
            _ => bail!("未知歌单播放器操作"),
        }
        Ok(self.describe())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn worker(spec: &Path) -> Result<()> {
    let queue: Queue = serde_json::from_slice(&fs::read(spec)?)?;
    ensure!(!queue.tracks.is_empty(), "歌单为空");
    let data = spec.parent().context("歌单播放文件路径无效")?;
    let listener = UnixListener::bind(socket(data))?;
    let mut player = Player {
        queue,
        index: 0,
        child: None,
        paused: false,
    };
    player.play_track(0);
    loop {
        player.advance()?;
        listener.set_nonblocking(player.child.is_some())?;
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(120));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        stream.set_write_timeout(Some(Duration::from_millis(500)))?;
        let mut action = String::new();
        if BufReader::new(&stream).read_line(&mut action).is_err() {
            continue;
        }
        if action.trim_end() == "stop" {
            player.stop();
            let _ = writeln!(stream, "已停止");
            return Ok(());
        }
        let result = player
            .command(action.trim_end())
            .unwrap_or_else(|error| format!("错误：{error}"));
        let _ = writeln!(stream, "{result}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m3u_keeps_order_and_duplicates_while_skipping_missing_tracks() -> Result<()> {
        let tmp = std::env::temp_dir().join(format!("mr-li-playlist-{}", uuid::Uuid::new_v4()));
        let root = tmp.join("media");
        let data = tmp.join("data");
        let test = (|| -> Result<()> {
            fs::create_dir_all(&root)?;
            fs::create_dir_all(data.join(DIRECTORY))?;
            fs::write(root.join("a.mp3"), [])?;
            fs::write(root.join("b.m4a"), [])?;
            fs::write(tmp.join("outside.mp3"), [])?;
            fs::write(
                data.join(DIRECTORY).join("我的最爱.m3u8"),
                "\u{feff}#EXTM3U\nb.m4a\nmissing.mp3\n../outside.mp3\na.mp3\nb.m4a\n",
            )?;
            let root = root.canonicalize()?;
            let (queue, skipped) = load("喜爱歌曲", &root, &data)?;
            assert_eq!(
                queue.tracks,
                [root.join("b.m4a"), root.join("a.mp3"), root.join("b.m4a")]
            );
            assert_eq!(skipped, 2);
            Ok(())
        })();
        let _ = fs::remove_dir_all(tmp);
        test
    }
}
