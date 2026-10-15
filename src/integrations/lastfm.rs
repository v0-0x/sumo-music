//! Last.fm: authentication, now playing, scrobbling (with an offline queue) and loving tracks.
//!
//! API docs: <https://www.last.fm/api/scrobbling>. Every write call is a signed, form-encoded
//! POST. Scrobbles that fail for transient reasons (network, 5xx, rate limiting) are kept in a
//! JSON queue file and resubmitted in batches by [`Lastfm::flush_queue`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::collections::HashMap;
use std::sync::Mutex;

use crate::integrations::genius::{Genius, Match};
use crate::model::{now_unix, Source, Track};

pub const API_ROOT: &str = "https://ws.audioscrobbler.com/2.0/";
const AUTH_URL: &str = "https://www.last.fm/api/auth/";

/// Max scrobbles per `track.scrobble` request.
pub const BATCH_SIZE: usize = 50;
/// Max queued scrobbles kept on disk (oldest are dropped first).
pub const QUEUE_CAP: usize = 2000;
/// Last.fm rejects scrobbles with a timestamp older than this.
pub const MAX_SCROBBLE_AGE_SECS: i64 = 14 * 24 * 60 * 60;

/// Error codes (<https://www.last.fm/api/errorcodes>).
const ERR_OPERATION_FAILED: i64 = 8;
const ERR_INVALID_SESSION: i64 = 9;
const ERR_SERVICE_OFFLINE: i64 = 11;
const ERR_UNAUTHORIZED_TOKEN: i64 = 14;
const ERR_TEMPORARY: i64 = 16;
const ERR_RATE_LIMIT: i64 = 29;

/// Computes `api_sig`: params sorted by key, `key + value` concatenated, secret appended,
/// md5 as lowercase hex. `format` and `callback` are not part of the signature.
pub fn sign(params: &[(&str, &str)], secret: &str) -> String {
    let mut sorted: Vec<&(&str, &str)> = params
        .iter()
        .filter(|(k, _)| *k != "format" && *k != "callback")
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = Md5::new();
    for (k, v) in sorted {
        hasher.update(k.as_bytes());
        hasher.update(v.as_bytes());
    }
    hasher.update(secret.as_bytes());
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A failed Last.fm call.
#[derive(Debug, Clone, PartialEq)]
pub enum LastfmError {
    /// `{ "error": code, "message": "..." }` returned by the API.
    Api { code: i64, message: String },
    /// Non-success HTTP status without a parseable API error.
    Http { status: u16 },
}

impl fmt::Display for LastfmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LastfmError::Api { code, message } => write!(f, "Last.fm error {code}: {message}"),
            LastfmError::Http { status } => write!(f, "Last.fm returned HTTP {status}"),
        }
    }
}

impl std::error::Error for LastfmError {}

fn api_error_code(err: &anyhow::Error) -> Option<i64> {
    match err.downcast_ref::<LastfmError>() {
        Some(LastfmError::Api { code, .. }) => Some(*code),
        _ => None,
    }
}

/// True for `auth.getSession` failing because the user hasn't approved the token yet (code 14).
pub fn is_pending_auth(err: &anyhow::Error) -> bool {
    api_error_code(err) == Some(ERR_UNAUTHORIZED_TOKEN)
}

/// True for the session key being invalid/revoked (code 9): the user must re-authenticate.
pub fn is_auth_error(err: &anyhow::Error) -> bool {
    api_error_code(err) == Some(ERR_INVALID_SESSION)
}

/// True when retrying later may succeed: network errors, HTTP 5xx/429, and Last.fm codes
/// 8 (operation failed), 11 (service offline), 16 (temporarily unavailable), 29 (rate limit).
pub fn is_transient(err: &anyhow::Error) -> bool {
    if err.chain().any(|e| e.downcast_ref::<reqwest::Error>().is_some()) {
        return true;
    }
    match err.downcast_ref::<LastfmError>() {
        Some(LastfmError::Api { code, .. }) => {
            matches!(
                *code,
                ERR_OPERATION_FAILED | ERR_SERVICE_OFFLINE | ERR_TEMPORARY | ERR_RATE_LIMIT
            )
        }
        Some(LastfmError::Http { status }) => *status >= 500 || *status == 429,
        None => false,
    }
}

/// Turns a response body into JSON or a [`LastfmError`].
fn parse_response(status: u16, body: &str) -> anyhow::Result<Value> {
    let json: Option<Value> = serde_json::from_str(body).ok();
    if let Some(v) = &json {
        if let Some(code) = v.get("error").and_then(Value::as_i64) {
            let message = v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            return Err(LastfmError::Api { code, message }.into());
        }
    }
    if !(200..300).contains(&status) {
        return Err(LastfmError::Http { status }.into());
    }
    json.context("Last.fm returned invalid JSON")
}

/// Whether an `album.getInfo` answer has a real cover (not empty, not Last.fm's grey star).
pub fn album_has_cover(v: &Value) -> bool {
    const PLACEHOLDER: &str = "2a96cbd8b46e442fc41c2b86b821562f";
    v.pointer("/album/image")
        .and_then(Value::as_array)
        .is_some_and(|images| {
            images.iter().any(|i| {
                i.get("#text")
                    .and_then(Value::as_str)
                    .is_some_and(|url| url.starts_with("http") && !url.contains(PLACEHOLDER))
            })
        })
}

/// Why Last.fm ignored scrobbles in a `track.scrobble` answer (`ignoredMessage` codes).
pub fn ignored_reasons(v: &serde_json::Value) -> Vec<String> {
    let scrobbles = match v.pointer("/scrobbles/scrobble") {
        Some(serde_json::Value::Array(list)) => list.iter().collect(),
        Some(one) => vec![one],
        None => Vec::new(),
    };
    scrobbles
        .into_iter()
        .filter_map(|s| {
            let message = s.get("ignoredMessage")?;
            let code = message
                .get("code")
                .and_then(|c| {
                    c.as_str()
                        .map(str::to_string)
                        .or_else(|| c.as_u64().map(|n| n.to_string()))
                })
                .unwrap_or_default();
            let why = match code.as_str() {
                "" | "0" => return None,
                "1" => "Last.fm rejected the artist name (check the file's tags)",
                "2" => "Last.fm rejected the song title (check the file's tags)",
                "3" => "it was played too long ago",
                "4" => "its time is in the future (check your computer's clock)",
                "5" => "you've reached Last.fm's daily scrobble limit",
                _ => "Last.fm ignored it",
            };
            let track = s.pointer("/track/#text").and_then(|t| t.as_str()).unwrap_or_default();
            Some(if track.is_empty() {
                why.to_string()
            } else {
                format!("“{track}”: {why}")
            })
        })
        .collect()
}

