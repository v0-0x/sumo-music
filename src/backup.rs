//! Settings files: everything on the Settings page, and optionally your keys and logins and
//! your own playlists, in one file to move to another computer (Linux, Windows or macOS) or to
//! keep as a backup.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{expand_home, Config};
use crate::library::{Library, LIKED_ID};
use crate::model::{Playlist, PlaylistKind, Source, Track};

/// Format of the file; newer versions keep reading older ones.
pub const FORMAT: u32 = 1;

/// Spotify login files kept in the Spotify data folder.
pub const LOGIN_FILES: &[&str] = &["oauth.json", "oauth-webapi.json", "credentials.json"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SettingsFile {
    /// Marks the file as Sumo's (and its format).
    pub multimusic_settings: u32,
    pub app_version: String,
    pub created_at: i64,
    /// "linux", "windows" or "macos".
    pub platform: String,
    pub config: Config,
    /// Whether `config` holds keys and `logins` the Spotify login.
    #[serde(default)]
    pub has_keys: bool,
    /// Spotify login files by name.
    #[serde(default)]
    pub logins: BTreeMap<String, String>,
    /// Your own playlists and Liked Songs (synced Spotify / SoundCloud playlists come back by
    /// themselves once you are logged in).
    #[serde(default)]
    pub playlists: Vec<Playlist>,
    /// The songs in `playlists`.
    #[serde(default)]
    pub tracks: Vec<Track>,
}

/// What goes into an exported file besides the settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Include {
    pub keys: bool,
    pub playlists: bool,
}

/// Every key, token and account name in the settings.
fn key_fields(c: &mut Config) -> [&mut String; 10] {
    [
        &mut c.spotify.web_api_client_id,
        &mut c.spotify.web_api_client_secret,
        &mut c.soundcloud.oauth_token,
        &mut c.apple_music.developer_token,
        &mut c.apple_music.user_token,
        &mut c.lastfm.api_key,
        &mut c.lastfm.api_secret,
        &mut c.lastfm.session_key,
        &mut c.lastfm.username,
        &mut c.discord.app_id,
    ]
}

/// The settings with every key and login left out.
pub fn without_keys(cfg: &Config) -> Config {
    let mut cfg = cfg.clone();
    for field in key_fields(&mut cfg) {
        field.clear();
    }
    cfg
}

fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// Builds a settings file from the current settings, the Spotify login files in
/// `spotify_dir` and the library.
pub fn build(cfg: &Config, spotify_dir: &Path, lib: &Library, include: Include) -> SettingsFile {
    let config = if include.keys { cfg.clone() } else { without_keys(cfg) };
    let logins = if include.keys {
        LOGIN_FILES
            .iter()
            .filter_map(|name| Some((name.to_string(), std::fs::read_to_string(spotify_dir.join(name)).ok()?)))
            .collect()
    } else {
        BTreeMap::new()
    };
    let (playlists, tracks) = if include.playlists {
        let playlists: Vec<Playlist> = lib
            .playlists
            .iter()
            .filter(|p| matches!(p.kind, PlaylistKind::Custom | PlaylistKind::M3u | PlaylistKind::Liked))
            .cloned()
            .collect();
        let mut seen = HashSet::new();
        let tracks = playlists
            .iter()
            .flat_map(|p| p.track_ids.iter())
            .filter(|id| seen.insert(id.as_str()))
            .filter_map(|id| lib.tracks.get(id).cloned())
            .collect();
        (playlists, tracks)
    } else {
        (Vec::new(), Vec::new())
    };
    SettingsFile {
        multimusic_settings: FORMAT,
        app_version: env!("CARGO_PKG_VERSION").into(),
        created_at: crate::model::now_unix(),
        platform: platform().into(),
        config,
        has_keys: include.keys,
        logins,
        playlists,
        tracks,
    }
}

/// Writes the file (readable only by you where the system allows it: it may hold keys).
pub fn write(path: &Path, file: &SettingsFile) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("couldn't create {}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(file)?;
    crate::config::write_private(path, text).with_context(|| format!("couldn't write {}", path.display()))?;
    Ok(())
}

