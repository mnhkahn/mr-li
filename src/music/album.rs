//! Find albums in the local artist/album directory layout.
use super::playlist::Queue;
use anyhow::{Result, bail};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub fn load(query: &str, root: &Path, files: Vec<PathBuf>) -> Result<Queue> {
    let mut albums: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for path in files {
        let Some(relative) = path.strip_prefix(root).ok().and_then(Path::parent) else {
            continue;
        };
        let mut directories = relative.components();
        let (Some(artist), Some(album)) = (directories.next(), directories.next()) else {
            continue;
        };
        let key = (
            artist.as_os_str().to_string_lossy().into_owned(),
            album.as_os_str().to_string_lossy().into_owned(),
        );
        let disc = directories
            .filter_map(|part| disc_number(&part.as_os_str().to_string_lossy()))
            .next();
        let order = track_order(&path, disc);
        albums.entry(key).or_default().push((order, path));
    }
    let query = query.trim();
    let mut candidates = select(&albums, query, "");
    if candidates.is_empty()
        && let Some((artist, title)) = query.split_once('/').or_else(|| query.split_once('的'))
    {
        candidates = select(
            &albums,
            title.trim().trim_matches(['《', '》']),
            artist.trim(),
        );
    }
    match candidates.as_slice() {
        [key] => {
            let key = (*key).clone();
            let mut tracks = albums.remove(&key).expect("selected album exists");
            tracks.sort_unstable();
            Ok(Queue {
                name: format!("{} · {}", key.1, key.0),
                root: root.into(),
                tracks: tracks.into_iter().map(|(_, path)| path).collect(),
            })
        }
        [] => bail!("未找到本地专辑「{query}」。请使用本地资料库显示的专辑名。"),
        choices => {
            let names = choices
                .iter()
                .take(6)
                .map(|(artist, title)| format!("{artist} / {title}"))
                .collect::<Vec<_>>();
            bail!(
                "有多张匹配专辑，使用 /music album <歌手> / <专辑名> 指定：{}",
                names.join("、")
            )
        }
    }
}

type AlbumTracks = BTreeMap<(String, String), Vec<((u32, u32), PathBuf)>>;

fn select<'a>(albums: &'a AlbumTracks, title: &str, artist: &str) -> Vec<&'a (String, String)> {
    let title = normalize(title);
    let artist = normalize(artist);
    if title.is_empty() {
        return Vec::new();
    }
    let mut exact = Vec::new();
    let mut partial = Vec::new();
    for entry in albums.keys() {
        if !normalize(&entry.0).contains(&artist) {
            continue;
        }
        let name = normalize(&entry.1);
        if name == title {
            exact.push(entry);
        } else if name.contains(&title) {
            partial.push(entry);
        }
    }
    if exact.is_empty() { partial } else { exact }
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn leading_number(value: &str) -> Option<(u32, &str)> {
    let rest = value.trim_start_matches(|c: char| c.is_ascii_digit());
    let digits = &value[..value.len() - rest.len()];
    Some((digits.parse().ok()?, rest))
}

fn disc_number(directory: &str) -> Option<u32> {
    let lower = directory.to_ascii_lowercase();
    let number = ["cd", "disc", "disk"]
        .iter()
        .find_map(|prefix| lower.strip_prefix(prefix))?;
    let (disc, rest) = leading_number(number.trim_start_matches([' ', '-', '_']))?;
    rest.trim().is_empty().then_some(disc)
}

fn track_order(path: &Path, folder_disc: Option<u32>) -> (u32, u32) {
    let name = path.file_stem().unwrap_or_default().to_string_lossy();
    let Some((first, rest)) = leading_number(&name) else {
        return (folder_disc.unwrap_or(1), u32::MAX);
    };
    if let Some((track, tail)) = rest.strip_prefix('-').and_then(leading_number)
        && tail.starts_with([' ', '.', '-', '_'])
    {
        return (first, track);
    }
    let track = if rest.is_empty() || rest.starts_with([' ', '.', '-', '_']) {
        first
    } else {
        u32::MAX
    };
    (folder_disc.unwrap_or(1), track)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn album_queue_orders_discs_and_tracks_and_disambiguates_artist() -> Result<()> {
        let root = Path::new("/music");
        let paths = [
            "歌手甲/同名/CD 2/01 Finale.mp3",
            "歌手甲/同名/1-10 Ten.m4a",
            "歌手乙/同名/01 Other.m4a",
            "歌手甲/同名/2 Opening.m4a",
        ];
        let files = || paths.iter().map(|path| root.join(path)).collect();
        assert!(load("同名", root, files()).is_err());
        let queue = load("歌手甲 / 同名", root, files())?;
        assert_eq!(
            queue.tracks,
            [
                root.join(paths[3]),
                root.join(paths[1]),
                root.join(paths[0])
            ]
        );
        Ok(())
    }
}