/// Artist and title as Last.fm knows them. SoundCloud uploads are often titled
/// "Artist - Title [Free DL]" and posted by a label, a fan or a repost channel, so for those
/// the artist comes from the title and upload tags are dropped.
pub fn scrobble_names(track: &Track) -> (String, String) {
    let artist = scrobble_artist(&track.artist).to_string();
    let title = track.title.trim().to_string();
    if track.source != Source::SoundCloud {
        return (artist, title);
    }
    // "(+Yung Lean)" is how uploads credit a guest.
    let title = plus_features(&title);
    // Official releases on SoundCloud come with proper titles (and an album); only the
    // collaboration credit ("Bladee x Uli K") needs to become the main artist.
    if !track.album.trim().is_empty() {
        return (main_credit(&artist).to_string(), title);
    }
    let title = strip_upload_tags(&title);
    if let Some((left, right)) = split_artist_title(&title) {
        return (main_credit(&left).to_string(), right);
    }
    (main_credit(&artist).to_string(), title)
}

/// "Artist - Title" -> ("Artist", "Title"); `None` for a plain title or "Song - Live" (a
/// version, not an artist).
pub(crate) fn split_artist_title(title: &str) -> Option<(String, String)> {
    let (left, right) = title.split_once(" - ")?;
    let (left, right) = (left.trim(), right.trim());
    let lower = right.to_lowercase();
    let version = VERSION_WORDS
        .iter()
        .any(|w| lower == *w || lower.ends_with(&format!(" {w}")));
    (!left.is_empty() && !right.is_empty() && !version).then(|| (left.to_string(), right.to_string()))
}

const VERSION_WORDS: &[&str] = &[
    "live",
    "remix",
    "edit",
    "mix",
    "slowed",
    "sped up",
    "reverb",
    "demo",
    "acoustic",
    "instrumental",
    "remastered",
    "remaster",
    "bootleg",
    "flip",
    "vip",
    "rework",
    "cover",
    "extended",
    "version",
];

/// "Song (+Guest)" -> "Song (feat. Guest)", the way Last.fm and Spotify name features.
fn plus_features(title: &str) -> String {
    static PLUS_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"[(\[]\s*\+\s*([^)\]+][^)\]]*?)\s*[)\]]").expect("valid regex"));
    PLUS_RE.replace_all(title, "(feat. $1)").into_owned()
}

/// "A ft. B" / "A feat. B" / "A x B" / "A+B" -> "A".
fn main_credit(artist: &str) -> &str {
    // "a+b" (no spaces) is a collaboration; "Florence + the Machine" is a band.
    let bytes = artist.as_bytes();
    let plus = (1..bytes.len().saturating_sub(1))
        .find(|&i| bytes[i] == b'+' && bytes[i - 1].is_ascii_alphanumeric() && bytes[i + 1].is_ascii_alphanumeric());
    let artist = plus.map_or(artist, |i| artist[..i].trim());
    // ASCII lowercase keeps byte positions the same as in `artist`.
    let lower = artist.to_ascii_lowercase();
    // "A x B" is how SoundCloud credits collaborations.
    [
        " feat. ",
        " feat ",
        " ft. ",
        " ft ",
        " featuring ",
        " (feat",
        " (ft",
        " x ",
    ]
    .iter()
    .filter_map(|sep| lower.find(sep))
    .min()
    .map_or(artist, |i| artist[..i].trim())
}

/// Drops "[Free DL]", "(Official Audio)", "(prod. X)" and similar from an upload's title.
pub(crate) fn strip_upload_tags(title: &str) -> String {
    const TAGS: &[&str] = &[
        "free",
        "download",
        "dl",
        "out now",
        "premiere",
        "exclusive",
        "official",
        "video",
        "audio",
        "prod",
        "produced",
        "lyrics",
        "hq",
        "320",
    ];
    let mut out = String::new();
    let mut rest = title;
    while let Some(start) = rest.find(['[', '(']) {
        let close = if rest[start..].starts_with('[') { ']' } else { ')' };
        let Some(len) = rest[start..].find(close) else { break };
        let inner = rest[start + 1..start + len].to_lowercase();
        let words: Vec<&str> = inner
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let tag = TAGS.iter().any(|t| {
            if t.contains(' ') {
                inner.contains(t)
            } else {
                words.contains(t)
            }
        });
        out.push_str(&rest[..start]);
        if !tag {
            out.push_str(&rest[start..=start + len]);
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Last.fm prefers the main artist: "A, B" -> "A".
pub fn scrobble_artist(artist: &str) -> &str {
    let artist = artist.trim();
    match artist.split_once(", ") {
        Some((first, _)) if !first.trim().is_empty() => first.trim(),
        _ => artist,
    }
}

/// One scrobble, as stored in the offline queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedScrobble {
    pub artist: String,
    pub track: String,
    #[serde(default)]
    pub album: String,
    /// Seconds, 0 = unknown.
    #[serde(default)]
    pub duration: u64,
    /// Unix seconds when playback started.
    pub timestamp: i64,
}

impl QueuedScrobble {
    /// `None` when the track lacks an artist or title (Last.fm would reject it).
    pub fn from_track(track: &Track, started_at: i64) -> Option<Self> {
        let (artist, title) = scrobble_names(track);
        if artist.is_empty() || title.is_empty() {
            return None;
        }
        Some(QueuedScrobble {
            artist,
            track: title,
            album: track.album.trim().to_string(),
            duration: track.duration_ms.saturating_add(500) / 1000,
            timestamp: started_at,
        })
    }
}

/// Drops scrobbles Last.fm would reject for being older than 14 days.
pub fn prune_queue(queue: Vec<QueuedScrobble>, now: i64) -> Vec<QueuedScrobble> {
    queue
        .into_iter()
        .filter(|s| now.saturating_sub(s.timestamp) < MAX_SCROBBLE_AGE_SECS)
        .collect()
}

/// Keeps at most [`QUEUE_CAP`] entries, dropping the oldest (front) ones.
fn cap_queue(queue: &mut Vec<QueuedScrobble>) {
    if queue.len() > QUEUE_CAP {
        let excess = queue.len() - QUEUE_CAP;
        queue.drain(..excess);
    }
}

/// Reads the queue file; a missing or corrupt file is an empty queue.
pub async fn load_queue(path: &Path) -> Vec<QueuedScrobble> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!("lastfm: ignoring corrupt scrobble queue {}: {e}", path.display());
            Vec::new()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            tracing::warn!("lastfm: can't read scrobble queue {}: {e}", path.display());
            Vec::new()
        }
    }
}

