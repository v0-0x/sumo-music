//! Local music folder scanning.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

use lofty::config::ParseOptions;
use lofty::prelude::*;
use lofty::probe::Probe;
use regex::Regex;

use crate::model::{now_unix, Source, Track};

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "m4a", "m4b", "mp4", "aac", "alac", "wav", "wave", "aif", "aiff", "aifc",
    "ape", "wv", "mpc", "wma", "mka", "dsf", "dff", "spx", "tta",
];

const COVER_NAMES: &[&str] = &[
    "cover",
    "folder",
    "front",
    "album",
    "albumart",
    "albumartsmall",
    "artwork",
];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub struct ScanResult {
    /// New or modified tracks.
    pub changed: Vec<Track>,
    /// mtimes of `changed` tracks.
    pub mtimes: HashMap<String, i64>,
    /// Local track ids that no longer exist on disk.
    pub removed: Vec<String>,
    pub total_files: usize,
}

pub fn mtime_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Scans `folders` and returns what changed compared to `known` (track id -> mtime).
/// `progress(done, total)` is called periodically from worker threads.
pub fn scan(folders: &[PathBuf], known: &HashMap<String, i64>, progress: &(dyn Fn(usize, usize) + Sync)) -> ScanResult {
    let mut files = Vec::new();
    for folder in folders {
        let entries = walkdir::WalkDir::new(folder)
            .follow_links(true)
            .into_iter()
            // Downloads in progress.
            .filter_entry(|e| {
                !e.file_name()
                    .to_string_lossy()
                    .starts_with(crate::downloader::WORK_PREFIX)
            });
        for entry in entries.flatten() {
            if entry.file_type().is_file() && is_audio_file(entry.path()) {
                files.push((entry.into_path(), folder));
            }
        }
    }
    let total = files.len();

    let mut seen = HashSet::with_capacity(total);
    let mut todo = Vec::new();
    for (path, folder) in files {
        let id = Track::local_id(&path.to_string_lossy());
        let mtime = mtime_of(&path);
        if known.get(&id) != Some(&mtime) {
            todo.push((path, folder, mtime));
        }
        seen.insert(id);
    }
    let removed: Vec<String> = known.keys().filter(|id| !seen.contains(*id)).cloned().collect();

    // Tag parsing is IO + CPU bound, spread it over a few threads.
    let done = AtomicUsize::new(total - todo.len());
    progress(done.load(Ordering::Relaxed), total);
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 4);
    let chunk = todo.len().div_ceil(threads).max(1);
    let cover_cache = std::sync::Mutex::new(HashMap::<PathBuf, Option<String>>::new());
    let now = now_unix();

    let changed: Vec<(Track, i64)> = std::thread::scope(|s| {
        let handles: Vec<_> = todo
            .chunks(chunk)
            .map(|part| {
                let done = &done;
                let cover_cache = &cover_cache;
                s.spawn(move || {
                    let mut out = Vec::with_capacity(part.len());
                    for (path, folder, mtime) in part {
                        let cover = path.parent().and_then(|dir| {
                            // The folder is read without holding the cache, so the other
                            // threads don't wait on it.
                            let cached = cover_cache.lock().unwrap().get(dir).cloned();
                            cached.unwrap_or_else(|| {
                                let found = find_cover(dir);
                                cover_cache.lock().unwrap().insert(dir.to_path_buf(), found.clone());
                                found
                            })
                        });
                        // "Added" is when the file appeared: its modification time, which for
                        // music copied or downloaded in is when it arrived.
                        let added = if *mtime > 0 { (*mtime).min(now) } else { now };
                        out.push((read_track_in(path, Some(folder.as_path()), cover, added), *mtime));
                        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if n % 50 == 0 {
                            progress(n, total);
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    progress(total, total);

    let mut mtimes = HashMap::with_capacity(changed.len());
    let changed = changed
        .into_iter()
        .map(|(t, m)| {
            mtimes.insert(t.id.clone(), m);
            t
        })
        .collect();
    ScanResult {
        changed,
        mtimes,
        removed,
        total_files: total,
    }
}

/// Reads tags of one file. Never fails: what the tags leave out is guessed from the file's name
/// (and its folder as the album).
pub fn read_track(path: &Path, cover: Option<String>, added_at: i64) -> Track {
    read_track_in(path, None, cover, added_at)
}

/// Like [`read_track`] for a file in the library folder `root`, whose folders below `root`
/// also name the artist ("Artist/Album/01 Song.mp3", "Artist - Album/Song.mp3").
pub fn read_track_in(path: &Path, root: Option<&Path>, cover: Option<String>, added_at: i64) -> Track {
    let path_str = path.to_string_lossy().to_string();
    let mut track = Track {
        album_artist: String::new(),
        disc_no: None,
        id: Track::local_id(&path_str),
        source: Source::Local,
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        duration_ms: 0,
        track_no: None,
        // No folder image: point at the audio file itself, the art loader reads embedded pictures.
        art: Some(cover.unwrap_or_else(|| path_str.clone())),
        uri: path_str,
        added_at,
    };

    let parsed = Probe::open(path)
        .map(|p| p.options(ParseOptions::new().read_cover_art(false)))
        .and_then(|p| p.read());
    match parsed {
        Ok(file) => {
            track.duration_ms = file.properties().duration().as_millis() as u64;
            if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
                let names = tag_names(tag);
                (track.title, track.artist, track.album) = (names.title, names.artist, names.album);
                track.track_no = names.track_no;
                // Albums group by their artist: a compilation's songs stay one album.
                let album_artist = tag.get_string(ItemKey::AlbumArtist).map(str::trim).unwrap_or_default();
                if !album_artist.eq_ignore_ascii_case(&track.artist) {
                    track.album_artist = album_artist.to_string();
                }
                track.disc_no = tag.disk().filter(|n| *n > 0);
            }
        }
        Err(e) => tracing::debug!("could not read tags of {}: {e}", path.display()),
    }
    let missing = track.title.is_empty() || track.artist.is_empty() || track.album.is_empty();
    if missing || track.track_no.is_none() {
        let guess = guess_from_path(path, root);
        let fill = |field: &mut String, value: String| {
            if field.is_empty() {
                *field = value;
            }
        };
        fill(&mut track.title, guess.title);
        fill(&mut track.artist, guess.artist);
        fill(&mut track.album, guess.album);
        track.track_no = track.track_no.or(guess.track_no);
    }
    if track.artist.is_empty() {
        track.artist = UNKNOWN_ARTIST.into();
    }
    track
}

/// Shown for files that name no artist anywhere.
pub const UNKNOWN_ARTIST: &str = "Unknown Artist";

/// Title, artist (or album artist) and album from a tag; empty when not set.
fn tag_names(tag: &lofty::tag::Tag) -> Guess {
    let text = |value: Option<&str>| value.map(str::trim).unwrap_or_default().to_string();
    let mut artist = text(tag.artist().as_deref());
    if artist.is_empty() {
        artist = text(tag.get_string(ItemKey::AlbumArtist));
    }
    Guess {
        title: text(tag.title().as_deref()),
        artist,
        album: text(tag.album().as_deref()),
        track_no: tag.track().filter(|n| *n > 0),
    }
}

/// Which details a file's tags leave out, so the library guessed them from its name and folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Untagged {
    /// No title or no artist.
    pub names: bool,
    pub album: bool,
}

pub fn untagged(path: &Path) -> Untagged {
    let parsed = Probe::open(path)
        .map(|p| p.options(ParseOptions::new().read_properties(false).read_cover_art(false)))
        .and_then(|p| p.read());
    let names = parsed
        .ok()
        .and_then(|file| file.primary_tag().or_else(|| file.first_tag()).map(tag_names))
        .unwrap_or_default();
    Untagged {
        names: names.title.is_empty() || names.artist.is_empty(),
        album: names.album.is_empty(),
    }
}

/// What a file's name and folders say about a song.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Guess {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub track_no: Option<u32>,
}

/// yt-dlp's video id suffix: " [dQw4w9WgXcQ]" (YouTube) or " [1234567890]" (SoundCloud).
static VIDEO_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*\[([A-Za-z0-9_-]{11}|\d{6,})\]\s*$").expect("valid regex"));
/// "01 - ", "01. ", "1-01 ", "01) ", "01 ": a leading (disc and) track number.
static TRACK_NO_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:\d[-.](\d{2})|(\d{1,3}))(\s*[-.)_]\s*|\s+)(\S.*)$").expect("valid regex"));
/// "CD1", "Disc 2": a disc folder inside an album folder.
static DISC_DIR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:cd|dis[ck])\s*\d+$").expect("valid regex"));
/// "2019 - Album": a year in front of an album folder's name.
static YEAR_PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\(?(?:19|20)\d{2}\)?\s*(?:-\s*|\.\s*|\s)").expect("valid regex"));

/// Guesses a song's details from where it is: "Artist - Title.mp3", "01 - Title.flac",
/// "Artist - 03 - Title.mp3", "bladee_-_waster.opus" and folders like "Artist/Album/" or
/// "Artist - Album (2019)/" below the library folder `root`.
pub fn guess_from_path(path: &Path, root: Option<&Path>) -> Guess {
    let stem = path.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default();
    let mut guess = guess_from_name(&stem);

    let mut dirs: Vec<String> = match root.and_then(|r| path.parent()?.strip_prefix(r).ok()) {
        Some(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect(),
        // Not in a library folder: only the folder it is in, as the album.
        None => path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| vec![n.to_string_lossy().to_string()])
            .unwrap_or_default(),
    };
    if dirs.len() > 1 && dirs.last().is_some_and(|d| DISC_DIR_RE.is_match(d.trim())) {
        dirs.pop();
    }
    let Some(album_dir) = dirs.last().map(|d| clean_folder(d)) else {
        return guess;
    };
    let (folder_artist, album) = match crate::integrations::lastfm::split_artist_title(&album_dir) {
        Some((artist, album)) => (artist, album),
        None if root.is_some() && dirs.len() > 1 => (clean_folder(&dirs[dirs.len() - 2]), album_dir),
        None => (String::new(), album_dir),
    };
    // "Artist/Artist - Song.mp3": the folder is the artist's, not an album.
    let artist_folder = folder_artist.is_empty() && same_name(&album, &guess.artist);
    if guess.artist.is_empty() {
        guess.artist = folder_artist;
    }
    if !artist_folder {
        guess.album = album;
    }
    guess
}

/// Title, artist and track number from a file name.
fn guess_from_name(stem: &str) -> Guess {
    let name = match VIDEO_ID_RE.captures(stem) {
        Some(caps) if looks_random(&caps[1]) => &stem[..caps.get(0).map_or(stem.len(), |m| m.start())],
        _ => stem,
    };
    let name = tidy(&name.replace('_', " ").replace(" – ", " - ").replace(" — ", " - "));
    let name = crate::integrations::lastfm::strip_upload_tags(&name);
    let mut guess = Guess::default();
    let mut rest = name.as_str();
    if let Some(caps) = TRACK_NO_RE.captures(&name) {
        let number = caps.get(1).or(caps.get(2)).map_or("", |m| m.as_str());
        let separator = caps.get(3).map_or("", |m| m.as_str());
        let after = caps.get(4).map_or("", |m| m.as_str());
        // "01 Song", "01 - Song", "12 Song" are numbered; "50 Cent - In Da Club" and "7 Rings"
        // are names.
        let padded = number.starts_with('0') || (number.len() == 2 && !after.contains(" - "));
        if !separator.trim().is_empty() || padded {
            guess.track_no = number.parse().ok().filter(|n| *n > 0);
            rest = after;
        }
    }
    let parts: Vec<&str> = rest.split(" - ").map(str::trim).filter(|p| !p.is_empty()).collect();
    // "Artist - 03 - Title" / "Artist - Album - 03 - Title".
    let number_at = (1..parts.len().saturating_sub(1))
        .find(|i| parts[*i].len() <= 3 && parts[*i].chars().all(|c| c.is_ascii_digit()));
    if let Some(i) = number_at {
        guess.artist = parts[0].to_string();
        guess.album = parts[1..i].join(" - ");
        guess.track_no = guess.track_no.or(parts[i].parse().ok().filter(|n| *n > 0));
        guess.title = parts[i + 1..].join(" - ");
    } else if let Some((artist, title)) = crate::integrations::lastfm::split_artist_title(rest) {
        (guess.artist, guess.title) = (artist, title);
    } else {
        guess.title = rest.to_string();
    }
    if guess.title.is_empty() {
        guess.title = stem.trim().to_string();
    }
    guess
}

/// Whether a bracketed word is a video id ("dQw4w9WgXcQ", "1234567890") rather than a word
/// ("Documentary").
fn looks_random(id: &str) -> bool {
    id.chars()
        .skip(1)
        .any(|c| c.is_ascii_digit() || c.is_ascii_uppercase() || c == '-' || c == '_')
}

/// An album folder's name without its year and release details: "Artist - Album (2019)
/// [FLAC]" -> "Artist - Album", "2019 - Album" -> "Album".
fn clean_folder(name: &str) -> String {
    let name = tidy(&name.replace('_', " "));
    let mut out = String::new();
    let mut rest = name.as_str();
    while let Some(start) = rest.find(['[', '(']) {
        let close = if rest[start..].starts_with('[') { ']' } else { ')' };
        let Some(len) = rest[start..].find(close) else { break };
        out.push_str(&rest[..start]);
        if !is_release_info(&rest[start + 1..start + len]) {
            out.push_str(&rest[start..=start + len]);
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    let out = tidy(&out);
    let out = YEAR_PREFIX_RE.replace(&out, "");
    if out.trim().is_empty() {
        name.trim().to_string()
    } else {
        tidy(&out)
    }
}

/// "2019", "FLAC", "24bit 96kHz", "WEB 320": not part of an album's name.
fn is_release_info(inner: &str) -> bool {
    const WORDS: &[&str] = &[
        "flac", "mp3", "aac", "alac", "ogg", "opus", "wav", "m4a", "320", "320kbps", "256", "v0", "v2", "kbps", "cbr",
        "vbr", "web", "cd", "vinyl", "lossless", "hi", "res", "hires", "bit", "24bit", "16bit", "24", "16", "khz",
        "44", "1", "48", "88", "96", "192",
    ];
    let mut words = inner
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .peekable();
    words.peek().is_some()
        && words.all(|w| {
            let w = w.to_ascii_lowercase();
            let year =
                w.len() == 4 && (w.starts_with("19") || w.starts_with("20")) && w.chars().all(|c| c.is_ascii_digit());
            year || WORDS.contains(&w.as_str()) || w.strip_suffix("khz").is_some_and(|n| n.parse::<u32>().is_ok())
        })
}

/// Collapses runs of whitespace (and trims).
fn tidy(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The same artist name, ignoring case and punctuation.
fn same_name(a: &str, b: &str) -> bool {
    let squash = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let a = squash(a);
    !a.is_empty() && a == squash(b)
}

/// Finds a cover image in an album folder.
pub fn find_cover(dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut fallback = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
            continue;
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if COVER_NAMES.contains(&stem.as_str()) {
            return Some(path.to_string_lossy().to_string());
        }
        if fallback.is_none() {
            fallback = Some(path.to_string_lossy().to_string());
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_audio_extensions() {
        assert!(is_audio_file(Path::new("/a/b/song.FLAC")));
        assert!(is_audio_file(Path::new("x.opus")));
        assert!(!is_audio_file(Path::new("cover.jpg")));
        assert!(!is_audio_file(Path::new("noext")));
    }

    #[test]
    fn scan_finds_new_and_removed_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let album = dir.join("Artist").join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01 Song.mp3"), b"not really audio").unwrap();
        // A download in progress is not part of the library yet.
        std::fs::create_dir_all(dir.join(".multimusic-1-1")).unwrap();
        std::fs::write(dir.join(".multimusic-1-1").join("half.mp3"), b"x").unwrap();
        std::fs::write(album.join("cover.jpg"), b"jpg").unwrap();

        let mut known = HashMap::new();
        known.insert("local:/gone.mp3".to_string(), 1);
        let result = scan(std::slice::from_ref(&dir), &known, &|_, _| {});
        assert_eq!(result.total_files, 1);
        assert_eq!(result.changed.len(), 1);
        assert_eq!(result.removed, vec!["local:/gone.mp3".to_string()]);
        let t = &result.changed[0];
        // Not really audio, so no tags: the names come from the path.
        assert_eq!((t.title.as_str(), t.track_no), ("Song", Some(1)));
        assert_eq!((t.artist.as_str(), t.album.as_str()), ("Artist", "Album"));
        assert!(t.art.as_deref().unwrap().ends_with("cover.jpg"));

        // Unchanged files are skipped on the next scan.
        let known: HashMap<String, i64> = result.mtimes.clone();
        let again = scan(std::slice::from_ref(&dir), &known, &|_, _| {});
        assert!(again.changed.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn name(stem: &str) -> (String, String, Option<u32>) {
        let g = guess_from_name(stem);
        (g.artist, g.title, g.track_no)
    }

    fn names(artist: &str, title: &str, track: Option<u32>) -> (String, String, Option<u32>) {
        (artist.to_string(), title.to_string(), track)
    }

    #[test]
    fn names_from_file_names() {
        assert_eq!(name("Bladee - Waster"), names("Bladee", "Waster", None));
        assert_eq!(
            name("01 - Bladee - Waster [Free DL]"),
            names("Bladee", "Waster", Some(1))
        );
        assert_eq!(name("bladee_-_waster"), names("bladee", "waster", None));
        assert_eq!(name("Bladee - Waster [dQw4w9WgXcQ]"), names("Bladee", "Waster", None));
        assert_eq!(name("Bladee - Waster [1234567890]"), names("Bladee", "Waster", None));
        assert_eq!(name("01 Waster"), names("", "Waster", Some(1)));
        assert_eq!(name("07. Waster"), names("", "Waster", Some(7)));
        assert_eq!(name("1-03 Waster"), names("", "Waster", Some(3)));
        assert_eq!(name("12 Waster"), names("", "Waster", Some(12)));
        assert_eq!(name("Bladee - 03 - Waster"), names("Bladee", "Waster", Some(3)));
        assert_eq!(
            name("Bladee - Waster (Official Audio)"),
            names("Bladee", "Waster", None)
        );
        // Names that start with a number, versions and plain titles.
        assert_eq!(name("50 Cent - In Da Club"), names("50 Cent", "In Da Club", None));
        assert_eq!(name("7 Rings"), names("", "7 Rings", None));
        assert_eq!(name("1999"), names("", "1999", None));
        assert_eq!(name("Waster - Live"), names("", "Waster - Live", None));
        assert_eq!(name("Waster [Documentary]"), names("", "Waster [Documentary]", None));
        assert_eq!(name("Bladee – Waster"), names("Bladee", "Waster", None));
    }

    #[test]
    fn names_from_folders() {
        let root = Path::new("/music");
        let guess = |path: &str| {
            let g = guess_from_path(Path::new(path), Some(root));
            (g.artist, g.album, g.title)
        };
        let three = |a: &str, b: &str, c: &str| (a.to_string(), b.to_string(), c.to_string());
        assert_eq!(
            guess("/music/Bladee/Eversince/01 Bladee.mp3"),
            three("Bladee", "Eversince", "Bladee")
        );
        assert_eq!(
            guess("/music/Bladee/Eversince/CD1/02 Song.mp3"),
            three("Bladee", "Eversince", "Song")
        );
        assert_eq!(
            guess("/music/Bladee - Eversince (2016) [FLAC]/03. Song.flac"),
            three("Bladee", "Eversince", "Song")
        );
        assert_eq!(
            guess("/music/2016 - Eversince/Song.mp3"),
            three("", "Eversince", "Song")
        );
        // The file's own name wins over the folder's.
        assert_eq!(
            guess("/music/Various/Hits/Ecco2k - Peroxide.mp3"),
            three("Ecco2k", "Hits", "Peroxide")
        );
        // A folder named after the artist is no album.
        assert_eq!(
            guess("/music/Bladee/Bladee - Waster.mp3"),
            three("Bladee", "", "Waster")
        );
        // Right in the library folder: nothing from folders.
        assert_eq!(guess("/music/Bladee - Waster.mp3"), three("Bladee", "", "Waster"));
        // Outside the library: only the folder it is in, as the album.
        let g = guess_from_path(Path::new("/tmp/stuff/Bladee - Waster.mp3"), None);
        assert_eq!((g.artist.as_str(), g.album.as_str()), ("Bladee", "stuff"));
    }

    #[test]
    fn folder_names_lose_release_details() {
        assert_eq!(clean_folder("Album (2019)"), "Album");
        assert_eq!(clean_folder("Album [24bit 96kHz]"), "Album");
        assert_eq!(
            clean_folder("Album (Deluxe Edition) [WEB 320]"),
            "Album (Deluxe Edition)"
        );
        assert_eq!(clean_folder("(2019) Album"), "Album");
        assert_eq!(clean_folder("2019"), "2019");
        assert_eq!(clean_folder("Some_Album"), "Some Album");
    }

    #[test]
    fn tags_win_over_the_file_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let folder = dir.join("Folder Artist").join("Folder Album");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("05 - Name Artist - Name Title.wav");
        std::fs::write(&path, wav()).unwrap();
        // Untagged: everything from the path.
        let t = read_track_in(&path, Some(&dir), None, 0);
        assert_eq!((t.artist.as_str(), t.title.as_str()), ("Name Artist", "Name Title"));
        assert_eq!((t.album.as_str(), t.track_no), ("Folder Album", Some(5)));
        assert_eq!(
            untagged(&path),
            Untagged {
                names: true,
                album: true
            }
        );
        // A title tag only: the rest still comes from the path.
        let meta = crate::library::tags::Metadata {
            title: "Tag Title".into(),
            ..Default::default()
        };
        crate::library::tags::write(&path, &meta, None, false).unwrap();
        let t = read_track_in(&path, Some(&dir), None, 0);
        assert_eq!((t.artist.as_str(), t.title.as_str()), ("Name Artist", "Tag Title"));
        assert_eq!(
            untagged(&path),
            Untagged {
                names: true,
                album: true
            }
        );
        let meta = crate::library::tags::Metadata {
            artist: "Tag Artist".into(),
            album: "Tag Album".into(),
            track: Some(9),
            ..Default::default()
        };
        crate::library::tags::write(&path, &meta, None, false).unwrap();
        let t = read_track_in(&path, Some(&dir), None, 0);
        assert_eq!((t.artist.as_str(), t.title.as_str()), ("Tag Artist", "Tag Title"));
        assert_eq!((t.album.as_str(), t.track_no), ("Tag Album", Some(9)));
        assert_eq!(
            untagged(&path),
            Untagged {
                names: false,
                album: false
            }
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A tiny valid WAV file (silence).
    fn wav() -> Vec<u8> {
        let data_len = 882u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&44100u32.to_le_bytes());
        out.extend_from_slice(&88200u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(out.len() + data_len as usize, 0);
        out
    }
}
