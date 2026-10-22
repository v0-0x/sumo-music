//! Lyrics lookup: sidecar `.lrc`/`.txt` files, embedded tags and LRCLIB, with an on-disk cache.
//!
//! Lookup order (first hit wins):
//! 1. Local tracks: `<stem>.lrc` / `<stem>.txt` next to the audio file (or in a `Lyrics/` subfolder).
//! 2. Local tracks: lyrics embedded in the file's tags.
//! 3. The disk cache of earlier online lookups (positive and negative results).
//! 4. LRCLIB (<https://lrclib.net>), when online lookups are enabled.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use md5::{Digest, Md5};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{now_unix, LyricLine, Lyrics, Source, Track};

pub const PROVIDER_SIDECAR: &str = "Sidecar file";
pub const PROVIDER_EMBEDDED: &str = "Embedded tag";
pub const PROVIDER_LRCLIB: &str = "LRCLIB";
pub const PROVIDER_GENIUS: &str = "Genius";

const LRCLIB_GET: &str = "https://lrclib.net/api/get";
const LRCLIB_SEARCH: &str = "https://lrclib.net/api/search";

/// Negative ("no lyrics found") cache entries are retried after this many seconds.
pub const NEGATIVE_CACHE_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// Max allowed difference between the track duration and an LRCLIB search result.
const DURATION_TOLERANCE_SECS: f64 = 3.0;

/// LRC metadata tags that never carry lyric text.
const METADATA_TAGS: &[&str] = &[
    "ar", "ti", "al", "au", "by", "length", "re", "ve", "offset", "tool", "la", "lang", "id", "#",
];

/// Enhanced (word level) LRC timing tags: `<mm:ss.xx>`.
static WORD_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<\d+:\d+(?:[.:,]\d+)?>").expect("valid regex"));

static WHITESPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("valid regex"));

/// `(feat. X)`, `[ft. X]`, `(with X)`.
static FEAT_PAREN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*[(\[]\s*(?:feat\.?|ft\.?|featuring|with)\s[^)\]]*[)\]]").expect("valid regex")
});

/// Trailing `feat. X` without brackets.
static FEAT_TAIL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s+(?:feat\.?|ft\.|featuring)\s.*$").expect("valid regex"));

/// Parenthesised version decorations: `(Remastered 2011)`, `(Radio Edit)`, `(Live)`.
static VERSION_PAREN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s*\([^)]*\b(?:remaster(?:ed)?|version|edit|live|mono|stereo|mix|single|deluxe|bonus|anniversary|acoustic)\b[^)]*\)",
    )
    .expect("valid regex")
});

/// ` - Remastered 2011`, ` - Radio Edit`, ` - Live at X`.
static VERSION_DASH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s+[-\u{2013}\u{2014}]\s+.*\b(?:remaster(?:ed)?|version|edit|live|mono|stereo|mix|single|deluxe|bonus|anniversary|acoustic)\b.*$",
    )
    .expect("valid regex")
});

/// Any `[...]` decoration.
static BRACKET_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\[[^\]]*\]").expect("valid regex"));

/// Separators between credited artists.
static ARTIST_SEP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:, | & |; | / | x | feat\.?(?:\s|$)| ft\.\s| featuring\s| with\s)").expect("valid regex")
});

// ---------------------------------------------------------------------------------------------
// LRC parsing
// ---------------------------------------------------------------------------------------------

/// One line of an LRC file split into its leading bracket tags and the remaining text.
struct LrcLine<'a> {
    times: Vec<u64>,
    offset: Option<i64>,
    /// The line consisted only of metadata tags (`[ar:...]`).
    meta_only: bool,
    text: &'a str,
}

fn split_lrc_line(line: &str) -> LrcLine<'_> {
    let mut rest = line.trim();
    let mut times = Vec::new();
    let mut offset = None;
    let mut had_meta = false;

    while let Some(close) = rest.strip_prefix('[').and_then(|r| r.find(']')) {
        let inner = &rest[1..1 + close];
        let after = &rest[close + 2..];
        if let Some(t) = parse_timestamp(inner) {
            times.push(t);
            rest = after.trim_start();
        } else if times.is_empty() {
            match metadata_tag(inner) {
                Some((key, value)) => {
                    if key == "offset" {
                        if let Ok(v) = value.trim().parse::<i64>() {
                            offset = Some(v);
                        }
                    }
                    had_meta = true;
                    rest = after.trim_start();
                }
                None => break,
            }
        } else {
            break;
        }
    }

    LrcLine {
        meta_only: had_meta && times.is_empty() && rest.is_empty(),
        times,
        offset,
        text: rest,
    }
}