/// Writes the queue atomically (temp file + rename). An empty queue removes the file.
pub async fn save_queue(path: &Path, queue: &[QueuedScrobble]) -> anyhow::Result<()> {
    if queue.is_empty() {
        match tokio::fs::remove_file(path).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(dir).await?;
    }
    let json = serde_json::to_vec_pretty(queue)?;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    tokio::fs::write(&tmp, json)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// `track.scrobble` batch parameters (`artist[i]`, `track[i]`, ...).
fn batch_params(batch: &[QueuedScrobble]) -> Vec<(String, String)> {
    let mut params = Vec::with_capacity(batch.len() * 5);
    for (i, s) in batch.iter().enumerate() {
        params.push((format!("artist[{i}]"), s.artist.clone()));
        params.push((format!("track[{i}]"), s.track.clone()));
        params.push((format!("timestamp[{i}]"), s.timestamp.to_string()));
        if !s.album.is_empty() {
            params.push((format!("album[{i}]"), s.album.clone()));
        }
        if s.duration > 0 {
            params.push((format!("duration[{i}]"), s.duration.to_string()));
        }
    }
    params
}

fn session_from(v: &Value) -> anyhow::Result<(String, String)> {
    let session = v.get("session").context("Last.fm response has no session")?;
    let key = session
        .get("key")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .context("Last.fm session has no key")?;
    let name = session.get("name").and_then(Value::as_str).unwrap_or_default();
    Ok((key.to_string(), name.to_string()))
}

pub struct Lastfm {
    http: reqwest::Client,
    api_key: String,
    api_secret: String,
    session_key: Option<String>,
    queue_path: PathBuf,
    /// Serializes access to the queue file.
    queue_lock: tokio::sync::Mutex<()>,
    root: String,
    /// Albums Last.fm knows for "artist\u{1f}title" (lowercase); `None` = it doesn't.
    albums: Mutex<HashMap<String, Option<String>>>,
    /// Whether Last.fm has a cover for "artist\u{1f}album" (lowercase).
    covers: Mutex<HashMap<String, bool>>,
    /// The real artist, title and album of SoundCloud uploads.
    genius: Option<std::sync::Arc<Genius>>,
}

impl Lastfm {
    pub fn new(http: reqwest::Client, api_key: &str, api_secret: &str, session_key: &str, queue_path: PathBuf) -> Self {
        let session_key = session_key.trim();
        Lastfm {
            http,
            api_key: api_key.trim().to_string(),
            api_secret: api_secret.trim().to_string(),
            session_key: (!session_key.is_empty()).then(|| session_key.to_string()),
            queue_path,
            queue_lock: tokio::sync::Mutex::new(()),
            // Tests of the whole app point this at a stand-in for Last.fm.
            root: std::env::var("MULTIMUSIC_LASTFM_API").unwrap_or_else(|_| API_ROOT.to_string()),
            albums: Mutex::new(HashMap::new()),
            covers: Mutex::new(HashMap::new()),
            genius: None,
        }
    }

    pub fn with_genius(mut self, genius: std::sync::Arc<Genius>) -> Self {
        self.genius = Some(genius);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_root(mut self, root: &str) -> Self {
        self.root = root.to_string();
        self
    }

    /// The album Last.fm has for this song (`track.getInfo`), for songs that come without one
    /// (most SoundCloud uploads). Last.fm and apps built on it (e.g. .fmbot) take the cover art
    /// from the album, so a scrobble without one shows no art.
    async fn known_album(&self, artist: &str, title: &str) -> Option<String> {
        let key = format!("{}\u{1f}{}", artist.to_lowercase(), title.to_lowercase());
        if let Some(found) = self.albums.lock().unwrap().get(&key) {
            return found.clone();
        }
        let params = vec![
            ("artist".to_string(), artist.to_string()),
            ("track".to_string(), title.to_string()),
            ("autocorrect".to_string(), "1".to_string()),
        ];
        let answer = tokio::time::timeout(Duration::from_secs(8), self.get("track.getInfo", params)).await;
        let album = match answer {
            Ok(Ok(v)) => v
                .pointer("/track/album/title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(str::to_string),
            // "Track not found" is an answer too.
            Ok(Err(e)) if api_error_code(&e).is_some() => None,
            // Network trouble: don't remember anything.
            _ => return None,
        };
        let mut albums = self.albums.lock().unwrap();
        if albums.len() > 2000 {
            albums.clear();
        }
        albums.insert(key, album.clone());
        album
    }

    /// Makes a SoundCloud upload look like the song it is (Genius knows the real artist, title
    /// and album), then fills in a missing album from Last.fm itself.
    async fn complete(&self, s: &mut QueuedScrobble, track: &Track) {
        let upload = track.source == Source::SoundCloud && track.album.trim().is_empty();
        if let Some(genius) = self.genius.as_ref().filter(|_| upload) {
            if let Some(song) = genius.find(&s.artist, &s.track, Match::Upload).await {
                tracing::debug!(
                    "lastfm: {} - {} is {} - {} on Genius",
                    s.artist,
                    s.track,
                    song.artist,
                    song.title
                );
                if !song.artist.is_empty() && !song.title.is_empty() {
                    s.artist = song.artist;
                    s.track = song.title;
                }
                if !song.album.is_empty() {
                    s.album = song.album;
                }
            }
        }
        let cleaned = crate::integrations::lyrics::clean_title(&s.track);
        let mut listed = self.known_album(&s.artist, &s.track).await;
        if listed.is_none() && cleaned != s.track {
            listed = self.known_album(&s.artist, &cleaned).await;
        }
        if s.album.is_empty() {
            if let Some(album) = &listed {
                s.album = album.clone();
            }
        }
        // A SoundCloud song's album was worked out by Sumo, so it may as well be one Last.fm
        // has a cover for: apps like .fmbot then show it at once instead of searching for art.
        if track.source == Source::SoundCloud && self.has_cover(&s.artist, &s.album).await == Some(false) {
            for candidate in [listed, Some(cleaned)].into_iter().flatten() {
                if !candidate.eq_ignore_ascii_case(&s.album)
                    && self.has_cover(&s.artist, &candidate).await == Some(true)
                {
                    tracing::debug!("lastfm: {} has no cover on Last.fm; using {candidate}", s.album);
                    s.album = candidate;
                    break;
                }
            }
        }
    }

    /// Whether Last.fm has cover art for the album (`album.getInfo`); `None` when it couldn't
    /// be asked. Remembered.
    async fn has_cover(&self, artist: &str, album: &str) -> Option<bool> {
        if album.trim().is_empty() {
            return Some(false);
        }
        let key = format!("{}\u{1f}{}", artist.to_lowercase(), album.to_lowercase());
        if let Some(known) = self.covers.lock().unwrap().get(&key) {
            return Some(*known);
        }
        let params = vec![
            ("artist".to_string(), artist.to_string()),
            ("album".to_string(), album.to_string()),
            ("autocorrect".to_string(), "1".to_string()),
        ];
        let answer = tokio::time::timeout(Duration::from_secs(8), self.get("album.getInfo", params)).await;
        let found = match answer {
            Ok(Ok(v)) => album_has_cover(&v),
            // "Album not found": no cover.
            Ok(Err(e)) if api_error_code(&e).is_some() => false,
            _ => return None,
        };
        let mut covers = self.covers.lock().unwrap();
        if covers.len() > 2000 {
            covers.clear();
        }
        covers.insert(key, found);
        Some(found)
    }

    pub fn is_authenticated(&self) -> bool {
        self.session_key.is_some()
    }

    /// Adds `method`, `api_key`, optionally `sk`, then `api_sig` and `format=json`.
    fn signed(
        &self,
        method: &str,
        mut params: Vec<(String, String)>,
        with_session: bool,
    ) -> anyhow::Result<Vec<(String, String)>> {
        if self.api_key.is_empty() || self.api_secret.is_empty() {
            anyhow::bail!("Last.fm API key/secret not configured");
        }
        params.push(("method".into(), method.into()));
        params.push(("api_key".into(), self.api_key.clone()));
        if with_session {
            let sk = self.session_key.as_ref().context("not logged in to Last.fm")?;
            params.push(("sk".into(), sk.clone()));
        }
        let refs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let sig = sign(&refs, &self.api_secret);
        params.push(("api_sig".into(), sig));
        params.push(("format".into(), "json".into()));
        Ok(params)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> anyhow::Result<Value> {
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let body = resp.text().await?;
        parse_response(status, &body)
    }

    async fn get(&self, method: &str, params: Vec<(String, String)>) -> anyhow::Result<Value> {
        let params = self.signed(method, params, false)?;
        self.send(self.http.get(&self.root).query(&params)).await
    }

    /// A read-only call (`user.getInfo`, `user.getTopArtists`, …), which needs the API key but
    /// no signature or login.
    pub(crate) async fn public(&self, method: &str, mut params: Vec<(String, String)>) -> anyhow::Result<Value> {
        if self.api_key.is_empty() {
            anyhow::bail!("Last.fm API key not configured");
        }
        params.push(("method".into(), method.into()));
        params.push(("api_key".into(), self.api_key.clone()));
        params.push(("format".into(), "json".into()));
        self.send(self.http.get(&self.root).query(&params)).await
    }

    async fn post(&self, method: &str, params: Vec<(String, String)>, with_session: bool) -> anyhow::Result<Value> {
        let params = self.signed(method, params, with_session)?;
        self.send(self.http.post(&self.root).form(&params)).await
    }

    /// `auth.getToken`: a request token the user approves at [`Lastfm::auth_url`].
    pub async fn get_token(&self) -> anyhow::Result<String> {
        let v = self.get("auth.getToken", Vec::new()).await?;
        v.get("token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .context("Last.fm returned no token")
    }

    pub fn auth_url(&self, token: &str) -> String {
        format!(
            "{AUTH_URL}?api_key={}&token={}",
            urlencoding::encode(&self.api_key),
            urlencoding::encode(token)
        )
    }

    /// `auth.getSession` -> `(session_key, username)`. Until the user approved the token this
    /// fails with an error for which [`is_pending_auth`] is true.
    pub async fn get_session(&self, token: &str) -> anyhow::Result<(String, String)> {
        let v = self
            .get("auth.getSession", vec![("token".into(), token.into())])
            .await?;
        session_from(&v)
    }

    /// `track.updateNowPlaying`. Tracks without artist/title are silently skipped.
    pub async fn now_playing(&self, track: &Track) -> anyhow::Result<()> {
        let Some(mut s) = QueuedScrobble::from_track(track, 0) else {
            return Ok(());
        };
        self.complete(&mut s, track).await;
        let mut params = vec![("artist".into(), s.artist), ("track".into(), s.track)];
        if !s.album.is_empty() {
            params.push(("album".into(), s.album));
        }
        if s.duration > 0 {
            params.push(("duration".into(), s.duration.to_string()));
        }
        self.post("track.updateNowPlaying", params, true).await?;
        Ok(())
    }

    /// `track.scrobble`. Returns why Last.fm ignored any of them (it answers 200 either way).
    async fn submit(&self, batch: &[QueuedScrobble]) -> anyhow::Result<Vec<String>> {
        let v = self.post("track.scrobble", batch_params(batch), true).await?;
        let ignored = ignored_reasons(&v);
        for reason in &ignored {
            tracing::warn!("lastfm: scrobble ignored: {reason}");
        }
        Ok(ignored)
    }

    async fn enqueue(&self, scrobble: QueuedScrobble) -> anyhow::Result<()> {
        let _guard = self.queue_lock.lock().await;
        let mut queue = load_queue(&self.queue_path).await;
        queue.push(scrobble);
        cap_queue(&mut queue);
        save_queue(&self.queue_path, &queue).await
    }

    /// `track.scrobble`. Transient failures (network, 5xx, rate limiting) and an invalid
    /// session queue the scrobble for later; transient ones return `Ok(())`. After a
    /// successful scrobble the queue is flushed too.
    pub async fn scrobble(&self, track: &Track, started_at: i64) -> anyhow::Result<Option<String>> {
        let Some(mut s) = QueuedScrobble::from_track(track, started_at) else {
            tracing::debug!("lastfm: not scrobbling {:?}: missing artist or title", track.id);
            return Ok(None);
        };
        if !self.is_authenticated() {
            anyhow::bail!("not logged in to Last.fm");
        }
        self.complete(&mut s, track).await;
        match self.submit(std::slice::from_ref(&s)).await {
            Ok(ignored) => {
                if let Err(e) = self.flush_queue().await {
                    tracing::debug!("lastfm: flushing scrobble queue failed: {e:#}");
                }
                Ok(ignored.into_iter().next())
            }
            Err(e) if is_transient(&e) => {
                tracing::info!("lastfm: scrobble failed ({e:#}), queued for later");
                self.enqueue(s).await.map(|()| None)
            }
            Err(e) if is_auth_error(&e) => {
                // Keep it so it can be submitted after re-authenticating.
                if let Err(qe) = self.enqueue(s).await {
                    tracing::warn!("lastfm: can't queue scrobble: {qe:#}");
                }
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Submits queued scrobbles in batches of 50. Returns how many were submitted.
    /// Entries older than 14 days are dropped; on a transient failure the rest stays queued.
    pub async fn flush_queue(&self) -> anyhow::Result<usize> {
        if !self.is_authenticated() {
            return Ok(0);
        }
        let _guard = self.queue_lock.lock().await;
        let loaded = load_queue(&self.queue_path).await;
        if loaded.is_empty() {
            return Ok(0);
        }
        let loaded_len = loaded.len();
        let mut queue = prune_queue(loaded, now_unix());
        if queue.len() < loaded_len {
            tracing::info!(
                "lastfm: dropped {} queued scrobbles older than 14 days",
                loaded_len - queue.len()
            );
        }

        let mut submitted = 0;
        let mut result = Ok(());
        while !queue.is_empty() {
            let n = queue.len().min(BATCH_SIZE);
            match self.submit(&queue[..n]).await {
                Ok(_) => {
                    submitted += n;
                    queue.drain(..n);
                }
                Err(e) if is_transient(&e) || is_auth_error(&e) => {
                    result = Err(e);
                    break;
                }
                Err(e) => {
                    // Malformed entries would block the queue forever; drop this batch.
                    tracing::warn!("lastfm: dropping {n} queued scrobbles rejected by Last.fm: {e:#}");
                    queue.drain(..n);
                }
            }
        }

        save_queue(&self.queue_path, &queue).await?;
        if submitted > 0 {
            tracing::info!("lastfm: submitted {submitted} queued scrobbles");
        }
        result.map(|()| submitted)
    }

    /// `track.love` / `track.unlove`.
    pub async fn love(&self, track: &Track, love: bool) -> anyhow::Result<()> {
        let artist = scrobble_artist(&track.artist);
        let title = track.title.trim();
        if artist.is_empty() || title.is_empty() {
            anyhow::bail!("track has no artist or title");
        }
        let params = vec![
            ("artist".into(), artist.to_string()),
            ("track".into(), title.to_string()),
        ];
        let method = if love { "track.love" } else { "track.unlove" };
        self.post(method, params, true).await?;
        Ok(())
    }
}

/// Last.fm scrobble rules: a track qualifies once it is longer than 30 seconds and has been
/// played for half its duration or 4 minutes, whichever comes first. Paused time doesn't count.
/// In instant mode a track qualifies as soon as it starts.
#[derive(Debug, Default)]
pub struct ScrobbleTracker {
    current: Option<Tracked>,
    /// Scrobble as soon as a track starts instead of after half of it.
    instant: bool,
}

#[derive(Debug)]
struct Tracked {
    track_id: String,
    duration_ms: u64,
    started_at: i64,
    listened: Duration,
    scrobbled: bool,
}

const MIN_TRACK_LEN: Duration = Duration::from_secs(30);
const MAX_LISTEN_REQUIRED: Duration = Duration::from_secs(240);

impl ScrobbleTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scrobble tracks as soon as they start (`true`) or by Last.fm's usual rule.
    pub fn set_instant(&mut self, instant: bool) {
        self.instant = instant;
    }

    /// A new track started playing (resets all progress).
    pub fn start(&mut self, track_id: &str, duration_ms: u64, started_at_unix: i64) {
        self.current = Some(Tracked {
            track_id: track_id.to_string(),
            duration_ms,
            started_at: started_at_unix,
            listened: Duration::ZERO,
            scrobbled: false,
        });
    }

    /// Call periodically with the wall time since the previous tick; it only counts while playing.
    pub fn tick(&mut self, playing: bool, elapsed: Duration) {
        if let Some(t) = self.current.as_mut().filter(|_| playing) {
            t.listened = t.listened.saturating_add(elapsed);
        }
    }

    pub fn should_scrobble(&self) -> bool {
        let Some(t) = &self.current else {
            return false;
        };
        let duration = Duration::from_millis(t.duration_ms);
        if self.instant {
            // Unknown lengths count too; known short clips (< 30 s) still don't.
            return !t.scrobbled && (t.duration_ms == 0 || duration > MIN_TRACK_LEN);
        }
        !t.scrobbled && duration > MIN_TRACK_LEN && t.listened >= (duration / 2).min(MAX_LISTEN_REQUIRED)
    }

    /// The length reported by the player, for tracks whose tags didn't have one (they could
    /// never reach "half the song" otherwise).
    pub fn set_duration_if_unknown(&mut self, duration_ms: u64) {
        if let Some(t) = self.current.as_mut().filter(|t| t.duration_ms == 0) {
            t.duration_ms = duration_ms;
        }
    }

    pub fn mark_scrobbled(&mut self) {
        if let Some(t) = self.current.as_mut() {
            t.scrobbled = true;
        }
    }

    /// Whether `track_id` is the tracked track and was scrobbled already.
    pub fn scrobbled(&self, track_id: &str) -> bool {
        self.current
            .as_ref()
            .is_some_and(|t| t.track_id == track_id && t.scrobbled)
    }

    /// `(track_id, started_at)` of the tracked track.
    pub fn current(&self) -> Option<(&str, i64)> {
        self.current.as_ref().map(|t| (t.track_id.as_str(), t.started_at))
    }

    /// Time actually spent playing the current track.
    #[cfg(test)]
    pub fn listened(&self) -> Duration {
        self.current.as_ref().map(|t| t.listened).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Source;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn entry(n: i64) -> QueuedScrobble {
        QueuedScrobble {
            artist: format!("Artist {n}"),
            track: format!("Track {n}"),
            album: if n % 2 == 0 { String::new() } else { "Album".into() },
            duration: 180,
            timestamp: n,
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "multimusic-lastfm-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ))
            .join(name)
    }

    fn track(artist: &str, title: &str) -> Track {
        Track {
            id: "local:/x.flac".into(),
            source: Source::Local,
            title: title.into(),
            artist: artist.into(),
            album: String::new(),
            duration_ms: 200_400,
            track_no: None,
            art: None,
            uri: "/x.flac".into(),
            added_at: 0,
        }
    }

    #[test]
    fn signature_known_vector() {
        let params = [("api_key", "xxx"), ("method", "auth.getSession"), ("token", "yyy")];
        // md5("api_keyxxxmethodauth.getSessiontokenyyyzzz")
        assert_eq!(sign(&params, "zzz"), "75df1fdb6b738160924a52b1732fdde7");
        // Order independent; format/callback excluded.
        let shuffled = [
            ("token", "yyy"),
            ("format", "json"),
            ("method", "auth.getSession"),
            ("callback", "cb"),
            ("api_key", "xxx"),
        ];
        assert_eq!(sign(&shuffled, "zzz"), "75df1fdb6b738160924a52b1732fdde7");
        assert_eq!(sign(&[], ""), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn signed_params_contain_sig_and_format() {
        let fm = Lastfm::new(reqwest::Client::new(), "xxx", "zzz", "", PathBuf::from("q.json"));
        assert!(!fm.is_authenticated());
        let p = fm
            .signed("auth.getSession", vec![("token".into(), "yyy".into())], false)
            .unwrap();
        let get = |k: &str| p.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.as_str());
        assert_eq!(get("api_sig"), Some("75df1fdb6b738160924a52b1732fdde7"));
        assert_eq!(get("format"), Some("json"));
        // Session required but missing.
        assert!(fm.signed("track.love", Vec::new(), true).is_err());
        let fm = Lastfm::new(reqwest::Client::new(), "k", "s", " sk ", PathBuf::from("q.json"));
        assert!(fm.is_authenticated());
        assert!(fm.auth_url("t o").ends_with("?api_key=k&token=t%20o"));
    }

    #[test]
    fn api_errors() {
        let err = parse_response(403, r#"{"error":14,"message":"Unauthorized Token"}"#).unwrap_err();
        assert!(is_pending_auth(&err));
        assert!(!is_transient(&err));
        assert!(err.to_string().contains("14"));

        let err = parse_response(200, r#"{"error":29,"message":"Rate limit exceeded"}"#).unwrap_err();
        assert!(is_transient(&err));
        assert!(!is_pending_auth(&err));

        let err = parse_response(503, "<html>down</html>").unwrap_err();
        assert!(is_transient(&err));
        let err = parse_response(400, "nope").unwrap_err();
        assert!(!is_transient(&err));

        let err = parse_response(403, r#"{"error":9,"message":"Invalid session key"}"#).unwrap_err();
        assert!(is_auth_error(&err));

        assert!(parse_response(200, r#"{"token":"abc"}"#).is_ok());
        assert!(parse_response(200, "not json").is_err());
    }

    #[test]
    fn scrobble_fields() {
        let s = QueuedScrobble::from_track(&track("Daft Punk, Pharrell Williams", " Get Lucky "), 42).unwrap();
        assert_eq!(s.artist, "Daft Punk");
        assert_eq!(s.track, "Get Lucky");
        assert_eq!(s.duration, 200);
        assert_eq!(s.timestamp, 42);
        assert_eq!(scrobble_artist("Simon & Garfunkel"), "Simon & Garfunkel");
        assert!(QueuedScrobble::from_track(&track("", "Title"), 0).is_none());
        assert!(QueuedScrobble::from_track(&track("Artist", " "), 0).is_none());

        let params = batch_params(&[entry(1), entry(2)]);
        let keys: Vec<&str> = params.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "artist[0]",
                "track[0]",
                "timestamp[0]",
                "album[0]",
                "duration[0]",
                "artist[1]",
                "track[1]",
                "timestamp[1]",
                "duration[1]"
            ]
        );
    }

    fn upload(title: &str, uploader: &str) -> Track {
        Track {
            id: "soundcloud:1".into(),
            source: Source::SoundCloud,
            title: title.into(),
            artist: uploader.into(),
            album: String::new(),
            duration_ms: 200_000,
            track_no: None,
            art: None,
            uri: "https://soundcloud.com/x/y".into(),
            added_at: 0,
        }
    }

    #[test]
    fn soundcloud_upload_names() {
        let names = |t: &Track| scrobble_names(t);
        assert_eq!(
            names(&upload("Bladee - Waster [Free DL]", "drainfan99")),
            ("Bladee".into(), "Waster".into())
        );
        assert_eq!(
            names(&upload(
                "Yung Lean ft. Bladee - Hennessy & Sailor Moon (prod. Gud)",
                "sadboys"
            )),
            ("Yung Lean".into(), "Hennessy & Sailor Moon".into())
        );
        // Versions, not "Artist - Song".
        assert_eq!(
            names(&upload("Waster - Slowed", "bladee")),
            ("bladee".into(), "Waster - Slowed".into())
        );
        assert_eq!(
            names(&upload("Be Nice 2 Me - Live", "Bladee")),
            ("Bladee".into(), "Be Nice 2 Me - Live".into())
        );
        // Plain titles, remix brackets kept.
        assert_eq!(
            names(&upload("Waster (Remix)", "bladee")),
            ("bladee".into(), "Waster (Remix)".into())
        );
        // Official releases (with an album) keep their titles; collaborations go under the main
        // artist like Spotify's do. Other sources are left alone.
        let mut official = upload("Artist - Song", "Label");
        official.album = "Album".into();
        assert_eq!(names(&official), ("Label".into(), "Artist - Song".into()));
        let mut collab = upload("Kiss of Death", "Bladee x Uli K");
        collab.album = "Kiss of Death".into();
        assert_eq!(names(&collab), ("Bladee".into(), "Kiss of Death".into()));
        assert_eq!(
            names(&upload("Rat Race", "Yung Lean X Bladee")),
            ("Yung Lean".into(), "Rat Race".into())
        );
        assert_eq!(names(&upload("Song", "Malcolm X")), ("Malcolm X".into(), "Song".into()));
        // "a+b" credits and "(+Guest)" features.
        assert_eq!(
            names(&upload("bladee+Thaiboy Digital - X o n u (+Yung Lean)", "drain gang")),
            ("bladee".into(), "X o n u (feat. Yung Lean)".into())
        );
        assert_eq!(
            names(&upload("Song [+ Ecco2k]", "bladee+ecco2k")),
            ("bladee".into(), "Song (feat. Ecco2k)".into())
        );
        assert_eq!(
            names(&upload("Dog Days", "Florence + the Machine")),
            ("Florence + the Machine".into(), "Dog Days".into())
        );
        assert_eq!(names(&upload("Song", "C++")), ("C++".into(), "Song".into()));
        // Lookups (Spotify, Genius, lyrics) search without the feature.
        assert_eq!(
            crate::integrations::lyrics::song_names(&upload(
                "bladee+Thaiboy Digital - X o n u (+Yung Lean)",
                "drain gang"
            )),
            ("bladee".into(), "X o n u".into())
        );
        assert_eq!(names(&track("A, B", "X - Y")), ("A".into(), "X - Y".into()));
    }

    /// A SoundCloud re-upload is scrobbled under the real artist and title with an album
    /// (from Genius), and a fan upload gets its album from Last.fm.
    #[tokio::test]
    async fn soundcloud_scrobbles_get_real_details() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                // Read the whole request (headers and form body).
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&buf).to_string();
                let line = request.lines().next().unwrap_or("").to_string();
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                let body = request.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(format!("{path} {body}"));
                let reply = if path.starts_with("/api/search/song") && path.contains("Angel") {
                    r#"{"response":{"sections":[{"type":"song","hits":[{"type":"song","result":{"_type":"song","id":7,
                        "title":"Angel with a Shotgun","artist_names":"The Cab","primary_artist":{"name":"The Cab"},
                        "url":"https://genius.com/The-cab-angel-with-a-shotgun-lyrics"}}]}]}}"#
                        .to_string()
                } else if path.starts_with("/api/search/song") {
                    r#"{"response":{"sections":[{"type":"song","hits":[]}]}}"#.to_string()
                } else if path == "/api/songs/7" {
                    r#"{"response":{"song":{"album":{"name":"Symphony Soldier"},"release_date":"2011-08-23"}}}"#
                        .to_string()
                } else if path.contains("method=track.getInfo") {
                    r#"{"track":{"name":"Waster","album":{"artist":"Bladee","title":"Icedancer"}}}"#.to_string()
                } else {
                    r##"{"scrobbles":{"scrobble":{"track":{"#text":"x"},"ignoredMessage":{"code":"0","#text":""}},"@attr":{"accepted":1,"ignored":0}}}"##
                        .to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let genius = std::sync::Arc::new(crate::integrations::genius::Genius::new(http.clone()).with_root(&base));
        let lfm = Lastfm::new(http, "key", "secret", "session", temp_path("queue.json"))
            .with_root(&format!("{base}/2.0/"))
            .with_genius(genius);

        let ignored = lfm
            .scrobble(&upload("Angel With A Shotgun", "Nightcore Reality"), 1_700_000_000)
            .await
            .unwrap();
        assert_eq!(ignored, None);
        lfm.now_playing(&upload("Bladee - Waster [Free DL]", "drainfan99"))
            .await
            .unwrap();

        let seen = seen.lock().unwrap().clone();
        let scrobble = seen
            .iter()
            .find(|r| r.contains("method=track.scrobble"))
            .expect("scrobbled");
        assert!(scrobble.contains("artist%5B0%5D=The+Cab"), "{scrobble}");
        assert!(scrobble.contains("track%5B0%5D=Angel+with+a+Shotgun"), "{scrobble}");
        assert!(scrobble.contains("album%5B0%5D=Symphony+Soldier"), "{scrobble}");
        let playing = seen
            .iter()
            .find(|r| r.contains("method=track.updateNowPlaying"))
            .expect("now playing sent");
        assert!(playing.contains("artist=Bladee"), "{playing}");
        assert!(playing.contains("track=Waster"), "{playing}");
        assert!(playing.contains("album=Icedancer"), "{playing}");
    }

    #[test]
    fn tracker_learns_unknown_lengths_from_the_player() {
        let mut t = ScrobbleTracker::new();
        // Tags without a length: half of "unknown" can never be reached.
        t.start("a", 0, 100);
        t.tick(true, secs(200));
        assert!(!t.should_scrobble());
        t.set_duration_if_unknown(180_000);
        assert!(t.should_scrobble());
        // A known length is kept.
        t.start("b", 300_000, 200);
        t.set_duration_if_unknown(10_000);
        t.tick(true, secs(150));
        assert!(t.should_scrobble());
    }

    #[test]
    fn album_covers() {
        let with: serde_json::Value = serde_json::from_str(
            r##"{"album":{"name":"Bladeecity","image":[{"#text":"","size":"small"},
                {"#text":"https://lastfm.freetls.fastly.net/i/u/300x300/abc.png","size":"extralarge"}]}}"##,
        )
        .unwrap();
        assert!(album_has_cover(&with));
        let star: serde_json::Value = serde_json::from_str(
            r##"{"album":{"image":[{"#text":"https://lastfm.freetls.fastly.net/i/u/300x300/2a96cbd8b46e442fc41c2b86b821562f.png"}]}}"##,
        )
        .unwrap();
        assert!(!album_has_cover(&star));
        assert!(!album_has_cover(&serde_json::json!({"album":{"image":[{"#text":""}]}})));
        assert!(!album_has_cover(&serde_json::json!({})));
    }

    #[test]
    fn ignored_scrobbles_say_why() {
        let one: serde_json::Value = serde_json::from_str(
            r##"{"scrobbles":{"scrobble":{"track":{"corrected":"0","#text":"01 Intro"},
                "artist":{"corrected":"0","#text":"Unknown Artist"},
                "ignoredMessage":{"code":"1","#text":"Artist was ignored"}},
                "@attr":{"ignored":1,"accepted":0}}}"##,
        )
        .unwrap();
        assert_eq!(
            ignored_reasons(&one),
            vec!["“01 Intro”: Last.fm rejected the artist name (check the file's tags)".to_string()]
        );
        let batch: serde_json::Value = serde_json::from_str(
            r##"{"scrobbles":{"scrobble":[
                {"track":{"#text":"A"},"ignoredMessage":{"code":"0","#text":""}},
                {"track":{"#text":"B"},"ignoredMessage":{"code":"5","#text":"Daily limit"}}],
                "@attr":{"ignored":1,"accepted":1}}}"##,
        )
        .unwrap();
        assert_eq!(
            ignored_reasons(&batch),
            vec!["“B”: you've reached Last.fm's daily scrobble limit".to_string()]
        );
        assert!(ignored_reasons(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn tracker_instant_mode() {
        let mut t = ScrobbleTracker::new();
        t.set_instant(true);
        t.start("a", 200_000, 100);
        assert!(t.should_scrobble());
        t.mark_scrobbled();
        assert!(!t.should_scrobble());
        t.tick(true, secs(500));
        assert!(!t.should_scrobble());
        // Unknown length still counts, short clips don't.
        t.start("stream", 0, 200);
        assert!(t.should_scrobble());
        t.start("clip", 20_000, 300);
        assert!(!t.should_scrobble());
    }

    #[test]
    fn tracker_short_track_never_scrobbles() {
        let mut t = ScrobbleTracker::new();
        t.start("a", 30_000, 100);
        t.tick(true, secs(30));
        assert!(!t.should_scrobble());
        t.start("unknown", 0, 100);
        t.tick(true, secs(1000));
        assert!(!t.should_scrobble());
    }

    #[test]
    fn tracker_half_duration_rule() {
        let mut t = ScrobbleTracker::new();
        assert!(!t.should_scrobble());
        assert_eq!(t.current(), None);
        t.start("a", 200_000, 100);
        assert_eq!(t.current(), Some(("a", 100)));
        t.tick(true, secs(99));
        assert!(!t.should_scrobble());
        t.tick(true, secs(1));
        assert!(t.should_scrobble());
    }

    #[test]
    fn tracker_four_minute_cap() {
        let mut t = ScrobbleTracker::new();
        t.start("long", 20 * 60_000, 0);
        t.tick(true, secs(239));
        assert!(!t.should_scrobble());
        t.tick(true, secs(1));
        assert!(t.should_scrobble());
    }

    #[test]
    fn tracker_paused_time_not_counted() {
        let mut t = ScrobbleTracker::new();
        t.start("a", 100_000, 0);
        t.tick(false, secs(500));
        assert!(!t.should_scrobble());
        t.tick(true, secs(30));
        t.tick(false, secs(60));
        assert_eq!(t.listened(), secs(30));
        assert!(!t.should_scrobble());
        t.tick(true, secs(20));
        assert!(t.should_scrobble());
    }

    #[test]
    fn tracker_only_once_and_reset_on_start() {
        let mut t = ScrobbleTracker::new();
        t.start("a", 100_000, 5);
        t.tick(true, secs(60));
        assert!(t.should_scrobble());
        t.mark_scrobbled();
        assert!(!t.should_scrobble());
        t.tick(true, secs(600));
        assert!(!t.should_scrobble());
        // Replaying (new start) counts again.
        t.start("a", 100_000, 500);
        assert!(!t.should_scrobble());
        t.tick(true, secs(50));
        assert!(t.should_scrobble());
        assert_eq!(t.current(), Some(("a", 500)));
    }

    #[test]
    fn prune_drops_old_scrobbles() {
        let now = 10_000_000;
        let queue = vec![
            entry(now - MAX_SCROBBLE_AGE_SECS - 1),
            entry(now - MAX_SCROBBLE_AGE_SECS),
            entry(now - MAX_SCROBBLE_AGE_SECS + 60),
            entry(now - 10),
        ];
        let kept = prune_queue(queue, now);
        assert_eq!(
            kept.iter().map(|s| s.timestamp).collect::<Vec<_>>(),
            [now - MAX_SCROBBLE_AGE_SECS + 60, now - 10]
        );
    }

    #[test]
    fn queue_cap_drops_oldest() {
        let mut q: Vec<_> = (0..QUEUE_CAP as i64 + 5).map(entry).collect();
        cap_queue(&mut q);
        assert_eq!(q.len(), QUEUE_CAP);
        assert_eq!(q[0].timestamp, 5);
    }

    #[tokio::test]
    async fn queue_persistence_round_trip() {
        let path = temp_path("scrobbles.json");
        assert!(load_queue(&path).await.is_empty());

        let queue: Vec<_> = (1..=3).map(entry).collect();
        save_queue(&path, &queue).await.unwrap();
        assert_eq!(load_queue(&path).await, queue);
        assert!(!path.with_extension("json.tmp").exists());

        // Missing optional fields default.
        tokio::fs::write(&path, r#"[{"artist":"a","track":"t","timestamp":5}]"#)
            .await
            .unwrap();
        let q = load_queue(&path).await;
        assert_eq!(q[0].album, "");
        assert_eq!(q[0].duration, 0);

        // Corrupt file = empty queue.
        tokio::fs::write(&path, "{oops").await.unwrap();
        assert!(load_queue(&path).await.is_empty());

        // Saving an empty queue removes the file.
        save_queue(&path, &[]).await.unwrap();
        assert!(!path.exists());

        // Enqueue through the client (no network involved).
        let fm = Lastfm::new(reqwest::Client::new(), "k", "s", "", path.clone());
        fm.enqueue(entry(7)).await.unwrap();
        fm.enqueue(entry(8)).await.unwrap();
        assert_eq!(load_queue(&path).await, vec![entry(7), entry(8)]);
        // Not authenticated: flushing is a no-op that keeps the queue.
        assert_eq!(fm.flush_queue().await.unwrap(), 0);
        assert_eq!(load_queue(&path).await.len(), 2);

        if let Some(dir) = path.parent() {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}
