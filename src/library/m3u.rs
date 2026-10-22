//! M3U / M3U8 playlist import and export.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::model::{now_unix, ImportedPlaylist, Source, Track};

use super::scanner;

/// One entry of an M3U file: a resolved path plus the optional `#EXTINF` hints.
#[derive(Debug, Clone, PartialEq)]
pub struct M3uEntry {
    pub path: PathBuf,
    pub title: Option<String>,
    pub duration_secs: Option<i64>,
}

pub fn parse(text: &str, base_dir: &Path) -> Vec<M3uEntry> {
    let mut entries = Vec::new();
    let mut pending: Option<(Option<i64>, Option<String>)> = None;
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            let (dur, title) = match info.split_once(',') {
                Some((d, t)) => (
                    d.split_whitespace().next().and_then(|d| d.parse().ok()),
                    Some(t.trim().to_string()),
                ),
                None => (info.trim().parse().ok(), None),
            };
            pending = Some((dur, title));
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if line.contains("://") && !line.starts_with("file://") {
            // Remote streams are not supported in playlists.
            pending = None;
            continue;
        }
        let raw = line.strip_prefix("file://").unwrap_or(line);
        let decoded = urlencoding::decode(raw)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| raw.to_string());
        let normalized = decoded.replace('\\', "/");
        let p = PathBuf::from(&normalized);
        let path = if p.is_absolute() { p } else { base_dir.join(p) };
        let (duration_secs, title) = pending.take().unwrap_or((None, None));
        entries.push(M3uEntry {
            path,
            title,
            duration_secs,
        });
    }
    entries
}

/// Imports an M3U file. Files that exist are read with their tags; tracks already in
/// `known` (by id) are reused as is.
pub fn import(path: &Path, known: &dyn Fn(&str) -> Option<Track>) -> Result<ImportedPlaylist> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let base = path.parent().unwrap_or(Path::new("/"));
    let now = now_unix();
    let mut tracks = Vec::new();
    for entry in parse(&text, base) {
        let path_str = entry.path.to_string_lossy().to_string();
        if let Some(t) = known(&Track::local_id(&path_str)) {
            tracks.push(t);
        } else if entry.path.is_file() {
            let cover = entry.path.parent().and_then(scanner::find_cover);
            tracks.push(scanner::read_track(&entry.path, cover, now));
        } else {
            tracing::debug!("m3u entry missing: {}", entry.path.display());
        }
    }
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Imported playlist".into());
    Ok(ImportedPlaylist {
        remote_id: path.to_string_lossy().to_string(),
        name,
        description: format!("Imported from {}", path.display()),
        art: tracks.iter().find_map(|t| t.art.clone()),
        tracks,
    })
}

/// Writes an extended M3U with the local tracks of a playlist.
pub fn export(path: &Path, tracks: &[Track]) -> Result<usize> {
    let mut out = String::from("#EXTM3U\n");
    let mut n = 0;
    for t in tracks.iter().filter(|t| t.source == Source::Local) {
        out.push_str(&format!(
            "#EXTINF:{},{} - {}\n{}\n",
            t.duration_ms / 1000,
            t.artist,
            t.title,
            t.uri
        ));
        n += 1;
    }
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extended_m3u() {
        let text = "#EXTM3U\n#EXTINF:123,Artist - Song\nsub/song.mp3\n\n/abs/other.flac\nhttp://radio/stream\nfile:///music/A%20B.ogg\n";
        let entries = parse(text, Path::new("/base"));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, PathBuf::from("/base/sub/song.mp3"));
        assert_eq!(entries[0].duration_secs, Some(123));
        assert_eq!(entries[0].title.as_deref(), Some("Artist - Song"));
        assert_eq!(entries[1].path, PathBuf::from("/abs/other.flac"));
        assert_eq!(entries[1].title, None);
        assert_eq!(entries[2].path, PathBuf::from("/music/A B.ogg"));
    }

    #[test]
    fn export_then_parse() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("multimusic-test-{}.m3u", std::process::id()));
        let t = Track {
            album_artist: String::new(),
            disc_no: None,
            id: "local:/x/y.flac".into(),
            source: Source::Local,
            title: "Y".into(),
            artist: "X".into(),
            album: String::new(),
            duration_ms: 61_000,
            track_no: None,
            art: None,
            uri: "/x/y.flac".into(),
            added_at: 0,
        };
        assert_eq!(export(&file, &[t]).unwrap(), 1);
        let text = std::fs::read_to_string(&file).unwrap();
        let entries = parse(&text, &dir);
        assert_eq!(entries[0].path, PathBuf::from("/x/y.flac"));
        assert_eq!(entries[0].duration_secs, Some(61));
        std::fs::remove_file(file).unwrap();
    }
}