/// `ar:Artist` -> `("ar", "Artist")` for known metadata keys.
fn metadata_tag(inner: &str) -> Option<(String, &str)> {
    let (key, value) = inner.split_once(':')?;
    let key = key.trim().to_ascii_lowercase();
    METADATA_TAGS.contains(&key.as_str()).then_some((key, value))
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit())
}

fn parse_num(s: &str) -> Option<u64> {
    if all_digits(s) {
        s.parse().ok()
    } else {
        None
    }
}

/// Fraction digits -> milliseconds ("5" = 500, "05" = 50, "050" = 50, "0505" = 50).
fn frac_to_ms(f: &str) -> Option<u64> {
    if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits: String = f.chars().take(3).collect();
    let n: u64 = digits.parse().ok()?;
    Some(match digits.len() {
        1 => n * 100,
        2 => n * 10,
        _ => n,
    })
}

fn split_frac(s: &str) -> (&str, Option<&str>) {
    match s.split_once(['.', ',']) {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    }
}

/// Parses `mm:ss`, `mm:ss.xx`, `mm:ss.xxx`, `mm:ss:xx` and `hh:mm:ss.xx` into milliseconds.
fn parse_timestamp(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    let (hours, mins, secs, frac) = match parts.as_slice() {
        [m, s] => {
            let (s, f) = split_frac(s);
            (0, parse_num(m)?, parse_num(s)?, f)
        }
        // hh:mm:ss.xx
        [h, m, s] if s.contains(['.', ',']) => {
            let (s, f) = split_frac(s);
            (parse_num(h)?, parse_num(m)?, parse_num(s)?, f)
        }
        // mm:ss:xx (hundredths)
        [m, s, f] => (0, parse_num(m)?, parse_num(s)?, Some(*f)),
        _ => return None,
    };
    let frac_ms = match frac {
        Some(f) => frac_to_ms(f)?,
        None => 0,
    };
    hours
        .checked_mul(3_600_000)?
        .checked_add(mins.checked_mul(60_000)?)?
        .checked_add(secs.checked_mul(1000)?)?
        .checked_add(frac_ms)
}

/// Removes enhanced word timing tags and tidies whitespace.
fn clean_lyric_text(text: &str) -> String {
    if WORD_TAG_RE.is_match(text) {
        let stripped = WORD_TAG_RE.replace_all(text, "");
        WHITESPACE_RE.replace_all(stripped.trim(), " ").into_owned()
    } else {
        text.trim().to_string()
    }
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Parses LRC text into time-sorted lines.
///
/// Lines without a timestamp are ignored. Timestamp lines with no text are kept as empty
/// strings (instrumental gaps). `[offset:+ms]` shifts every line earlier (negative: later).
pub fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut offset_ms: i64 = 0;
    let mut lines = Vec::new();

    for raw in strip_bom(text).lines() {
        let parsed = split_lrc_line(raw);
        if let Some(o) = parsed.offset {
            offset_ms = o;
        }
        if parsed.times.is_empty() {
            continue;
        }
        let text = clean_lyric_text(parsed.text);
        for t in parsed.times {
            lines.push((t, text.clone()));
        }
    }

    let mut out: Vec<LyricLine> = lines
        .into_iter()
        .map(|(t, text)| {
            let shifted = (t as i64).saturating_sub(offset_ms).max(0);
            LyricLine {
                time_ms: shifted as u64,
                text,
            }
        })
        .collect();
    // Stable: lines sharing a timestamp keep their file order.
    out.sort_by_key(|l| l.time_ms);
    out
}

/// Joins lines, collapsing runs of blank lines into one and trimming blank edges.
fn join_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Plain text with LRC timestamps, metadata tags and word timing tags removed.
fn strip_lrc_tags(text: &str) -> String {
    let lines: Vec<String> = strip_bom(text)
        .lines()
        .filter_map(|raw| {
            let parsed = split_lrc_line(raw);
            (!parsed.meta_only).then(|| clean_lyric_text(parsed.text))
        })
        .collect();
    join_lines(lines.iter().map(String::as_str))
}

fn is_instrumental_marker(plain: &str) -> bool {
    let core: String = plain
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | '(' | ')' | '*' | '-' | '♪'))
        .collect();
    core.trim().eq_ignore_ascii_case("instrumental")
}

/// Builds [`Lyrics`] from a text blob, detecting LRC automatically.
///
/// If the text contains timestamped lines with words, `synced` is filled and `plain` holds the
/// lines in time order. Otherwise `plain` is the text with any stray tags stripped.
pub fn lyrics_from_text(text: &str, provider: &str) -> Lyrics {
    let synced = parse_lrc(text);
    if synced.iter().any(|l| !l.text.is_empty()) {
        let plain = join_lines(synced.iter().map(|l| l.text.as_str()));
        return Lyrics {
            synced,
            plain,
            instrumental: false,
            provider: provider.to_string(),
        };
    }
    let plain = strip_lrc_tags(text);
    Lyrics {
        synced: Vec::new(),
        instrumental: is_instrumental_marker(&plain),
        plain,
        provider: provider.to_string(),
    }
}