pub fn read(path: &Path) -> Result<SettingsFile> {
    let text = std::fs::read_to_string(path).with_context(|| format!("couldn't read {}", path.display()))?;
    let file: SettingsFile = serde_json::from_str(&text).map_err(|e| {
        if text.contains("multimusic_settings") {
            anyhow!("{} is damaged: {e}", path.display())
        } else {
            anyhow!("{} isn't a Sumo settings file", path.display())
        }
    })?;
    if file.multimusic_settings > FORMAT {
        return Err(anyhow!(
            "{} comes from a newer Sumo ({}): update this one first",
            path.display(),
            file.app_version
        ));
    }
    Ok(file)
}

/// Whether a file looks like a settings file (for files dropped on the window).
pub fn is_settings_file(path: &Path) -> bool {
    use std::io::Read;
    let lower = path.to_string_lossy().to_ascii_lowercase();
    if !lower.ends_with(".json") {
        return false;
    }
    let mut head = [0u8; 256];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    String::from_utf8_lossy(&head[..n]).contains("\"multimusic_settings\"")
}

/// The imported settings, adjusted for this computer: library and download folders that don't
/// exist here, the mpv and yt-dlp programs and the audio devices stay as they are. A file
/// without keys keeps the keys and logins you have. What decides which programs run (yt-dlp's
/// extra options) and where updates come from is never taken from a file: one someone sends
/// could otherwise run their commands or install their build.
pub fn merge_config(current: &Config, imported: &SettingsFile) -> Config {
    let mut cfg = imported.config.clone();
    if !imported.has_keys {
        let mut mine = current.clone();
        for (field, own) in key_fields(&mut cfg).into_iter().zip(key_fields(&mut mine)) {
            *field = std::mem::take(own);
        }
    }
    let folders: Vec<PathBuf> = cfg.library.folders.iter().filter(|f| f.is_dir()).cloned().collect();
    cfg.library.folders = if folders.is_empty() {
        current.library.folders.clone()
    } else {
        folders
    };
    let downloads = cfg.downloads.folder.trim();
    if !downloads.is_empty() {
        let path = expand_home(downloads);
        // A folder, or one that can be made in a folder that is there (never a file).
        let usable =
            path.is_absolute() && (path.is_dir() || (!path.exists() && path.parent().is_some_and(Path::is_dir)));
        if !usable {
            cfg.downloads.folder = current.downloads.folder.clone();
        }
    }
    cfg.playback.mpv_path = current.playback.mpv_path.clone();
    cfg.playback.audio_device = current.playback.audio_device.clone();
    cfg.downloads.ytdlp_path = current.downloads.ytdlp_path.clone();
    cfg.downloads.ytdlp_args = current.downloads.ytdlp_args.clone();
    cfg.spotify.audio_output = current.spotify.audio_output.clone();
    cfg.updates = current.updates.clone();
    cfg
}

/// Your playlists after importing `file`'s: new playlists are added, ones you already have get
/// the songs they lack, and Liked Songs gets the liked songs. Songs from files that aren't on
/// this computer are left out. Returns the playlists to save and the songs they need.
pub fn merge_playlists(lib: &Library, file: &SettingsFile) -> (Vec<Playlist>, Vec<Track>) {
    let tracks: Vec<Track> = file
        .tracks
        .iter()
        .filter(|t| t.source != Source::Local || Path::new(&t.uri).is_file())
        .cloned()
        .collect();
    let usable: HashSet<&str> = tracks
        .iter()
        .map(|t| t.id.as_str())
        .chain(lib.tracks.keys().map(String::as_str))
        .collect();
    let mut out = Vec::new();
    for p in &file.playlists {
        let ids: Vec<String> = p
            .track_ids
            .iter()
            .filter(|id| usable.contains(id.as_str()))
            .cloned()
            .collect();
        let id = if p.kind == PlaylistKind::Liked {
            LIKED_ID
        } else {
            p.id.as_str()
        };
        match lib.playlist(id) {
            Some(mine) => {
                let have: HashSet<&String> = mine.track_ids.iter().collect();
                let missing: Vec<String> = ids.into_iter().filter(|i| !have.contains(i)).collect();
                if !missing.is_empty() {
                    let mut merged = mine.clone();
                    merged.track_ids.extend(missing);
                    out.push(merged);
                }
            }
            None => out.push(Playlist {
                track_ids: ids,
                ..p.clone()
            }),
        }
    }
    (out, tracks)
}

/// Where an export goes by default: the Documents folder.
pub fn default_export_path() -> PathBuf {
    let dir = directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(Path::to_path_buf))
        .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()))
        .unwrap_or_else(std::env::temp_dir);
    let today = chrono_date(crate::model::now_unix());
    dir.join(format!("Sumo settings {today}.json"))
}

