//! Find albums in the local artist/album directory layout.
use super::{SearchResponse, local, playlist::Queue, storefront_code};
use anyhow::{Context, Result, bail, ensure};
use reqwest::Client;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

pub async fn resolve(
    query: &str,
    root: &Path,
    files: Vec<PathBuf>,
    storefront: &str,
) -> Result<Queue> {
    let mut albums = index(root, files);
    let query = query.trim();
    let (artist, title) = query
        .split_once('/')
        .or_else(|| query.split_once('的'))
        .unwrap_or(("", query));
    let title = title.trim().trim_matches(['《', '》', '「', '」']);
    let direct = select(&albums, query, "");
    let local = if direct.is_empty() {
        select(&albums, title, artist.trim())
    } else {
        direct
    };
    if !local.is_empty() {
        let key = one_album(&local)?;
        return Ok(queue(&mut albums, root, &key, &key.1));
    }

    let region = storefront_code(storefront)?;
    let client = Client::builder().timeout(Duration::from_secs(8)).build()?;
    let term = query.replace('/', " ");
    let albums_found: SearchResponse<CatalogAlbum> = client
        .get("https://itunes.apple.com/search")
        .query(&[
            ("term", term.trim()),
            ("country", &region),
            ("media", "music"),
            ("entity", "album"),
            ("limit", "1"),
        ])
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("Apple 专辑搜索失败")?
        .json()
        .await
        .context("无法读取 Apple 专辑搜索结果")?;
    let album = albums_found
        .results
        .first()
        .with_context(|| format!("Apple 搜索没有找到专辑「{query}」"))?;
    let id = album.collection_id.to_string();
    let track_results: SearchResponse<CatalogItem> = client
        .get("https://itunes.apple.com/lookup")
        .query(&[
            ("id", id.as_str()),
            ("country", region.as_str()),
            ("entity", "song"),
        ])
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("Apple 专辑曲目查询失败")?
        .json()
        .await
        .context("无法读取 Apple 专辑曲目")?;
    let tracks = catalog_tracks(album.collection_id, track_results.results);
    let choices = match_tracks(&albums, &tracks);
    ensure!(
        !choices.is_empty(),
        "Apple 找到了「{}」，本地资料库没有匹配的专辑文件。",
        album.collection_name
    );
    let key = one_album(&choices)?;
    Ok(queue(&mut albums, root, &key, &album.collection_name))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogAlbum {
    collection_id: u64,
    collection_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogItem {
    kind: Option<String>,
    collection_id: Option<u64>,
    disc_number: Option<u32>,
    track_number: Option<u32>,
    track_name: Option<String>,
}

fn catalog_tracks(id: u64, items: Vec<CatalogItem>) -> BTreeMap<(u32, u32), String> {
    items
        .into_iter()
        .filter_map(|item| {
            if item.kind.as_deref() != Some("song") || item.collection_id != Some(id) {
                return None;
            }
            Some((
                (item.disc_number.unwrap_or(1), item.track_number?),
                normalize(&item.track_name?),
            ))
        })
        .collect()
}

fn match_tracks<'a>(
    albums: &'a AlbumTracks,
    tracks: &BTreeMap<(u32, u32), String>,
) -> Vec<&'a (String, String)> {
    if tracks.is_empty() {
        return Vec::new();
    }
    let mut best = Vec::new();
    let mut best_count = 0;
    for (album, files) in albums {
        let evidence = files
            .iter()
            .filter_map(|(position, path)| {
                let name = tracks.get(position)?;
                (!name.is_empty() && normalize(&local::title(path)) == *name).then_some(position)
            })
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        // Require agreement across the disc/track sequence to avoid a shared song
        // (for example on a compilation) selecting an unrelated local album.
        if evidence < tracks.len().min(3) || evidence * 2 < tracks.len() || evidence < best_count {
            continue;
        }
        if evidence > best_count {
            best.clear();
            best_count = evidence;
        }
        best.push(album);
    }
    best
}

fn index(root: &Path, files: Vec<PathBuf>) -> AlbumTracks {
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
    albums
}

type AlbumTracks = BTreeMap<(String, String), Vec<((u32, u32), PathBuf)>>;

fn select<'a>(albums: &'a AlbumTracks, title: &str, artist: &str) -> Vec<&'a (String, String)> {
    let title = normalize(title);
    let artist = normalize(artist);
    albums
        .keys()
        .filter(|(local_artist, local_title)| {
            normalize(local_title) == title
                && (artist.is_empty() || normalize(local_artist) == artist)
        })
        .collect()
}

fn one_album(choices: &[&(String, String)]) -> Result<(String, String)> {
    match choices {
        [key] => Ok((*key).clone()),
        _ => {
            let names = choices
                .iter()
                .take(6)
                .map(|(artist, title)| format!("{artist} / {title}"))
                .collect::<Vec<_>>();
            bail!(
                "有多张匹配专辑，使用 /music album <歌手> / <本地专辑名> 指定：{}",
                names.join("、")
            )
        }
    }
}

fn queue(albums: &mut AlbumTracks, root: &Path, key: &(String, String), title: &str) -> Queue {
    let mut tracks = albums.remove(key).expect("selected album exists");
    tracks.sort_unstable();
    Queue {
        name: format!("{title} · {}", key.0),
        root: root.into(),
        tracks: tracks.into_iter().map(|(_, path)| path).collect(),
    }
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
        let mut albums = index(root, paths.iter().map(|path| root.join(path)).collect());
        assert!(one_album(&select(&albums, "同名", "")).is_err());
        let key = one_album(&select(&albums, "同名", "歌手甲"))?;
        let queue = queue(&mut albums, root, &key, &key.1);
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
    #[test]
    fn apple_tracks_identify_a_local_album_with_a_different_name() {
        let root = Path::new("/music");
        let files = [
            "周杰伦/Yeh, Hwei-Mei/01 以父之名.m4a",
            "周杰伦/Yeh, Hwei-Mei/02 懦夫.m4a",
            "周杰伦/Yeh, Hwei-Mei/03 晴天.m4a",
            "周杰伦/精选/03 晴天.m4a",
        ];
        let albums = index(root, files.iter().map(|path| root.join(path)).collect());
        let tracks = BTreeMap::from([
            ((1, 1), "以父之名".into()),
            ((1, 2), "懦夫".into()),
            ((1, 3), "晴天".into()),
        ]);
        assert_eq!(
            match_tracks(&albums, &tracks),
            [&("周杰伦".into(), "Yeh, Hwei-Mei".into())]
        );
    }
}