fn has_content(l: &Lyrics) -> bool {
    l.instrumental || !l.synced.is_empty() || !l.plain.trim().is_empty()
}

// ---------------------------------------------------------------------------------------------
// LRCLIB result handling
// ---------------------------------------------------------------------------------------------

fn non_empty_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Converts one LRCLIB record (`/api/get` response or `/api/search` item) into [`Lyrics`].
fn lyrics_from_lrclib(v: &Value, keep_synced: bool) -> Option<Lyrics> {
    let instrumental = v.get("instrumental").and_then(Value::as_bool).unwrap_or(false);
    let synced_text = non_empty_str(v, "syncedLyrics").filter(|_| keep_synced);
    let plain_text = non_empty_str(v, "plainLyrics");

    if let Some(s) = synced_text {
        let mut l = lyrics_from_text(s, PROVIDER_LRCLIB);
        if let Some(p) = plain_text {
            l.plain = join_lines(p.lines());
        }
        if has_content(&l) {
            return Some(l);
        }
    }
    if let Some(p) = plain_text {
        let mut l = lyrics_from_text(p, PROVIDER_LRCLIB);
        // Plain lyrics from LRCLIB are never meant to be time synced.
        if !l.synced.is_empty() {
            l.synced.clear();
            l.plain = strip_lrc_tags(p);
        }
        if has_content(&l) {
            return Some(l);
        }
    }
    instrumental.then(|| Lyrics {
        instrumental: true,
        provider: PROVIDER_LRCLIB.to_string(),
        ..Default::default()
    })
}

fn duration_matches(v: &Value, duration_secs: f64) -> bool {
    if duration_secs <= 0.0 {
        return true;
    }
    match v.get("duration").and_then(Value::as_f64) {
        Some(d) => (d - duration_secs).abs() <= DURATION_TOLERANCE_SECS,
        None => false,
    }
}

/// Picks the best LRCLIB search result.
///
/// Preference: synced lyrics with a matching duration (±3s), then plain lyrics with a matching
/// duration, then a matching instrumental marker, then the first result with plain lyrics
/// (timing dropped since it belongs to a different version of the song).
/// `duration_secs <= 0` means unknown: every result counts as matching.
pub fn pick_best(results: &[Value], duration_secs: f64) -> Option<Lyrics> {
    let matching = || results.iter().filter(|v| duration_matches(v, duration_secs));

    if let Some(l) = matching()
        .filter(|v| non_empty_str(v, "syncedLyrics").is_some())
        .find_map(|v| lyrics_from_lrclib(v, true))
    {
        return Some(l);
    }
    if let Some(l) = matching()
        .filter(|v| non_empty_str(v, "plainLyrics").is_some())
        .find_map(|v| lyrics_from_lrclib(v, true))
    {
        return Some(l);
    }
    if let Some(l) = matching()
        .filter(|v| v.get("instrumental").and_then(Value::as_bool) == Some(true))
        .find_map(|v| lyrics_from_lrclib(v, true))
    {
        return Some(l);
    }
    results
        .iter()
        .filter(|v| non_empty_str(v, "plainLyrics").is_some())
        .find_map(|v| lyrics_from_lrclib(v, false))
}

// ---------------------------------------------------------------------------------------------
// Query cleanup
// ---------------------------------------------------------------------------------------------

/// Strips `(feat. X)`, ` - Remastered 2011`, `[Explicit]` and similar decorations.
pub fn clean_title(title: &str) -> String {
    let s = FEAT_PAREN_RE.replace_all(title, "");
    let s = BRACKET_RE.replace_all(&s, "");
    let s = VERSION_PAREN_RE.replace_all(&s, "");
    let s = VERSION_DASH_RE.replace(&s, "");
    let s = FEAT_TAIL_RE.replace(&s, "");
    let cleaned = WHITESPACE_RE.replace_all(s.trim(), " ").into_owned();
    if cleaned.is_empty() {
        title.trim().to_string()
    } else {
        cleaned
    }
}