/// Settings files in Downloads, Documents and on the Desktop, newest first.
pub fn find_settings_files() -> Vec<PathBuf> {
    let Some(user) = directories::UserDirs::new() else {
        return Vec::new();
    };
    let dirs = [user.download_dir(), user.document_dir(), user.desktop_dir()];
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = dirs
        .into_iter()
        .flatten()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|entries| entries.flatten())
        .map(|e| e.path())
        .filter(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            // Files saved before the app was renamed from MultiMusic count too.
            (name.starts_with("sumo settings") || name.starts_with("multimusic settings")) && is_settings_file(p)
        })
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// "2026-10-08" for a unix time (UTC).
fn chrono_date(unix: i64) -> String {
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed_config() -> Config {
        let mut cfg = Config::default();
        cfg.lastfm.api_key = "key".into();
        cfg.lastfm.api_secret = "secret".into();
        cfg.lastfm.session_key = "session".into();
        cfg.lastfm.username = "simo".into();
        cfg.spotify.web_api_client_secret = "sp-secret".into();
        cfg.soundcloud.oauth_token = "2-123".into();
        cfg.ui.accent = [1, 2, 3];
        cfg
    }

    fn track(id: &str, source: Source, uri: &str) -> Track {
        Track {
            album_artist: String::new(),
            disc_no: None,
            id: id.into(),
            source,
            title: id.into(),
            artist: "A".into(),
            album: String::new(),
            duration_ms: 1000,
            track_no: None,
            art: None,
            uri: uri.into(),
            added_at: 0,
        }
    }

    fn playlist(id: &str, kind: PlaylistKind, ids: &[&str]) -> Playlist {
        Playlist {
            id: id.into(),
            name: id.into(),
            kind,
            remote_id: None,
            description: String::new(),
            art: None,
            track_ids: ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn exports_with_or_without_keys() {
        let dir = std::env::temp_dir().join(format!("multimusic-backup-{}", std::process::id()));
        let spotify = dir.join("spotify");
        std::fs::create_dir_all(&spotify).unwrap();
        std::fs::write(spotify.join("oauth.json"), "{\"refresh_token\":\"r\"}").unwrap();
        std::fs::write(spotify.join("other.json"), "nope").unwrap();
        let mut lib = Library::default();
        lib.tracks
            .insert("spotify:track:1".into(), track("spotify:track:1", Source::Spotify, ""));
        lib.tracks
            .insert("spotify:track:2".into(), track("spotify:track:2", Source::Spotify, ""));
        lib.playlists = vec![
            playlist("custom:1", PlaylistKind::Custom, &["spotify:track:1"]),
            playlist(LIKED_ID, PlaylistKind::Liked, &["spotify:track:2"]),
            playlist("spotify:abc", PlaylistKind::Spotify, &["spotify:track:2"]),
        ];
        let cfg = keyed_config();

        let all = build(
            &cfg,
            &spotify,
            &lib,
            Include {
                keys: true,
                playlists: true,
            },
        );
        assert_eq!(all.config, cfg);
        assert_eq!(all.logins.keys().collect::<Vec<_>>(), vec!["oauth.json"]);
        // Synced playlists come back by themselves; only your own are kept.
        assert_eq!(all.playlists.len(), 2);
        assert_eq!(all.tracks.len(), 2);

        let bare = build(
            &cfg,
            &spotify,
            &lib,
            Include {
                keys: false,
                playlists: false,
            },
        );
        assert!(bare.logins.is_empty() && bare.playlists.is_empty());
        assert_eq!(bare.config.lastfm.session_key, "");
        assert_eq!(bare.config.ui.accent, [1, 2, 3]);

        // Round trip through a file.
        let path = dir.join("Sumo settings.json");
        write(&path, &all).unwrap();
        assert!(is_settings_file(&path));
        assert_eq!(read(&path).unwrap(), all);
        std::fs::write(dir.join("x.json"), "{\"a\":1}").unwrap();
        assert!(!is_settings_file(&dir.join("x.json")));
        assert!(read(&dir.join("x.json"))
            .unwrap_err()
            .to_string()
            .contains("isn't a Sumo"));
        let mut newer = all.clone();
        newer.multimusic_settings = FORMAT + 1;
        write(&path, &newer).unwrap();
        assert!(read(&path).unwrap_err().to_string().contains("newer Sumo"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn imports_adapt_to_this_computer() {
        let here = std::env::temp_dir();
        let mut current = Config::default();
        current.library.folders = vec![here.clone()];
        current.playback.mpv_path = "C:\\\\mpv\\\\mpv.exe".into();
        current.downloads.folder = String::new();
        current.lastfm.session_key = "mine".into();

        let mut from = keyed_config();
        from.library.folders = vec![PathBuf::from("/home/someone/Music-that-is-not-here")];
        from.downloads.folder = "/home/someone/nowhere/dl".into();
        from.playback.mpv_path = "/usr/bin/mpv".into();
        from.playback.volume = 33.0;
        from.downloads.ytdlp_args = "--exec 'touch /tmp/owned'".into();
        from.updates.repo = "someone/fake-sumo".into();
        from.updates.check = false;
        let file = SettingsFile {
            multimusic_settings: FORMAT,
            app_version: "0".into(),
            created_at: 0,
            platform: "linux".into(),
            config: from,
            has_keys: true,
            logins: BTreeMap::new(),
            playlists: Vec::new(),
            tracks: Vec::new(),
        };
        let cfg = merge_config(&current, &file);
        assert_eq!(cfg.playback.volume, 33.0);
        assert_eq!(cfg.lastfm.session_key, "session");
        // Folders that aren't here and this computer's programs stay.
        assert_eq!(cfg.library.folders, vec![here.clone()]);
        assert_eq!(cfg.downloads.folder, "");
        assert_eq!(cfg.playback.mpv_path, current.playback.mpv_path);
        // Nothing in a file decides which commands run or where updates come from.
        assert_eq!(cfg.downloads.ytdlp_args, current.downloads.ytdlp_args);
        assert_eq!(cfg.updates, current.updates);

        // A file without keys keeps yours.
        let mut keyless = file.clone();
        keyless.has_keys = false;
        keyless.config = without_keys(&keyless.config);
        assert_eq!(merge_config(&current, &keyless).lastfm.session_key, "mine");

        // Folders that exist are taken.
        let mut same = file;
        same.config.library.folders = vec![here.clone()];
        same.config.downloads.folder = here.join("dl").to_string_lossy().into();
        let cfg = merge_config(&Config::default(), &same);
        assert_eq!(cfg.library.folders, vec![here.clone()]);
        assert_eq!(cfg.downloads.folder, here.join("dl").to_string_lossy());
    }

    #[test]
    fn playlists_merge_into_yours() {
        let mut lib = Library::default();
        lib.tracks
            .insert("spotify:track:1".into(), track("spotify:track:1", Source::Spotify, ""));
        lib.playlists = vec![
            playlist(LIKED_ID, PlaylistKind::Liked, &["spotify:track:1"]),
            playlist("custom:mine", PlaylistKind::Custom, &["spotify:track:1"]),
        ];
        let file = SettingsFile {
            multimusic_settings: FORMAT,
            app_version: "0".into(),
            created_at: 0,
            platform: "linux".into(),
            config: Config::default(),
            has_keys: false,
            logins: BTreeMap::new(),
            playlists: vec![
                playlist(LIKED_ID, PlaylistKind::Liked, &["spotify:track:1", "spotify:track:2"]),
                playlist("custom:mine", PlaylistKind::Custom, &["spotify:track:1"]),
                playlist(
                    "custom:new",
                    PlaylistKind::Custom,
                    &["spotify:track:2", "local:/gone.mp3"],
                ),
            ],
            tracks: vec![
                track("spotify:track:2", Source::Spotify, ""),
                track("local:/gone.mp3", Source::Local, "/gone.mp3"),
            ],
        };
        let (playlists, tracks) = merge_playlists(&lib, &file);
        assert_eq!(tracks.len(), 1);
        // Liked Songs gains the new song; an unchanged playlist isn't saved again; the new one
        // comes without the file that isn't here.
        assert_eq!(playlists.len(), 2);
        assert_eq!(playlists[0].id, LIKED_ID);
        assert_eq!(playlists[0].track_ids, vec!["spotify:track:1", "spotify:track:2"]);
        assert_eq!(playlists[1].id, "custom:new");
        assert_eq!(playlists[1].track_ids, vec!["spotify:track:2"]);
    }

    #[test]
    fn dates() {
        assert_eq!(chrono_date(0), "1970-01-01");
        assert_eq!(chrono_date(1_791_417_600), "2026-10-08");
        assert_eq!(chrono_date(951_782_400), "2000-02-29");
    }
}