/// Lyrics as text for a file's lyrics tag: LRC when they are time-synced, else plain.
pub fn to_tag_text(l: &Lyrics) -> String {
    if l.instrumental {
        return String::new();
    }
    if l.synced.is_empty() {
        return l.plain.trim().to_string();
    }
    l.synced
        .iter()
        .map(|line| {
            let cs = line.time_ms / 10;
            format!("[{:02}:{:02}.{:02}]{}", cs / 6000, cs / 100 % 60, cs % 100, line.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Artist and title to look a song up by: SoundCloud upload names cleaned up, the first
/// credited artist and the title without "(feat. …)" / version suffixes.
pub fn song_names(track: &Track) -> (String, String) {
    let (artist, title) = crate::integrations::lastfm::scrobble_names(track);
    (first_artist(&artist), clean_title(&title))
}

/// The first credited artist ("A, B" / "A & B" / "A feat. B" -> "A").
pub fn first_artist(artist: &str) -> String {
    let artist = artist.trim();
    let first = match ARTIST_SEP_RE.find(artist) {
        Some(m) if m.start() > 0 => &artist[..m.start()],
        _ => artist,
    };
    first.trim().to_string()
}

// ---------------------------------------------------------------------------------------------
// Local files
// ---------------------------------------------------------------------------------------------

/// Decodes a lyrics file: UTF-8 (with or without BOM), UTF-16 with BOM, else Latin-1.
fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    let utf16 = |rest: &[u8], le: bool| {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, false);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Candidate sidecar paths: `.lrc` before `.txt`, next to the file before `Lyrics/` subfolders.
fn sidecar_candidates(audio: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) = (audio.parent(), audio.file_stem()) else {
        return Vec::new();
    };
    let dirs = [dir.to_path_buf(), dir.join("Lyrics"), dir.join("lyrics")];
    let mut out = Vec::new();
    for ext in ["lrc", "LRC", "txt"] {
        for d in &dirs {
            let mut name = stem.to_os_string();
            name.push(".");
            name.push(ext);
            let p = d.join(&name);
            if p != audio {
                out.push(p);
            }
        }
    }
    out
}

fn sidecar_lyrics(audio: &Path) -> Option<Lyrics> {
    for path in sidecar_candidates(audio) {
        if !path.is_file() {
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) => {
                let l = lyrics_from_text(&decode_text(&bytes), PROVIDER_SIDECAR);
                if has_content(&l) {
                    tracing::debug!("lyrics: using sidecar {}", path.display());
                    return Some(l);
                }
            }
            Err(e) => tracing::debug!("lyrics: can't read {}: {e}", path.display()),
        }
    }
    None
}

fn embedded_lyrics(audio: &Path) -> Option<Lyrics> {
    use lofty::config::ParseOptions;
    use lofty::prelude::*;
    use lofty::probe::Probe;

    let options = ParseOptions::new().read_properties(false).read_cover_art(false);
    let tagged = match Probe::open(audio)
        .and_then(|p| Ok(p.guess_file_type()?))
        .and_then(|p| p.options(options).read())
    {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!("lyrics: can't read tags of {}: {e}", audio.display());
            return None;
        }
    };

    let primary = tagged.primary_tag();
    let tags = primary.into_iter().chain(tagged.tags().iter());
    for tag in tags {
        for key in [ItemKey::Lyrics, ItemKey::UnsyncLyrics] {
            for text in tag.get_strings(key) {
                if text.trim().is_empty() {
                    continue;
                }
                let l = lyrics_from_text(text, PROVIDER_EMBEDDED);
                if has_content(&l) {
                    return Some(l);
                }
            }
        }
    }
    None
}

pub(crate) fn local_lyrics(audio: &Path) -> Option<Lyrics> {
    sidecar_lyrics(audio).or_else(|| embedded_lyrics(audio))
}

// ---------------------------------------------------------------------------------------------
// Disk cache
// ---------------------------------------------------------------------------------------------

/// What is stored per track in the lyrics cache.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEntry {
    /// `None` = looked up, nothing found.
    pub found: Option<Lyrics>,
    /// Unix seconds of the lookup.
    pub fetched_at: i64,
}

/// How a cache entry should be treated at time `now`.
#[derive(Debug, Clone, PartialEq)]
pub enum CacheState {
    Hit(Lyrics),
    /// Recently looked up without success; don't ask again yet.
    Negative,
    /// Expired negative result; look up again.
    Stale,
}

impl CacheEntry {
    pub fn state(&self, now: i64) -> CacheState {
        match &self.found {
            Some(l) => CacheState::Hit(l.clone()),
            None if now.saturating_sub(self.fetched_at) < NEGATIVE_CACHE_TTL_SECS => CacheState::Negative,
            None => CacheState::Stale,
        }
    }
}

fn md5_hex(data: &[u8]) -> String {
    Md5::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// Cache file name for a track: hex md5 of `artist|title|album|duration_secs` + `.json`.
pub fn cache_key(track: &Track) -> String {
    let norm = |s: &str| s.trim().to_lowercase();
    let key = format!(
        "{}|{}|{}|{}",
        norm(&track.artist),
        norm(&track.title),
        norm(&track.album),
        track.duration_ms.saturating_add(500) / 1000
    );
    format!("{}.json", md5_hex(key.as_bytes()))
}

// ---------------------------------------------------------------------------------------------
// Fetcher
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct LyricsFetcher {
    http: reqwest::Client,
    cache_dir: PathBuf,
    online: bool,
    /// Plain lyrics when LRCLIB has none.
    genius: Option<std::sync::Arc<crate::integrations::genius::Genius>>,
}

impl LyricsFetcher {
    pub fn new(http: reqwest::Client, cache_dir: PathBuf, online: bool) -> Self {
        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            tracing::warn!("lyrics: can't create cache dir {}: {e}", cache_dir.display());
        }
        LyricsFetcher {
            http,
            cache_dir,
            online,
            genius: None,
        }
    }

    pub fn with_genius(mut self, genius: std::sync::Arc<crate::integrations::genius::Genius>) -> Self {
        self.genius = Some(genius);
        self
    }

    pub fn cache_path(&self, track: &Track) -> PathBuf {
        self.cache_dir.join(cache_key(track))
    }

    /// Reads the cache entry for `track`, if any (corrupt files count as missing).
    pub async fn load_cached(&self, track: &Track) -> Option<CacheEntry> {
        let path = self.cache_path(track);
        let bytes = tokio::fs::read(&path).await.ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(entry) => Some(entry),
            Err(e) => {
                tracing::debug!("lyrics: ignoring corrupt cache file {}: {e}", path.display());
                None
            }
        }
    }

    /// Stores a lookup result (atomically: temp file + rename).
    pub async fn store_cached(&self, track: &Track, found: Option<&Lyrics>, fetched_at: i64) {
        let entry = CacheEntry {
            found: found.cloned(),
            fetched_at,
        };
        let path = self.cache_path(track);
        let result = async {
            tokio::fs::create_dir_all(&self.cache_dir).await?;
            let json = serde_json::to_vec(&entry)?;
            let tmp = path.with_extension("json.tmp");
            tokio::fs::write(&tmp, json).await?;
            tokio::fs::rename(&tmp, &path).await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::warn!("lyrics: can't write cache {}: {e:#}", path.display());
        }
    }

    /// Finds lyrics for `track`, or `None` if there are none (or they can't be fetched).
    pub async fn fetch(&self, track: &Track) -> Option<Lyrics> {
        if track.source == Source::Local && !track.uri.is_empty() {
            let path = PathBuf::from(&track.uri);
            match tokio::task::spawn_blocking(move || local_lyrics(&path)).await {
                Ok(Some(l)) => return Some(l),
                Ok(None) => {}
                Err(e) => tracing::warn!("lyrics: local lookup task failed: {e}"),
            }
        }

        if track.title.trim().is_empty() {
            return None;
        }

        let now = now_unix();
        if let Some(entry) = self.load_cached(track).await {
            match entry.state(now) {
                CacheState::Hit(l) => return Some(l),
                CacheState::Negative => return None,
                CacheState::Stale => {}
            }
        }

        if !self.online {
            return None;
        }

        let lrclib = self.fetch_lrclib(track).await;
        if let Ok(Some(found)) = lrclib {
            self.store_cached(track, Some(&found), now).await;
            return Some(found);
        }
        // Genius has plain (not time-synced) lyrics for far more songs.
        if let Some(found) = self.fetch_genius(track).await {
            self.store_cached(track, Some(&found), now).await;
            return Some(found);
        }
        match lrclib {
            Ok(_) => {
                self.store_cached(track, None, now).await;
                None
            }
            Err(e) => {
                tracing::warn!(
                    "lyrics: LRCLIB lookup for {} - {} failed: {e:#}",
                    track.artist,
                    track.title
                );
                None
            }
        }
    }

    async fn fetch_genius(&self, track: &Track) -> Option<Lyrics> {
        let genius = self.genius.as_ref()?;
        let (artist, title) = song_names(track);
        match genius.lyrics(&artist, &title).await {
            Ok(Some(text)) => Some(Lyrics {
                synced: Vec::new(),
                plain: text,
                instrumental: false,
                provider: PROVIDER_GENIUS.to_string(),
            }),
            Ok(None) => None,
            Err(e) => {
                tracing::debug!("lyrics: Genius lookup for {artist} - {title} failed: {e:#}");
                None
            }
        }
    }

    /// Exact metadata first, then a cleaned title / first artist. `Err` means at least one
    /// lookup failed for a transient reason, so a negative result must not be cached.
    async fn fetch_lrclib(&self, track: &Track) -> anyhow::Result<Option<Lyrics>> {
        let duration = track.duration_secs();
        let exact = (track.artist.trim().to_string(), track.title.trim().to_string());
        let cleaned = (first_artist(&track.artist), clean_title(&track.title));
        let mut attempts = vec![exact];
        // SoundCloud uploads: "Artist - Title [Free DL]" posted by someone else.
        for pair in [cleaned, song_names(track)] {
            if !attempts.contains(&pair) && !pair.1.is_empty() {
                attempts.push(pair);
            }
        }

        let mut error = None;
        for (artist, title) in attempts {
            match self.lrclib_lookup(&artist, &title, &track.album, duration).await {
                Ok(Some(l)) => return Ok(Some(l)),
                Ok(None) => tracing::debug!("lyrics: LRCLIB has nothing for {artist} - {title}"),
                Err(e) => {
                    tracing::debug!("lyrics: LRCLIB lookup for {artist} - {title} failed: {e:#}");
                    error = Some(e);
                }
            }
        }
        match error {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    async fn lrclib_lookup(
        &self,
        artist: &str,
        title: &str,
        album: &str,
        duration: f64,
    ) -> anyhow::Result<Option<Lyrics>> {
        if !artist.is_empty() {
            if let Some(l) = self.lrclib_get(artist, title, album, duration).await? {
                return Ok(Some(l));
            }
        }
        self.lrclib_search(artist, title, duration).await
    }

    async fn lrclib_get(
        &self,
        artist: &str,
        title: &str,
        album: &str,
        duration: f64,
    ) -> anyhow::Result<Option<Lyrics>> {
        let mut query: Vec<(&str, String)> =
            vec![("artist_name", artist.to_string()), ("track_name", title.to_string())];
        if !album.trim().is_empty() {
            query.push(("album_name", album.trim().to_string()));
        }
        if duration > 0.0 {
            query.push(("duration", format!("{}", duration.round() as u64)));
        }
        let resp = self.http.get(LRCLIB_GET).query(&query).send().await?;
        let status = resp.status();
        // 404 = no match; other 4xx (bad/missing params) also just mean "try searching".
        if status.is_client_error() && status != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Ok(None);
        }
        if !status.is_success() {
            anyhow::bail!("LRCLIB get returned {status}");
        }
        let v: Value = resp.json().await?;
        Ok(lyrics_from_lrclib(&v, true))
    }

    async fn lrclib_search(&self, artist: &str, title: &str, duration: f64) -> anyhow::Result<Option<Lyrics>> {
        let mut query: Vec<(&str, &str)> = vec![("track_name", title)];
        if !artist.is_empty() {
            query.push(("artist_name", artist));
        }
        let resp = self.http.get(LRCLIB_SEARCH).query(&query).send().await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("LRCLIB search returned {status}");
        }
        let v: Value = resp.json().await?;
        let results = v.as_array().map(Vec::as_slice).unwrap_or_default();
        Ok(pick_best(results, duration))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(time_ms: u64, text: &str) -> LyricLine {
        LyricLine {
            time_ms,
            text: text.to_string(),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "multimusic-lyrics-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn track(artist: &str, title: &str, album: &str, duration_ms: u64) -> Track {
        Track {
            album_artist: String::new(),
            disc_no: None,
            id: format!("spotify:track:{title}"),
            source: Source::Spotify,
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            duration_ms,
            track_no: None,
            art: None,
            uri: String::new(),
            added_at: 0,
        }
    }

    #[test]
    fn timestamp_formats() {
        let lrc = "[00:01.50]two digit\n[00:02.250]three digit\n[00:03]no fraction\n[00:04:75]colon hundredths\n[1:05.5]one digit";
        assert_eq!(
            parse_lrc(lrc),
            vec![
                line(1500, "two digit"),
                line(2250, "three digit"),
                line(3000, "no fraction"),
                line(4750, "colon hundredths"),
                line(65_500, "one digit"),
            ]
        );
    }

    #[test]
    fn multiple_timestamps_and_sorting() {
        let lrc = "[00:12.00][01:30.00]Chorus\n[00:05.00]Intro\n[00:20.00]Verse\n[00:20.00]Same time second";
        assert_eq!(
            parse_lrc(lrc),
            vec![
                line(5000, "Intro"),
                line(12_000, "Chorus"),
                line(20_000, "Verse"),
                line(20_000, "Same time second"),
                line(90_000, "Chorus"),
            ]
        );
    }

    #[test]
    fn offset_tag() {
        // Positive offset: lyrics appear earlier.
        let lrc = "[offset:+500]\n[00:10.00]a\n[00:00.20]b";
        assert_eq!(parse_lrc(lrc), vec![line(0, "b"), line(9500, "a")]);
        let lrc = "[offset:-250]\n[00:10.00]a";
        assert_eq!(parse_lrc(lrc), vec![line(10_250, "a")]);
        // Offset placed after the lines still applies to the whole file.
        let lrc = "[00:10.00]a\n[offset:1000]";
        assert_eq!(parse_lrc(lrc), vec![line(9000, "a")]);
    }

    #[test]
    fn metadata_ignored_and_empty_lines_kept() {
        let lrc = "\u{feff}[ar:Artist]\n[ti:Title]\n[al:Album]\n[by:someone]\n[length: 03:20]\n[re:tool]\n[ve:1.0]\n\
                   [00:01.00]First\n[00:05.00]\n[00:09.00]  Second  \r\nno timestamp line\n";
        assert_eq!(
            parse_lrc(lrc),
            vec![line(1000, "First"), line(5000, ""), line(9000, "Second")]
        );
    }

    #[test]
    fn enhanced_word_tags_stripped() {
        let lrc = "[00:12.00]<00:12.00> Hello <00:12.50> wor<00:12.80>ld <00:13.10>";
        assert_eq!(parse_lrc(lrc), vec![line(12_000, "Hello world")]);
    }

    #[test]
    fn bad_timestamps_are_not_lines() {
        let lrc = "[Chorus]\n[aa:bb]x\n[99999999999999999999:00]overflow\n[00:01.00]ok";
        assert_eq!(parse_lrc(lrc), vec![line(1000, "ok")]);
    }

    #[test]
    fn lyrics_from_lrc_text() {
        let l = lyrics_from_text(
            "[ar:X]\n[00:01.00]One\n[00:02.00]\n[00:03.00]\n[00:04.00]Two",
            "Sidecar file",
        );
        assert_eq!(l.synced.len(), 4);
        assert_eq!(l.plain, "One\n\nTwo");
        assert_eq!(l.provider, "Sidecar file");
        assert!(!l.instrumental);
    }

    #[test]
    fn lyrics_from_plain_text() {
        let text = "[ar:Someone]\nFirst line\n[Chorus]\nSecond line\n\n\n\nThird";
        let l = lyrics_from_text(text, "Embedded tag");
        assert!(l.synced.is_empty());
        assert_eq!(l.plain, "First line\n[Chorus]\nSecond line\n\nThird");

        // Timestamps without any words are not synced lyrics; they get stripped.
        let l = lyrics_from_text("[00:01.00]\n[00:02.00]\nJust text", "x");
        assert!(l.synced.is_empty());
        assert_eq!(l.plain, "Just text");

        let l = lyrics_from_text("[Instrumental]", "x");
        assert!(l.instrumental);
    }

    #[test]
    fn pick_best_prefers_synced_with_matching_duration() {
        let results = vec![
            json!({"duration": 200.0, "plainLyrics": "plain only", "syncedLyrics": null, "instrumental": false}),
            json!({"duration": 260.0, "plainLyrics": "wrong version", "syncedLyrics": "[00:01.00]wrong version", "instrumental": false}),
            json!({"duration": 201.5, "plainLyrics": "right", "syncedLyrics": "[00:01.00]right", "instrumental": false}),
        ];
        let l = pick_best(&results, 200.0).unwrap();
        assert_eq!(l.synced, vec![line(1000, "right")]);
        assert_eq!(l.plain, "right");
        assert_eq!(l.provider, "LRCLIB");
    }

    #[test]
    fn pick_best_falls_back_to_plain() {
        // Synced exists but with the wrong duration; plain with a matching duration wins.
        let results = vec![
            json!({"duration": 300.0, "plainLyrics": "far", "syncedLyrics": "[00:01.00]far"}),
            json!({"duration": 199.0, "plainLyrics": "close plain", "syncedLyrics": ""}),
        ];
        let l = pick_best(&results, 200.0).unwrap();
        assert!(l.synced.is_empty());
        assert_eq!(l.plain, "close plain");

        // Nothing matches the duration: first plain lyrics, without timing.
        let results = vec![
            json!({"duration": 300.0, "plainLyrics": null, "instrumental": true}),
            json!({"duration": 300.0, "plainLyrics": "far plain", "syncedLyrics": "[00:01.00]far plain"}),
        ];
        let l = pick_best(&results, 200.0).unwrap();
        assert!(l.synced.is_empty());
        assert_eq!(l.plain, "far plain");

        // Unknown duration: everything matches, synced wins.
        let l = pick_best(&results, 0.0).unwrap();
        assert_eq!(l.synced.len(), 1);
    }

    #[test]
    fn pick_best_instrumental_and_empty() {
        let results = vec![json!({"duration": 120.0, "instrumental": true, "plainLyrics": null, "syncedLyrics": null})];
        let l = pick_best(&results, 121.0).unwrap();
        assert!(l.instrumental);
        assert!(pick_best(&[], 100.0).is_none());
        assert!(pick_best(&[json!({"duration": 100.0, "plainLyrics": ""})], 100.0).is_none());
    }

    #[test]
    fn title_and_artist_cleanup() {
        assert_eq!(clean_title("Get Lucky (feat. Pharrell Williams)"), "Get Lucky");
        assert_eq!(clean_title("Let It Be - Remastered 2009"), "Let It Be");
        assert_eq!(clean_title("Song [Explicit]"), "Song");
        assert_eq!(clean_title("Song (Radio Edit) [feat. X]"), "Song");
        assert_eq!(clean_title("Track feat. Someone"), "Track");
        assert_eq!(clean_title("Plain Song"), "Plain Song");
        assert_eq!(clean_title("[Untitled]"), "[Untitled]");
        assert_eq!(first_artist("Daft Punk, Pharrell Williams"), "Daft Punk");
        assert_eq!(first_artist("Simon & Garfunkel"), "Simon");
        assert_eq!(first_artist("Artist feat. Other"), "Artist");
        assert_eq!(first_artist("Solo"), "Solo");
        assert_eq!(first_artist("Ünïcødé ft. İstanbul"), "Ünïcødé");
    }

    #[test]
    fn cache_entry_states() {
        let found = CacheEntry {
            found: Some(Lyrics::default()),
            fetched_at: 0,
        };
        assert!(matches!(found.state(i64::MAX), CacheState::Hit(_)));
        let negative = CacheEntry {
            found: None,
            fetched_at: 1_000,
        };
        assert_eq!(negative.state(1_000 + 60), CacheState::Negative);
        assert_eq!(negative.state(1_000 + NEGATIVE_CACHE_TTL_SECS), CacheState::Stale);
    }

    #[tokio::test]
    async fn cache_round_trip() {
        let dir = temp_dir("cache");
        let fetcher = LyricsFetcher::new(reqwest::Client::new(), dir.join("nested"), false);
        let t = track("Artist", "Title", "Album", 200_400);
        assert!(fetcher.load_cached(&t).await.is_none());

        let lyrics = lyrics_from_text("[00:01.00]hello", PROVIDER_LRCLIB);
        fetcher.store_cached(&t, Some(&lyrics), 1234).await;
        let entry = fetcher.load_cached(&t).await.unwrap();
        assert_eq!(entry.found.as_ref(), Some(&lyrics));
        assert_eq!(entry.fetched_at, 1234);
        assert!(fetcher
            .cache_path(&t)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with(".json"));
        // Offline fetch is served from the cache.
        assert_eq!(fetcher.fetch(&t).await, Some(lyrics));

        // Fresh negative result: None without going online.
        let other = track("Artist", "Other", "", 0);
        fetcher.store_cached(&other, None, now_unix()).await;
        assert_eq!(fetcher.load_cached(&other).await.unwrap().found, None);
        assert_eq!(fetcher.fetch(&other).await, None);

        // Different metadata -> different key.
        assert_ne!(cache_key(&t), cache_key(&other));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn sidecar_files() {
        let dir = temp_dir("sidecar");
        let audio = dir.join("01 Song.flac");
        std::fs::write(&audio, b"not really audio").unwrap();
        let mut t = track("A", "Song", "", 0);
        t.source = Source::Local;
        t.uri = audio.to_string_lossy().into_owned();
        let fetcher = LyricsFetcher::new(reqwest::Client::new(), dir.join("cache"), false);

        // Nothing local, offline: None (and no panic on the unparseable "audio" file).
        assert_eq!(fetcher.fetch(&t).await, None);

        std::fs::create_dir_all(dir.join("Lyrics")).unwrap();
        std::fs::write(dir.join("Lyrics").join("01 Song.txt"), "plain words").unwrap();
        let l = fetcher.fetch(&t).await.unwrap();
        assert_eq!(l.plain, "plain words");
        assert_eq!(l.provider, PROVIDER_SIDECAR);

        // .lrc wins over .txt.
        std::fs::write(dir.join("01 Song.lrc"), "[00:02.00]synced words").unwrap();
        let l = fetcher.fetch(&t).await.unwrap();
        assert_eq!(l.synced, vec![line(2000, "synced words")]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn decodes_legacy_encodings() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi"), "hi");
        assert_eq!(decode_text(b"caf\xE9"), "café");
        assert_eq!(decode_text(&[0xFF, 0xFE, b'h', 0, b'i', 0]), "hi");
    }

    #[test]
    fn lyrics_tag_text_roundtrips() {
        let l = lyrics_from_text("[00:01.00]Hello\n[01:02.34]World", "x");
        let text = to_tag_text(&l);
        assert_eq!(text, "[00:01.00]Hello\n[01:02.34]World");
        assert_eq!(lyrics_from_text(&text, "x").synced, l.synced);
        let plain = lyrics_from_text("Just words\nmore", "x");
        assert_eq!(to_tag_text(&plain), "Just words\nmore");
    }
}
