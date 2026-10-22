//! Small async Spotify Web API client: user profile, playlist and Liked Songs import,
//! track search and (un)liking tracks. Playback is handled by librespot, not here.
//!
//! Spotify's "February 2026 Web API Dev Mode changes" renamed several endpoints and fields,
//! but some client ids still get the old shapes, so everything here accepts both:
//! - `GET /playlists/{id}/items` (new) vs `GET /playlists/{id}/tracks` (old),
//! - `items.total` (new) vs `tracks.total` (old) in playlist objects,
//! - `item` (new) vs `track` (old) in playlist / saved-track item objects,
//! - `PUT|DELETE /me/library?uris=` (new) vs `PUT|DELETE /me/tracks?ids=` (old).
//!
//! The caller owns the OAuth token and refreshes it; when a request fails because the token
//! expired, [`is_unauthorized`] returns true for the returned error.

use std::fmt;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{header, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

use crate::model::{ArtistHit, CollectionHit, Source, Track};

const API_BASE: &str = "https://api.spotify.com/v1";

/// How often a 429 response is retried before giving up.
const MAX_RATE_LIMIT_RETRIES: u32 = 5;
/// How often a 5xx response (or a transport error) is retried before giving up.
const MAX_SERVER_RETRIES: u32 = 2;
/// Wait used when a 429 carries no (parsable) `Retry-After` header.
const DEFAULT_RETRY_AFTER_SECS: u64 = 2;
/// Never sleep longer than this for a single 429.
const MAX_RETRY_AFTER_SECS: u64 = 30;
/// A `Retry-After` beyond this means we are locked out for a long time: fail right away
/// instead of hammering the API with capped waits.
const GIVE_UP_RETRY_AFTER_SECS: u64 = 600;
/// Interactive requests (search) only wait out rate limits this short; longer ones fail
/// right away so the UI can say so instead of spinning.
const INTERACTIVE_MAX_RETRY_AFTER_SECS: u64 = 2;
/// Safety net against pagination loops.
const MAX_PAGES: usize = 2000;

/// How hard a request tries before giving up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Patience {
    /// Background work (library import): waits out rate limits.
    Patient,
    /// Something the user is waiting for: at most one short retry.
    Interactive,
}
/// Max pages fetched to fill a single search.
const MAX_SEARCH_PAGES: usize = 10;
/// Spotify's largest page size for search.
const MAX_SEARCH_LIMIT: u32 = 50;
/// Search page size that Development Mode apps are still allowed to request.
const DEV_MODE_SEARCH_LIMIT: u32 = 10;
/// Max length of the response body quoted in error messages.
const ERROR_SNIPPET_CHARS: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SpotifyUser {
    pub id: String,
    pub display_name: String,
    pub image: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SpotifyPlaylistMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    pub art: Option<String>,
    /// Number of items in the playlist (0 when Spotify doesn't tell us).
    pub total: u32,
    /// Changes whenever the playlist changes; use it to skip re-importing unchanged playlists.
    pub snapshot_id: String,
    /// Owner display name.
    pub owner: String,
}

/// A non-2xx response from the Web API. Wrapped in the `anyhow::Error`s returned by
/// [`SpotifyApi`]; use [`error_status`] / [`is_unauthorized`] to inspect them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub method: String,
    /// Request path relative to the API base (e.g. `/me/playlists?limit=50`).
    pub path: String,
    /// Spotify's error message, or a snippet of the response body.
    pub message: String,
}

impl ApiError {
    fn new(status: StatusCode, method: &Method, path: &str, body: &str) -> Self {
        Self {
            status: status.as_u16(),
            method: method.to_string(),
            path: path.to_owned(),
            message: error_message(body),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Spotify API {} {} failed with HTTP {}",
            self.method, self.path, self.status
        )?;
        if let Some(reason) = StatusCode::from_u16(self.status)
            .ok()
            .and_then(|s| s.canonical_reason())
        {
            write!(f, " {reason}")?;
        }
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

/// HTTP status of the Spotify API error inside `err`, if it is one.
pub fn error_status(err: &anyhow::Error) -> Option<u16> {
    err.chain().find_map(|e| e.downcast_ref::<ApiError>()).map(|e| e.status)
}

/// True when `err` was caused by a 401 (expired / revoked access token): refresh and retry.
pub fn is_unauthorized(err: &anyhow::Error) -> bool {
    match error_status(err) {
        Some(status) => status == 401,
        None => format!("{err:#}").contains("HTTP 401"),
    }
}

/// Async Spotify Web API client. Cheap to clone.
#[derive(Debug, Clone)]
pub struct SpotifyApi {
    http: reqwest::Client,
    /// `https://api.spotify.com/v1` (overridable in tests).
    base: String,
}

impl SpotifyApi {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            base: API_BASE.to_owned(),
        }
    }

    #[cfg(test)]
    fn with_base(http: reqwest::Client, base: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    /// The current user's profile (`GET /me`).
    pub async fn me(&self, token: &str) -> Result<SpotifyUser> {
        let v = self
            .get_json(token, &format!("{}/me", self.base))
            .await
            .context("fetching Spotify user profile")?;
        parse_user(&v).ok_or_else(|| anyhow!("Spotify returned a user profile without an id"))
    }

    /// Every playlist in the user's library (owned and followed), from `GET /me/playlists`.
    pub async fn playlists(&self, token: &str) -> Result<Vec<SpotifyPlaylistMeta>> {
        let url = format!("{}/me/playlists", self.base);
        let raw = self
            .get_paged(token, &url, &[50, 20])
            .await
            .context("fetching Spotify playlists")?;
        let lists: Vec<_> = raw.iter().filter_map(parse_playlist_meta).collect();
        debug!(count = lists.len(), "fetched Spotify playlists");
        Ok(lists)
    }

    /// All tracks of a playlist in playlist order. Local files and podcast episodes are skipped.
    /// `playlist_id` may be a bare id, a `spotify:playlist:` URI or an open.spotify.com URL.
    pub async fn playlist_tracks(&self, token: &str, playlist_id: &str) -> Result<Vec<Track>> {
        let id = normalize_playlist_id(playlist_id);
        if id.is_empty() {
            bail!("invalid Spotify playlist id {playlist_id:?}");
        }
        let enc = urlencoding::encode(&id);
        let items_url = format!("{}/playlists/{enc}/items", self.base);
        let raw = match self.get_paged(token, &items_url, &[100, 50]).await {
            Ok(raw) => raw,
            Err(items_err) if matches!(error_status(&items_err), Some(400 | 403 | 404)) => {
                warn!(playlist = %id, error = %items_err, "Spotify /items endpoint refused, falling back to /tracks");
                let tracks_url = format!("{}/playlists/{enc}/tracks", self.base);
                match self.get_paged(token, &tracks_url, &[100, 50]).await {
                    Ok(raw) => raw,
                    Err(tracks_err) => {
                        let summary =
                            format!("fetching Spotify playlist {id} (/items: {items_err}; /tracks: {tracks_err})");
                        // A 404 from the legacy endpoint is less telling than what /items said.
                        let primary = if error_status(&tracks_err) == Some(404) {
                            items_err
                        } else {
                            tracks_err
                        };
                        return Err(primary.context(summary));
                    }
                }
            }
            Err(e) => return Err(e.context(format!("fetching Spotify playlist {id}"))),
        };
        let tracks: Vec<Track> = raw.iter().filter_map(parse_playlist_item).collect();
        debug!(playlist = %id, items = raw.len(), tracks = tracks.len(), "fetched Spotify playlist");
        Ok(tracks)
    }

    /// The user's Liked Songs (`GET /me/tracks`), newest first.
    pub async fn liked_tracks(&self, token: &str) -> Result<Vec<Track>> {
        let url = format!("{}/me/tracks", self.base);
        let raw = self
            .get_paged(token, &url, &[50, 20])
            .await
            .context("fetching Spotify Liked Songs")?;
        let mut tracks: Vec<Track> = raw.iter().filter_map(parse_playlist_item).collect();
        // Spotify already returns newest first; the stable sort only guards against surprises.
        tracks.sort_by_key(|t| std::cmp::Reverse(t.added_at));
        debug!(count = tracks.len(), "fetched Spotify Liked Songs");
        Ok(tracks)
    }

    /// Track search. Returns at most `limit` (capped at 50) tracks.
    pub async fn search(&self, token: &str, query: &str, limit: u32) -> Result<Vec<Track>> {
        let query = query.trim();
        let limit = limit.min(MAX_SEARCH_LIMIT);
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let base = format!("{}/search?type=track&q={}", self.base, urlencoding::encode(query));
        let mut url = with_param(&base, "limit", limit);
        let mut page = match self.get_json(token, &url).await {
            Ok(page) => page,
            Err(e) if error_status(&e) == Some(400) && limit > DEV_MODE_SEARCH_LIMIT => {
                warn!(limit, error = %e, "Spotify search refused the page size, retrying with {DEV_MODE_SEARCH_LIMIT}");
                url = with_param(&base, "limit", DEV_MODE_SEARCH_LIMIT);
                self.get_json(token, &url)
                    .await
                    .with_context(|| format!("searching Spotify for {query:?}"))?
            }
            Err(e) => return Err(e.context(format!("searching Spotify for {query:?}"))),
        };

        let mut out: Vec<Track> = Vec::new();
        for pages in 1.. {
            let tracks_page = page.get_mut("tracks").map(Value::take).unwrap_or(Value::Null);
            let (items, next) = take_page(tracks_page);
            for t in items.iter().filter_map(|t| parse_track(t, None)) {
                if !out.iter().any(|o| o.id == t.id) {
                    out.push(t);
                }
            }
            if out.len() >= limit as usize || pages >= MAX_SEARCH_PAGES {
                break;
            }
            let Some(next) = next.and_then(|n| self.rebase_next(&n)).filter(|n| *n != url) else {
                break;
            };
            match self.get_json(token, &next).await {
                Ok(p) => {
                    page = p;
                    url = next;
                }
                Err(e) if is_unauthorized(&e) => return Err(e.context(format!("searching Spotify for {query:?}"))),
                Err(e) => {
                    warn!(error = %e, "Spotify search: failed to fetch more results, returning what we have");
                    break;
                }
            }
        }
        out.truncate(limit as usize);
        Ok(out)
    }

    /// Tracks and artists in one request (keeps rate-limit use low). First page only.
    pub async fn search_with_artists(
        &self,
        token: &str,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<Track>, Vec<ArtistHit>)> {
        let hits = self.search_types(token, query, "track,artist", limit).await?;
        Ok((hits.tracks, hits.artists))
    }

    /// Songs, artists, albums and public playlists matching `query`.
    pub async fn search_all(&self, token: &str, query: &str, limit: u32) -> Result<SearchHits> {
        self.search_types(token, query, "track,artist,album,playlist", limit)
            .await
    }

    async fn search_types(&self, token: &str, query: &str, types: &str, limit: u32) -> Result<SearchHits> {
        let query = query.trim();
        let limit = limit.min(MAX_SEARCH_LIMIT);
        if query.is_empty() || limit == 0 {
            return Ok(SearchHits::default());
        }
        let base = format!("{}/search?type={types}&q={}", self.base, urlencoding::encode(query));
        let first = with_param(&base, "limit", limit);
        let page = match self.get_json_with(token, &first, Patience::Interactive).await {
            Ok(page) => page,
            Err(e) if error_status(&e) == Some(400) && limit > DEV_MODE_SEARCH_LIMIT => self
                .get_json_with(
                    token,
                    &with_param(&base, "limit", DEV_MODE_SEARCH_LIMIT),
                    Patience::Interactive,
                )
                .await
                .with_context(|| format!("searching Spotify for {query:?}"))?,
            Err(e) => return Err(e.context(format!("searching Spotify for {query:?}"))),
        };
        let items = |key: &str| {
            page.get(key)
                .and_then(|p| p.get("items"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let mut tracks: Vec<Track> = Vec::new();
        for t in items("tracks").iter().filter_map(|t| parse_track(t, None)) {
            if !tracks.iter().any(|o| o.id == t.id) {
                tracks.push(t);
            }
        }
        Ok(SearchHits {
            tracks,
            artists: items("artists").iter().filter_map(parse_artist_hit).collect(),
            albums: items("albums").iter().filter_map(parse_album_hit).collect(),
            // Spotify lists playlists it can't show as null.
            playlists: items("playlists").iter().filter_map(parse_playlist_hit).collect(),
        })
    }

    /// Save (`liked = true`) or remove a track from the user's Liked Songs.
    /// `track_uri` is a `spotify:track:<id>` URI (a bare id or open.spotify.com URL also works).
    pub async fn set_liked(&self, token: &str, track_uri: &str, liked: bool) -> Result<()> {
        let (uri, id) =
            normalize_track_uri(track_uri).ok_or_else(|| anyhow!("not a Spotify track URI: {track_uri:?}"))?;
        let method = if liked { Method::PUT } else { Method::DELETE };
        let action = if liked { "saving" } else { "removing" };

        let url = format!("{}/me/library?uris={}", self.base, urlencoding::encode(&uri));
        match self.send(method.clone(), &url, token).await {
            Ok(_) => Ok(()),
            Err(e) if matches!(error_status(&e), Some(400 | 404 | 405)) => {
                warn!(%uri, error = %e, "Spotify /me/library refused, falling back to /me/tracks");
                let url = format!("{}/me/tracks?ids={}", self.base, urlencoding::encode(&id));
                self.send(method, &url, token)
                    .await
                    .map(|_| ())
                    .with_context(|| format!("{action} Spotify track {uri} (/me/library: {e})"))
            }
            Err(e) => Err(e.context(format!("{action} Spotify track {uri}"))),
        }
    }

    /// Save (`saved = true`) or remove an album (by id) in the user's Spotify library.
    pub async fn set_album_saved(&self, token: &str, album_id: &str, saved: bool) -> Result<()> {
        let uri = format!("spotify:album:{album_id}");
        let method = if saved { Method::PUT } else { Method::DELETE };
        let action = if saved { "saving" } else { "removing" };
        let url = format!("{}/me/library?uris={}", self.base, urlencoding::encode(&uri));
        match self.send(method.clone(), &url, token).await {
            Ok(_) => Ok(()),
            Err(e) if matches!(error_status(&e), Some(400 | 404 | 405)) => {
                warn!(%uri, error = %e, "Spotify /me/library refused, falling back to /me/albums");
                let url = format!("{}/me/albums?ids={}", self.base, urlencoding::encode(album_id));
                self.send(method, &url, token)
                    .await
                    .map(|_| ())
                    .with_context(|| format!("{action} Spotify album {uri} (/me/library: {e})"))
            }
            Err(e) => Err(e.context(format!("{action} Spotify album {uri}"))),
        }
    }

    // ---- plumbing -------------------------------------------------------------------------

    /// GETs `url` and parses the JSON body (an empty body becomes `Value::Null`).
    async fn get_json(&self, token: &str, url: &str) -> Result<Value> {
        self.get_json_with(token, url, Patience::Patient).await
    }

    async fn get_json_with(&self, token: &str, url: &str, patience: Patience) -> Result<Value> {
        let body = self.send_with(Method::GET, url, token, patience).await?;
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).with_context(|| {
            format!(
                "Spotify API GET {}: invalid JSON response",
                display_path(&self.base, url)
            )
        })
    }

    /// GETs the paging object at `base`, trying each page size in `limits` until one is
    /// accepted (Development Mode apps get a 400 for page sizes above their maximum), then
    /// follows `next` links. Returns every element of every page's `items`.
    async fn get_paged(&self, token: &str, base: &str, limits: &[u32]) -> Result<Vec<Value>> {
        let mut first = None;
        for (i, &limit) in limits.iter().enumerate() {
            let url = with_param(base, "limit", limit);
            match self.get_json(token, &url).await {
                Ok(page) => {
                    first = Some((url, page));
                    break;
                }
                Err(e) if error_status(&e) == Some(400) && i + 1 < limits.len() => {
                    warn!(limit, error = %e, "Spotify refused the page size, retrying with a smaller one");
                }
                Err(e) => return Err(e),
            }
        }
        let (mut url, mut page) = first.ok_or_else(|| anyhow!("no page size to request"))?;

        let mut out = Vec::new();
        for pages in 1.. {
            let (items, next) = take_page(page);
            out.extend(items);
            let Some(next) = next.and_then(|n| self.rebase_next(&n)).filter(|n| *n != url) else {
                break;
            };
            if pages >= MAX_PAGES {
                warn!(%url, "Spotify pagination exceeded {MAX_PAGES} pages, stopping");
                break;
            }
            page = self.get_json(token, &next).await?;
            url = next;
        }
        Ok(out)
    }

    /// Only follow `next` links that point at the API, so the token is never sent elsewhere.
    fn rebase_next(&self, next: &str) -> Option<String> {
        if next.starts_with(&self.base) {
            return Some(next.to_owned());
        }
        let i = next.find("/v1/")?;
        Some(format!("{}{}", self.base, &next[i + 3..]))
    }

    /// Sends a request and returns the body of a 2xx response. Retries 429s (honouring
    /// `Retry-After`) and 5xx / transport errors; other statuses become an [`ApiError`].
    async fn send(&self, method: Method, url: &str, token: &str) -> Result<String> {
        self.send_with(method, url, token, Patience::Patient).await
    }

    async fn send_with(&self, method: Method, url: &str, token: &str, patience: Patience) -> Result<String> {
        let path = display_path(&self.base, url);
        let (max_rate_limited, max_wait, max_failures) = match patience {
            Patience::Patient => (MAX_RATE_LIMIT_RETRIES, GIVE_UP_RETRY_AFTER_SECS, MAX_SERVER_RETRIES),
            Patience::Interactive => (1, INTERACTIVE_MAX_RETRY_AFTER_SECS, 1),
        };
        let mut rate_limited = 0;
        let mut failures = 0;
        loop {
            let mut req = self
                .http
                .request(method.clone(), url)
                .bearer_auth(token)
                .header(header::ACCEPT, "application/json");
            if method != Method::GET {
                // Spotify answers 411 to body-less PUTs without a Content-Length.
                req = req.header(header::CONTENT_LENGTH, "0");
            }
            debug!(%method, %path, "Spotify API request");

            let resp = match req.send().await {
                Ok(resp) => resp,
                Err(e) if failures < max_failures && is_transient(&e) => {
                    failures += 1;
                    let wait = backoff(failures);
                    warn!(%method, %path, error = %e, "Spotify API request failed, retrying in {wait:?}");
                    tokio::time::sleep(wait).await;
                    continue;
                }
                Err(e) => {
                    return Err(anyhow::Error::new(e).context(format!("Spotify API {method} {path}: request failed")))
                }
            };

            let status = resp.status();
            let mut retry_after = None;
            if status == StatusCode::TOO_MANY_REQUESTS {
                let wait = retry_after_secs(resp.headers());
                retry_after = Some(wait);
                if rate_limited < max_rate_limited && wait <= max_wait {
                    rate_limited += 1;
                    let wait = wait.min(MAX_RETRY_AFTER_SECS);
                    warn!(%method, %path, attempt = rate_limited, "Spotify API rate limited, retrying in {wait}s");
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                }
            } else if status.is_server_error() && failures < max_failures {
                failures += 1;
                let wait = backoff(failures);
                warn!(%method, %path, %status, "Spotify API server error, retrying in {wait:?}");
                tokio::time::sleep(wait).await;
                continue;
            }

            let body = match resp.text().await {
                Ok(body) => body,
                Err(e) if status.is_success() && failures < max_failures => {
                    failures += 1;
                    let wait = backoff(failures);
                    warn!(%method, %path, error = %e, "reading Spotify API response failed, retrying in {wait:?}");
                    tokio::time::sleep(wait).await;
                    continue;
                }
                Err(e) if status.is_success() => {
                    return Err(
                        anyhow::Error::new(e).context(format!("Spotify API {method} {path}: reading response failed"))
                    )
                }
                // The status is what matters for errors; the body is only decoration.
                Err(_) => String::new(),
            };
            if status.is_success() {
                return Ok(body);
            }

            let mut err = ApiError::new(status, &method, &path, &body);
            if let Some(secs) = retry_after {
                err.message = format!("{} (rate limited, retry after {secs}s)", err.message)
                    .trim_start()
                    .to_owned();
            }
            debug!(error = %err, "Spotify API error");
            return Err(err.into());
        }
    }
}

fn is_transient(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

/// 500 ms, 1 s, 2 s, ...
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64 << attempt.saturating_sub(1).min(6))
}

/// `Retry-After` in seconds (only the delta-seconds form; Spotify doesn't send HTTP dates).
fn retry_after_secs(headers: &header::HeaderMap) -> u64 {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| s.ceil() as u64)
        .unwrap_or(DEFAULT_RETRY_AFTER_SECS)
}

/// The URL relative to the API base, shortened for log and error messages.
fn display_path(base: &str, url: &str) -> String {
    truncate_chars(url.strip_prefix(base).unwrap_or(url), 200)
}

/// Spotify's error message (`{"error":{"status":401,"message":"..."}}` or the OAuth style
/// `{"error":"...","error_description":"..."}`), else a snippet of the raw body.
fn error_message(body: &str) -> String {
    let body = body.trim();
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let msg = v
            .pointer("/error/message")
            .or_else(|| v.get("error_description"))
            .or_else(|| v.get("error"))
            .or_else(|| v.get("message"))
            .and_then(Value::as_str);
        if let Some(msg) = msg.filter(|m| !m.is_empty()) {
            return truncate_chars(msg, ERROR_SNIPPET_CHARS);
        }
    }
    truncate_chars(
        &body.split_whitespace().collect::<Vec<_>>().join(" "),
        ERROR_SNIPPET_CHARS,
    )
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

fn with_param(url: &str, key: &str, value: impl fmt::Display) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}{key}={value}")
}

/// Splits a paging object into its `items` and `next` link.
fn take_page(mut page: Value) -> (Vec<Value>, Option<String>) {
    let next = page
        .get("next")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let items = match page.get_mut("items").map(Value::take) {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    (items, next)
}

/// `spotify:playlist:<id>`, `https://open.spotify.com/playlist/<id>?si=..` or `<id>` → `<id>`.
fn normalize_playlist_id(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("spotify:playlist:").unwrap_or(s);
    let s = match s.find("/playlist/") {
        Some(i) => &s[i + "/playlist/".len()..],
        None => s,
    };
    s.split(['?', '#', '/']).next().unwrap_or("").to_owned()
}

/// Returns `(spotify:track:<id>, <id>)` for a track URI, open.spotify.com URL or bare id.
fn normalize_track_uri(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    let id = if let Some(id) = s.strip_prefix("spotify:track:") {
        id
    } else if let Some(i) = s.find("/track/") {
        s[i + "/track/".len()..].split(['?', '#', '/']).next().unwrap_or("")
    } else if !s.contains(':') {
        s
    } else {
        return None;
    };
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some((format!("spotify:track:{id}"), id.to_owned()))
}

// ---- JSON mapping ---------------------------------------------------------------------------

/// Maps a Spotify track object to a [`Track`]. Returns `None` for nulls, episodes and local
/// files. `added_at` is the RFC 3339 timestamp from the surrounding playlist/library item.
pub fn parse_track(v: &Value, added_at: Option<&str>) -> Option<Track> {
    if !v.is_object() || v.get("is_local").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    match v.get("type").and_then(Value::as_str) {
        Some("track") | None => {}
        Some(_) => return None,
    }
    let uri = match v.get("uri").and_then(Value::as_str) {
        Some(uri) => uri.to_owned(),
        None => format!("spotify:track:{}", v.get("id")?.as_str()?),
    };
    // Also rejects `spotify:local:...` and `spotify:episode:...`.
    if uri.strip_prefix("spotify:track:").is_none_or(str::is_empty) {
        return None;
    }

    let artist = v
        .get("artists")
        .and_then(Value::as_array)
        .map(|artists| {
            artists
                .iter()
                .filter_map(|a| a.get("name").and_then(Value::as_str))
                .filter(|n| !n.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    Some(Track {
        album_artist: String::new(),
        disc_no: None,
        id: uri.clone(),
        source: Source::Spotify,
        title: str_field(v, "name"),
        artist,
        album: v
            .pointer("/album/name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        duration_ms: v.get("duration_ms").and_then(as_u64_lenient).unwrap_or(0),
        track_no: v
            .get("track_number")
            .and_then(as_u64_lenient)
            .filter(|n| *n > 0)
            .and_then(|n| u32::try_from(n).ok()),
        art: v.pointer("/album/images").and_then(pick_image),
        uri,
        added_at: added_at.and_then(parse_rfc3339).unwrap_or(0),
    })
}

/// Maps a playlist item / saved track object (`{"added_at": .., "item"|"track": {..}}`).
pub fn parse_playlist_item(v: &Value) -> Option<Track> {
    if v.get("is_local").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let added_at = v.get("added_at").and_then(Value::as_str);
    // New shape uses `item`, old uses `track`.
    let inner = ["item", "track"]
        .iter()
        .filter_map(|k| v.get(*k))
        .find(|t| t.is_object())?;
    parse_track(inner, added_at)
}

/// Maps a (simplified) playlist object from `/me/playlists`.
pub fn parse_playlist_meta(v: &Value) -> Option<SpotifyPlaylistMeta> {
    let id = v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let art = v
        .get("images")
        .and_then(Value::as_array)
        .and_then(|imgs| imgs.iter().find_map(|i| i.get("url").and_then(Value::as_str)))
        .map(str::to_owned);
    // New shape: `items: {href, total}`; old shape: `tracks: {href, total}`.
    let total = ["items", "tracks"]
        .iter()
        .find_map(|k| v.get(*k)?.get("total").and_then(as_u64_lenient))
        .map(|t| u32::try_from(t).unwrap_or(u32::MAX))
        .unwrap_or(0);
    let owner = v
        .pointer("/owner/display_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| v.pointer("/owner/id").and_then(Value::as_str))
        .unwrap_or("")
        .to_owned();
    Some(SpotifyPlaylistMeta {
        id: id.to_owned(),
        name: str_field(v, "name"),
        description: clean_description(&str_field(v, "description")),
        art,
        total,
        snapshot_id: str_field(v, "snapshot_id"),
        owner,
    })
}

/// Maps the `/me` user object.
pub fn parse_user(v: &Value) -> Option<SpotifyUser> {
    let id = v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let display_name = v
        .get("display_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(id)
        .to_owned();
    Some(SpotifyUser {
        id: id.to_owned(),
        display_name,
        image: v.get("images").and_then(pick_image),
    })
}

/// Maps a Web API artist object to a search hit.
/// What a search found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchHits {
    pub tracks: Vec<Track>,
    pub artists: Vec<ArtistHit>,
    pub albums: Vec<CollectionHit>,
    pub playlists: Vec<CollectionHit>,
}

/// An album in search results.
pub fn parse_album_hit(v: &Value) -> Option<CollectionHit> {
    let id = v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let title = v
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let by = v
        .get("artists")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    Some(CollectionHit {
        key: format!("spotify:album:{id}"),
        title: title.to_string(),
        by,
        image: v.get("images").and_then(pick_image),
        source: Source::Spotify,
        album: true,
        songs: v.get("total_tracks").and_then(as_u64_lenient).map(|n| n as u32),
        year: v
            .get("release_date")
            .and_then(Value::as_str)
            .and_then(|d| d.get(..4))
            .and_then(|y| y.parse().ok()),
    })
}

/// A public playlist in search results.
pub fn parse_playlist_hit(v: &Value) -> Option<CollectionHit> {
    let id = v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let title = v
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let songs = ["tracks", "items"]
        .iter()
        .find_map(|k| v.get(k).and_then(|t| t.get("total")).and_then(as_u64_lenient));
    Some(CollectionHit {
        key: format!("spotify:playlist:{id}"),
        title: title.to_string(),
        by: v
            .pointer("/owner/display_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        image: v.get("images").and_then(pick_image),
        source: Source::Spotify,
        album: false,
        songs: songs.map(|n| n as u32),
        year: None,
    })
}

pub fn parse_artist_hit(v: &Value) -> Option<ArtistHit> {
    let id = v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let name = v.get("name").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let followers = v.get("followers").and_then(|f| f.get("total")).and_then(Value::as_u64);
    let genre = v
        .get("genres")
        .and_then(Value::as_array)
        .and_then(|g| g.first())
        .and_then(Value::as_str);
    let subtitle = match (followers, genre) {
        (Some(n), _) => format!("{} followers", crate::model::human_count(n)),
        (None, Some(g)) => {
            let mut c = g.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        }
        (None, None) => "Artist".into(),
    };
    Some(ArtistHit {
        key: format!("spotify:artist:{id}"),
        name: name.to_string(),
        image: v.get("images").and_then(pick_image),
        source: Source::Spotify,
        subtitle,
    })
}

/// Picks the image closest to 300 px wide: the smallest one at least 250 px wide, else the
/// largest one. Images without a known width are only used when no width is known at all.
fn pick_image(images: &Value) -> Option<String> {
    let images: Vec<(&str, Option<u64>)> = images
        .as_array()?
        .iter()
        .filter_map(|i| {
            let url = i.get("url").and_then(Value::as_str).filter(|u| !u.is_empty())?;
            Some((url, i.get("width").and_then(as_u64_lenient)))
        })
        .collect();
    let with_width = || images.iter().filter_map(|&(u, w)| Some((u, w?)));
    with_width()
        .filter(|&(_, w)| w >= 250)
        .min_by_key(|&(_, w)| w)
        .or_else(|| with_width().max_by_key(|&(_, w)| w))
        .map(|(u, _)| u)
        .or_else(|| images.first().map(|&(u, _)| u))
        .map(str::to_owned)
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_owned()
}

fn as_u64_lenient(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64))
}

/// Playlist descriptions come HTML-escaped and may contain `<a href=..>` links.
fn clean_description(s: &str) -> String {
    let mut text = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    decode_entities(&text).trim().to_owned()
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest
            .find(';')
            .filter(|&end| end <= 10)
            .and_then(|end| Some((decode_entity(&rest[1..end])?, end)));
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code)?
        }
    })
}

/// Parses an RFC 3339 timestamp (`2024-05-01T12:34:56Z`, with optional fractional seconds and
/// a `Z` or `±HH:MM` offset) into unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = s.get(from..from + len)?;
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix(['.', ',']) {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &frac[digits..];
    }
    let offset = match rest {
        // RFC 3339 requires an offset; treat a missing one as UTC rather than failing.
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let off = &rest[1..];
            let (h, m) = match off.len() {
                5 if off.as_bytes()[2] == b':' => (off.get(0..2)?, off.get(3..5)?),
                4 => (off.get(0..2)?, off.get(2..4)?),
                2 => (off, "00"),
                _ => return None,
            };
            if !h.bytes().chain(m.bytes()).all(|c| c.is_ascii_digit()) {
                return None;
            }
            let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
            if h > 23 || m > 59 {
                return None;
            }
            sign * (h * 3600 + m * 60)
        }
    };
    let days = crate::clock::days_from_civil(year, month as u32, day as u32);
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn album() -> Value {
        json!({
            "album_type": "album",
            "artists": [{
                "external_urls": {"spotify": "https://open.spotify.com/artist/4tZwfgrHOc3mvqYlEYSvVi"},
                "href": "https://api.spotify.com/v1/artists/4tZwfgrHOc3mvqYlEYSvVi",
                "id": "4tZwfgrHOc3mvqYlEYSvVi",
                "name": "Daft Punk",
                "type": "artist",
                "uri": "spotify:artist:4tZwfgrHOc3mvqYlEYSvVi"
            }],
            "external_urls": {"spotify": "https://open.spotify.com/album/4m2880jivSbbyEGAKfITCa"},
            "href": "https://api.spotify.com/v1/albums/4m2880jivSbbyEGAKfITCa",
            "id": "4m2880jivSbbyEGAKfITCa",
            "images": [
                {"height": 640, "url": "https://i.scdn.co/image/ab67616d0000b2739b9b36b0e22870b9f542d937", "width": 640},
                {"height": 300, "url": "https://i.scdn.co/image/ab67616d00001e029b9b36b0e22870b9f542d937", "width": 300},
                {"height": 64, "url": "https://i.scdn.co/image/ab67616d000048519b9b36b0e22870b9f542d937", "width": 64}
            ],
            "name": "Random Access Memories",
            "release_date": "2013-05-17",
            "release_date_precision": "day",
            "total_tracks": 13,
            "type": "album",
            "uri": "spotify:album:4m2880jivSbbyEGAKfITCa"
        })
    }

    /// A track object as returned before the February 2026 changes.
    fn old_track() -> Value {
        json!({
            "album": album(),
            "artists": [
                {"id": "4tZwfgrHOc3mvqYlEYSvVi", "name": "Daft Punk", "type": "artist", "uri": "spotify:artist:4tZwfgrHOc3mvqYlEYSvVi"},
                {"id": "2RdwBSPQiwcmiDo9kixcl8", "name": "Pharrell Williams", "type": "artist", "uri": "spotify:artist:2RdwBSPQiwcmiDo9kixcl8"},
                {"id": "3yDIp0kaq9EFKe07X1X2rz", "name": "Nile Rodgers", "type": "artist", "uri": "spotify:artist:3yDIp0kaq9EFKe07X1X2rz"}
            ],
            "available_markets": ["DE", "US"],
            "disc_number": 1,
            "duration_ms": 369626,
            "episode": false,
            "explicit": false,
            "external_ids": {"isrc": "USQX91300108"},
            "external_urls": {"spotify": "https://open.spotify.com/track/69kOkLUCkxIZYexIgSG8rq"},
            "href": "https://api.spotify.com/v1/tracks/69kOkLUCkxIZYexIgSG8rq",
            "id": "69kOkLUCkxIZYexIgSG8rq",
            "is_local": false,
            "name": "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
            "popularity": 82,
            "preview_url": null,
            "track": true,
            "track_number": 8,
            "type": "track",
            "uri": "spotify:track:69kOkLUCkxIZYexIgSG8rq"
        })
    }

    /// The same track after the February 2026 changes (popularity etc. removed).
    fn new_track() -> Value {
        let mut t = old_track();
        let o = t.as_object_mut().unwrap();
        for k in ["available_markets", "external_ids", "popularity", "track", "episode"] {
            o.remove(k);
        }
        t
    }

    fn old_playlist_item() -> Value {
        json!({
            "added_at": "2023-05-12T18:22:31Z",
            "added_by": {"id": "someuser", "type": "user", "uri": "spotify:user:someuser"},
            "is_local": false,
            "primary_color": null,
            "track": old_track(),
            "video_thumbnail": {"url": null}
        })
    }

    fn new_playlist_item() -> Value {
        json!({
            "added_at": "2023-05-12T18:22:31.000Z",
            "added_by": {"id": "someuser", "type": "user", "uri": "spotify:user:someuser"},
            "is_local": false,
            "item": new_track()
        })
    }

    fn local_item() -> Value {
        json!({
            "added_at": "2021-01-01T00:00:00Z",
            "is_local": true,
            "track": {
                "album": {"name": "", "images": []},
                "artists": [{"name": "Some Artist", "type": "artist", "uri": "spotify:artist:"}],
                "duration_ms": 215000,
                "id": null,
                "is_local": true,
                "name": "Local Song",
                "track_number": 0,
                "type": "track",
                "uri": "spotify:local:Some+Artist::Local+Song:215"
            }
        })
    }

    fn episode_item() -> Value {
        json!({
            "added_at": "2022-03-04T05:06:07Z",
            "is_local": false,
            "item": {
                "description": "An episode",
                "duration_ms": 3600000,
                "id": "512ojhOuo1ktJprKbVcKyQ",
                "images": [{"height": 640, "url": "https://i.scdn.co/image/ep", "width": 640}],
                "name": "Episode 1",
                "release_date": "2022-03-01",
                "show": {"name": "Some Show"},
                "type": "episode",
                "uri": "spotify:episode:512ojhOuo1ktJprKbVcKyQ"
            }
        })
    }

    fn expected_track(added_at: i64) -> Track {
        Track {
            album_artist: String::new(),
            disc_no: None,
            id: "spotify:track:69kOkLUCkxIZYexIgSG8rq".into(),
            source: Source::Spotify,
            title: "Get Lucky (feat. Pharrell Williams and Nile Rodgers)".into(),
            artist: "Daft Punk, Pharrell Williams, Nile Rodgers".into(),
            album: "Random Access Memories".into(),
            duration_ms: 369626,
            track_no: Some(8),
            art: Some("https://i.scdn.co/image/ab67616d00001e029b9b36b0e22870b9f542d937".into()),
            uri: "spotify:track:69kOkLUCkxIZYexIgSG8rq".into(),
            added_at,
        }
    }

    // 2023-05-12T18:22:31Z
    const ADDED: i64 = 1_683_915_751;

    #[test]
    fn album_and_playlist_hits() {
        let album = json!({
            "id": "1x", "name": "Eversince", "release_date": "2016-05-04", "total_tracks": 13,
            "artists": [{"name": "Bladee"}],
            "images": [{"url": "https://i/640", "width": 640}, {"url": "https://i/300", "width": 300}]
        });
        let a = parse_album_hit(&album).unwrap();
        assert_eq!(a.key, "spotify:album:1x");
        assert_eq!((a.by.as_str(), a.year, a.songs), ("Bladee", Some(2016), Some(13)));
        assert_eq!(a.image.as_deref(), Some("https://i/300"));
        assert_eq!(a.subtitle(), "Bladee · 2016");
        let list = json!({
            "id": "37i", "name": "Drain Gang Essentials", "owner": {"display_name": "Spotify"},
            "tracks": {"total": 50}, "images": []
        });
        let p = parse_playlist_hit(&list).unwrap();
        assert_eq!(p.key, "spotify:playlist:37i");
        assert!(!p.album);
        assert_eq!(p.subtitle(), "by Spotify · 50 songs");
        // Spotify sends null for playlists it won't show.
        assert!(parse_playlist_hit(&Value::Null).is_none());
    }

    #[test]
    fn parses_old_playlist_item_shape() {
        assert_eq!(parse_playlist_item(&old_playlist_item()), Some(expected_track(ADDED)));
    }

    #[test]
    fn parses_new_playlist_item_shape() {
        assert_eq!(parse_playlist_item(&new_playlist_item()), Some(expected_track(ADDED)));
    }

    #[test]
    fn parse_track_direct() {
        assert_eq!(parse_track(&new_track(), None), Some(expected_track(0)));
        assert_eq!(
            parse_track(&old_track(), Some("2023-05-12T18:22:31Z")),
            Some(expected_track(ADDED))
        );
        assert_eq!(parse_track(&old_track(), Some("garbage")).unwrap().added_at, 0);
    }

    #[test]
    fn prefers_item_over_null_track() {
        // Transitional responses may carry both keys.
        let mut v = new_playlist_item();
        v["track"] = Value::Null;
        assert!(parse_playlist_item(&v).is_some());
        let v = json!({"added_at": "2023-05-12T18:22:31Z", "item": null, "track": old_track()});
        assert_eq!(parse_playlist_item(&v), Some(expected_track(ADDED)));
    }

    #[test]
    fn skips_null_local_and_episode_items() {
        assert_eq!(
            parse_playlist_item(&json!({"added_at": "2020-01-01T00:00:00Z", "track": null})),
            None
        );
        assert_eq!(
            parse_playlist_item(&json!({"added_at": "2020-01-01T00:00:00Z", "item": null})),
            None
        );
        assert_eq!(parse_playlist_item(&json!(null)), None);
        assert_eq!(parse_playlist_item(&local_item()), None);
        assert_eq!(parse_playlist_item(&episode_item()), None);
        // Local track object even without the wrapper flag.
        let mut inner = local_item()["track"].clone();
        inner["is_local"] = json!(false);
        assert_eq!(parse_track(&inner, None), None, "spotify:local URIs are not playable");
        assert_eq!(parse_track(&json!(null), None), None);
        assert_eq!(parse_track(&episode_item()["item"], None), None);
    }

    #[test]
    fn mixed_page_keeps_only_tracks() {
        let page = json!({
            "href": "https://api.spotify.com/v1/playlists/x/items?offset=0&limit=100",
            "items": [old_playlist_item(), local_item(), {"track": null}, episode_item(), new_playlist_item()],
            "limit": 100, "next": null, "offset": 0, "previous": null, "total": 5
        });
        let (items, next) = take_page(page);
        assert_eq!(next, None);
        let tracks: Vec<_> = items.iter().filter_map(parse_playlist_item).collect();
        assert_eq!(tracks.len(), 2);
    }

    #[test]
    fn missing_fields_are_tolerated() {
        let t = parse_track(
            &json!({"id": "abc123", "name": "Bare", "type": "track", "artists": [], "album": {"images": []}}),
            None,
        )
        .unwrap();
        assert_eq!(t.id, "spotify:track:abc123");
        assert_eq!(t.uri, t.id);
        assert_eq!(t.artist, "");
        assert_eq!(t.album, "");
        assert_eq!(t.art, None);
        assert_eq!(t.track_no, None);
        assert_eq!(t.duration_ms, 0);
    }

    #[test]
    fn image_selection() {
        let img = |w: Option<u64>| json!({"url": format!("u{}", w.map_or("?".to_owned(), |w| w.to_string())), "width": w, "height": w});
        let pick = |ws: &[Option<u64>]| pick_image(&Value::Array(ws.iter().map(|w| img(*w)).collect()));
        // Largest first (Spotify's order): smallest >= 250.
        assert_eq!(pick(&[Some(640), Some(300), Some(64)]).as_deref(), Some("u300"));
        // Order doesn't matter.
        assert_eq!(pick(&[Some(64), Some(640), Some(300)]).as_deref(), Some("u300"));
        assert_eq!(pick(&[Some(640), Some(250), Some(64)]).as_deref(), Some("u250"));
        // Nothing >= 250: the largest.
        assert_eq!(pick(&[Some(200), Some(64)]).as_deref(), Some("u200"));
        assert_eq!(pick(&[Some(64), Some(200)]).as_deref(), Some("u200"));
        // Only big ones.
        assert_eq!(pick(&[Some(1000), Some(640)]).as_deref(), Some("u640"));
        // Unknown widths (some playlist / user images): first.
        assert_eq!(pick(&[None, None]).as_deref(), Some("u?"));
        assert_eq!(pick(&[None, Some(300)]).as_deref(), Some("u300"));
        assert_eq!(pick(&[]), None);
        assert_eq!(pick_image(&Value::Null), None);
    }

    #[test]
    fn rfc3339_parser() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2023-05-12T18:22:31Z"), Some(ADDED));
        assert_eq!(parse_rfc3339("2023-05-12T18:22:31.123Z"), Some(ADDED));
        assert_eq!(parse_rfc3339("2023-05-12T18:22:31.123456789Z"), Some(ADDED));
        assert_eq!(parse_rfc3339("2023-05-12T18:22:31+00:00"), Some(ADDED));
        assert_eq!(parse_rfc3339("2023-05-12T18:22:31.5+00:00"), Some(ADDED));
        assert_eq!(parse_rfc3339("2023-05-12T20:22:31+02:00"), Some(ADDED));
        assert_eq!(parse_rfc3339("2023-05-12T13:52:31-04:30"), Some(ADDED));
        assert_eq!(parse_rfc3339("2023-05-12t18:22:31z"), Some(ADDED));
        assert_eq!(parse_rfc3339("2023-05-12 18:22:31Z"), Some(ADDED));
        assert_eq!(parse_rfc3339("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_rfc3339("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(parse_rfc3339("1969-12-31T23:59:59Z"), Some(-1));
        assert_eq!(parse_rfc3339("2038-01-19T03:14:08Z"), Some(1i64 << 31));
        for bad in [
            "",
            "garbage",
            "2023-05-12",
            "2023-13-01T00:00:00Z",
            "2023-02-29T00:00:00Z",
            "2023-05-12T24:00:00Z",
            "2023-05-12T18:22:31.Z",
            "2023-05-12T18:22:31+0x:00",
            "2023-05-12T18:22:31 junk",
            "２０２３-05-12T18:22:31Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn playlist_meta_old_and_new_shapes() {
        let old = json!({
            "collaborative": false,
            "description": "Chill beats &amp; vibes. More at <a href=\"spotify:playlist:37i9dQZF1DX4WYpdgoIcn6\">Chill Hits</a> &#x27;24",
            "external_urls": {"spotify": "https://open.spotify.com/playlist/3cEYpjA9oz9GiPac4AsH4n"},
            "href": "https://api.spotify.com/v1/playlists/3cEYpjA9oz9GiPac4AsH4n",
            "id": "3cEYpjA9oz9GiPac4AsH4n",
            "images": [{"height": null, "url": "https://mosaic.scdn.co/640/abc", "width": null}],
            "name": "Spotify Web API Testing playlist",
            "owner": {"display_name": "JMPerez²", "id": "jmperezperez", "type": "user", "uri": "spotify:user:jmperezperez"},
            "primary_color": null,
            "public": true,
            "snapshot_id": "MTgsZWFmNmZiNTIzYTg4ODM0OGQzZWQzOGI4NTdkNTJlMjU0OWFkYTUxMA==",
            "tracks": {"href": "https://api.spotify.com/v1/playlists/3cEYpjA9oz9GiPac4AsH4n/tracks", "total": 5},
            "type": "playlist",
            "uri": "spotify:playlist:3cEYpjA9oz9GiPac4AsH4n"
        });
        let m = parse_playlist_meta(&old).unwrap();
        assert_eq!(
            m,
            SpotifyPlaylistMeta {
                id: "3cEYpjA9oz9GiPac4AsH4n".into(),
                name: "Spotify Web API Testing playlist".into(),
                description: "Chill beats & vibes. More at Chill Hits '24".into(),
                art: Some("https://mosaic.scdn.co/640/abc".into()),
                total: 5,
                snapshot_id: "MTgsZWFmNmZiNTIzYTg4ODM0OGQzZWQzOGI4NTdkNTJlMjU0OWFkYTUxMA==".into(),
                owner: "JMPerez²".into(),
            }
        );

        let mut new = old.clone();
        let o = new.as_object_mut().unwrap();
        o.remove("tracks");
        o.insert(
            "items".into(),
            json!({"href": "https://api.spotify.com/v1/playlists/3cEYpjA9oz9GiPac4AsH4n/items", "total": 42}),
        );
        let m = parse_playlist_meta(&new).unwrap();
        assert_eq!(m.total, 42);
        assert_eq!(m.id, "3cEYpjA9oz9GiPac4AsH4n");

        // No images (null), no total, owner without display name.
        let bare = json!({"id": "p1", "name": "Empty", "images": null, "owner": {"id": "me", "display_name": null}});
        let m = parse_playlist_meta(&bare).unwrap();
        assert_eq!(
            (m.total, m.art, m.owner.as_str(), m.description.as_str()),
            (0, None, "me", "")
        );

        assert_eq!(parse_playlist_meta(&json!(null)), None);
        assert_eq!(parse_playlist_meta(&json!({"name": "no id"})), None);
    }

    #[test]
    fn user_parsing() {
        let v = json!({
            "display_name": "Sumo User",
            "external_urls": {"spotify": "https://open.spotify.com/user/multimusic"},
            "id": "multimusic",
            "images": [
                {"url": "https://i.scdn.co/image/small", "height": 64, "width": 64},
                {"url": "https://i.scdn.co/image/big", "height": 300, "width": 300}
            ],
            "type": "user",
            "uri": "spotify:user:multimusic"
        });
        assert_eq!(
            parse_user(&v),
            Some(SpotifyUser {
                id: "multimusic".into(),
                display_name: "Sumo User".into(),
                image: Some("https://i.scdn.co/image/big".into()),
            })
        );
        let v = json!({"id": "multimusic", "display_name": null, "images": []});
        assert_eq!(parse_user(&v).unwrap().display_name, "multimusic");
    }

    #[test]
    fn id_normalization() {
        assert_eq!(
            normalize_playlist_id("37i9dQZF1DXcBWIGoYBM5M"),
            "37i9dQZF1DXcBWIGoYBM5M"
        );
        assert_eq!(
            normalize_playlist_id("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"),
            "37i9dQZF1DXcBWIGoYBM5M"
        );
        assert_eq!(
            normalize_playlist_id("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc"),
            "37i9dQZF1DXcBWIGoYBM5M"
        );
        let want = Some((
            "spotify:track:69kOkLUCkxIZYexIgSG8rq".to_owned(),
            "69kOkLUCkxIZYexIgSG8rq".to_owned(),
        ));
        assert_eq!(normalize_track_uri("spotify:track:69kOkLUCkxIZYexIgSG8rq"), want);
        assert_eq!(normalize_track_uri("69kOkLUCkxIZYexIgSG8rq"), want);
        assert_eq!(
            normalize_track_uri("https://open.spotify.com/track/69kOkLUCkxIZYexIgSG8rq?si=x"),
            want
        );
        assert_eq!(normalize_track_uri("spotify:local:a:b:c:1"), None);
        assert_eq!(normalize_track_uri("spotify:episode:512ojhOuo1ktJprKbVcKyQ"), None);
        assert_eq!(normalize_track_uri(""), None);
    }

    #[test]
    fn errors_carry_status_and_message() {
        let e = ApiError::new(
            StatusCode::UNAUTHORIZED,
            &Method::GET,
            "/me",
            r#"{"error":{"status":401,"message":"The access token expired"}}"#,
        );
        assert_eq!(
            e.to_string(),
            "Spotify API GET /me failed with HTTP 401 Unauthorized: The access token expired"
        );
        let err = anyhow::Error::new(e)
            .context("fetching Spotify user profile")
            .context("outer");
        assert!(is_unauthorized(&err));
        assert_eq!(error_status(&err), Some(401));
        assert!(format!("{err:#}").contains("401"));

        let e: anyhow::Error = ApiError::new(StatusCode::FORBIDDEN, &Method::GET, "/x", "<html>  nope\n</html>").into();
        assert!(!is_unauthorized(&e));
        assert_eq!(error_status(&e), Some(403));
        assert!(e.to_string().ends_with(": <html> nope </html>"));
        assert!(!is_unauthorized(&anyhow!("network down")));
        assert!(is_unauthorized(&anyhow!("upstream said HTTP 401")));

        let long = "x".repeat(1000);
        assert_eq!(error_message(&long).chars().count(), ERROR_SNIPPET_CHARS + 1);
        assert_eq!(
            error_message(r#"{"error":"invalid_grant","error_description":"Refresh token revoked"}"#),
            "Refresh token revoked"
        );
    }

    #[test]
    fn retry_after_header() {
        let mut h = header::HeaderMap::new();
        assert_eq!(retry_after_secs(&h), DEFAULT_RETRY_AFTER_SECS);
        h.insert(header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(retry_after_secs(&h), 7);
        h.insert(header::RETRY_AFTER, "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap());
        assert_eq!(retry_after_secs(&h), DEFAULT_RETRY_AFTER_SECS);
    }

    #[test]
    fn description_cleanup() {
        assert_eq!(
            clean_description("a &amp;amp; b &lt;3 &unknown; & c&#33;"),
            "a &amp; b <3 &unknown; & c!"
        );
        assert_eq!(clean_description("  <b>bold</b>&nbsp;text "), "bold text");
    }

    #[test]
    fn next_links_stay_on_the_api() {
        let api = SpotifyApi::new(reqwest::Client::new());
        let n = "https://api.spotify.com/v1/me/tracks?offset=50&limit=50";
        assert_eq!(api.rebase_next(n).as_deref(), Some(n));
        assert_eq!(api.rebase_next("https://evil.example/steal"), None);
        let api = SpotifyApi::with_base(reqwest::Client::new(), "http://127.0.0.1:9/v1");
        assert_eq!(
            api.rebase_next(n).as_deref(),
            Some("http://127.0.0.1:9/v1/me/tracks?offset=50&limit=50")
        );
        assert_eq!(with_param("http://x/a", "limit", 5), "http://x/a?limit=5");
        assert_eq!(with_param("http://x/a?b=1", "limit", 5), "http://x/a?b=1&limit=5");
    }

    /// Tiny loopback HTTP/1.1 server so the request / retry / fallback logic can be tested
    /// without touching the network.
    mod mock {
        use std::sync::{Arc, Mutex};

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        pub struct Reply {
            pub status: u16,
            pub body: String,
            pub headers: Vec<(&'static str, String)>,
        }

        pub fn reply(status: u16, body: impl Into<String>) -> Reply {
            Reply {
                status,
                body: body.into(),
                headers: Vec::new(),
            }
        }

        pub type Log = Arc<Mutex<Vec<String>>>;

        /// Serves `handler(method, path_and_query)` (path relative to `/v1`) and records every
        /// request as `"METHOD /path?query"`. Requests without the bearer token get a 401.
        pub async fn serve<F>(handler: F) -> (String, Log)
        where
            F: Fn(&str, &str) -> Reply + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/v1", listener.local_addr().unwrap());
            let log: Log = Arc::default();
            let handler = Arc::new(handler);
            let server_log = log.clone();
            tokio::spawn(async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    let handler = handler.clone();
                    let log = server_log.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 4096];
                        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            match sock.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let head = String::from_utf8_lossy(&buf).to_string();
                        let mut first = head.lines().next().unwrap_or("").split_whitespace();
                        let method = first.next().unwrap_or("").to_owned();
                        let target = first.next().unwrap_or("").to_owned();
                        let path = target.strip_prefix("/v1").unwrap_or(&target).to_owned();
                        log.lock().unwrap().push(format!("{method} {path}"));
                        let authed = head
                            .lines()
                            .any(|l| l.eq_ignore_ascii_case("authorization: Bearer tok"));
                        let r = if authed {
                            handler(&method, &path)
                        } else {
                            reply(401, r#"{"error":{"status":401,"message":"No token provided"}}"#)
                        };
                        let mut resp = format!(
                            "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                            r.status,
                            r.body.len()
                        );
                        for (k, v) in &r.headers {
                            resp.push_str(&format!("{k}: {v}\r\n"));
                        }
                        resp.push_str("\r\n");
                        resp.push_str(&r.body);
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    });
                }
            });
            (base, log)
        }
    }

    use mock::{reply, serve, Reply};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn log_of(log: &mock::Log) -> Vec<String> {
        log.lock().unwrap().clone()
    }

    fn page(items: Vec<Value>, next: Option<&str>) -> String {
        json!({"items": items, "next": next, "total": 0}).to_string()
    }

    #[tokio::test]
    async fn playlist_tracks_uses_new_items_endpoint() {
        let (base, log) = serve(|_, path| match path {
            "/playlists/pl1/items?limit=100" => reply(
                200,
                page(
                    vec![new_playlist_item(), episode_item()],
                    Some("https://api.spotify.com/v1/playlists/pl1/items?offset=100&limit=100"),
                ),
            ),
            "/playlists/pl1/items?offset=100&limit=100" => reply(200, page(vec![old_playlist_item()], None)),
            _ => reply(404, r#"{"error":{"status":404,"message":"Not found."}}"#),
        })
        .await;
        let api = SpotifyApi::with_base(client(), &base);
        let tracks = api.playlist_tracks("tok", "spotify:playlist:pl1").await.unwrap();
        assert_eq!(tracks, vec![expected_track(ADDED), expected_track(ADDED)]);
        assert_eq!(
            log_of(&log),
            [
                "GET /playlists/pl1/items?limit=100",
                "GET /playlists/pl1/items?offset=100&limit=100"
            ]
        );
    }

    #[tokio::test]
    async fn playlist_tracks_falls_back_to_old_endpoint_and_smaller_pages() {
        let (base, log) = serve(|_, path| match path {
            p if p.starts_with("/playlists/pl2/items") => {
                reply(404, r#"{"error":{"status":404,"message":"Not found."}}"#)
            }
            "/playlists/pl2/tracks?limit=100" => reply(400, r#"{"error":{"status":400,"message":"Invalid limit"}}"#),
            "/playlists/pl2/tracks?limit=50" => reply(
                200,
                page(
                    vec![old_playlist_item(), local_item()],
                    Some("https://api.spotify.com/v1/playlists/pl2/tracks?offset=50&limit=50"),
                ),
            ),
            "/playlists/pl2/tracks?offset=50&limit=50" => {
                reply(200, page(vec![json!({"track": null}), old_playlist_item()], None))
            }
            _ => reply(500, "unexpected"),
        })
        .await;
        let api = SpotifyApi::with_base(client(), &base);
        let tracks = api.playlist_tracks("tok", "pl2").await.unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(
            log_of(&log),
            [
                "GET /playlists/pl2/items?limit=100",
                "GET /playlists/pl2/tracks?limit=100",
                "GET /playlists/pl2/tracks?limit=50",
                "GET /playlists/pl2/tracks?offset=50&limit=50",
            ]
        );
    }

    #[tokio::test]
    async fn playlist_tracks_reports_the_meaningful_error() {
        let (base, _log) = serve(|_, path| {
            if path.contains("/items") {
                reply(403, r#"{"error":{"status":403,"message":"Forbidden"}}"#)
            } else {
                reply(404, r#"{"error":{"status":404,"message":"Not found."}}"#)
            }
        })
        .await;
        let api = SpotifyApi::with_base(client(), &base);
        let err = api.playlist_tracks("tok", "pl3").await.unwrap_err();
        assert_eq!(error_status(&err), Some(403));
        let msg = format!("{err:#}");
        assert!(msg.contains("HTTP 403") && msg.contains("HTTP 404"), "{msg}");
    }

    #[tokio::test]
    async fn unauthorized_is_detectable() {
        let (base, log) = serve(|_, _| reply(200, "{}")).await;
        let api = SpotifyApi::with_base(client(), &base);
        // The mock rejects any token other than "tok".
        let err = api.playlist_tracks("expired", "pl").await.unwrap_err();
        assert!(is_unauthorized(&err), "{err:#}");
        assert!(format!("{err:#}").contains("401"));
        // 401 must not trigger the /tracks fallback.
        assert_eq!(log_of(&log), ["GET /playlists/pl/items?limit=100"]);
        let err = api.me("expired").await.unwrap_err();
        assert!(is_unauthorized(&err));
        assert!(err.to_string().contains("user profile"));
    }

    #[tokio::test]
    async fn retries_rate_limits_and_server_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let (base, log) = serve(move |_, _| match c.fetch_add(1, Ordering::SeqCst) {
            0 => Reply {
                headers: vec![("Retry-After", "0".into())],
                ..reply(429, "")
            },
            1 => reply(503, "upstream unavailable"),
            _ => reply(200, r#"{"id":"u1","display_name":"U","images":[]}"#),
        })
        .await;
        let api = SpotifyApi::with_base(client(), &base);
        let me = api.me("tok").await.unwrap();
        assert_eq!(
            me,
            SpotifyUser {
                id: "u1".into(),
                display_name: "U".into(),
                image: None
            }
        );
        assert_eq!(log_of(&log).len(), 3);
    }

    #[tokio::test]
    async fn search_fails_fast_when_rate_limited() {
        // Background requests would wait 30 s here; search must not keep the user waiting.
        let (base, log) = serve(|_, _| Reply {
            headers: vec![("Retry-After", "30".into())],
            ..reply(429, r#"{"error":{"status":429,"message":"API rate limit exceeded"}}"#)
        })
        .await;
        let api = SpotifyApi::with_base(client(), &base);
        let started = std::time::Instant::now();
        let err = api.search_with_artists("tok", "bladee", 20).await.unwrap_err();
        assert_eq!(error_status(&err), Some(429));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert_eq!(log_of(&log).len(), 1);

        // A short Retry-After is waited out once.
        let (base, log) = serve(|_, _| Reply {
            headers: vec![("Retry-After", "0".into())],
            ..reply(429, "")
        })
        .await;
        let api = SpotifyApi::with_base(client(), &base);
        assert!(api.search_with_artists("tok", "bladee", 20).await.is_err());
        assert_eq!(log_of(&log).len(), 2);
    }

    #[tokio::test]
    async fn rate_limit_gives_up_eventually() {
        let (base, log) = serve(|_, _| Reply {
            headers: vec![("Retry-After", "0".into())],
            ..reply(429, r#"{"error":{"status":429,"message":"API rate limit exceeded"}}"#)
        })
        .await;
        let api = SpotifyApi::with_base(client(), &base);
        let err = api.me("tok").await.unwrap_err();
        assert_eq!(error_status(&err), Some(429));
        assert_eq!(log_of(&log).len(), 1 + MAX_RATE_LIMIT_RETRIES as usize);

        // A very long lockout fails immediately.
        let (base, log) = serve(|_, _| Reply {
            headers: vec![("Retry-After", "86400".into())],
            ..reply(429, "")
        })
        .await;
        let api = SpotifyApi::with_base(client(), &base);
        let err = api.me("tok").await.unwrap_err();
        assert!(format!("{err:#}").contains("retry after 86400s"), "{err:#}");
        assert_eq!(log_of(&log).len(), 1);
    }

    #[tokio::test]
    async fn set_liked_tries_library_then_legacy_endpoint() {
        let (base, log) = serve(|method, path| match (method, path) {
            (_, p) if p.starts_with("/me/library") && method == "PUT" => reply(404, ""),
            ("DELETE", "/me/library?uris=spotify%3Atrack%3A69kOkLUCkxIZYexIgSG8rq") => reply(200, ""),
            ("PUT", "/me/tracks?ids=69kOkLUCkxIZYexIgSG8rq") => reply(200, ""),
            _ => reply(500, "unexpected"),
        })
        .await;
        let api = SpotifyApi::with_base(client(), &base);
        api.set_liked("tok", "spotify:track:69kOkLUCkxIZYexIgSG8rq", true)
            .await
            .unwrap();
        api.set_liked("tok", "spotify:track:69kOkLUCkxIZYexIgSG8rq", false)
            .await
            .unwrap();
        assert!(api.set_liked("tok", "spotify:local:x:y:z:1", true).await.is_err());
        assert_eq!(
            log_of(&log),
            [
                "PUT /me/library?uris=spotify%3Atrack%3A69kOkLUCkxIZYexIgSG8rq",
                "PUT /me/tracks?ids=69kOkLUCkxIZYexIgSG8rq",
                "DELETE /me/library?uris=spotify%3Atrack%3A69kOkLUCkxIZYexIgSG8rq",
            ]
        );
    }

    #[tokio::test]
    async fn search_falls_back_to_dev_mode_limit_and_pages() {
        let track_n = |n: usize| {
            let mut t = new_track();
            t["id"] = json!(format!("id{n}"));
            t["uri"] = json!(format!("spotify:track:id{n}"));
            t
        };
        let (base, log) = serve(move |_, path| {
            let Some(rest) = path.strip_prefix("/search?type=track&q=daft%20punk") else {
                return reply(500, "unexpected");
            };
            match rest {
                "&limit=25" => reply(400, r#"{"error":{"status":400,"message":"Invalid limit"}}"#),
                "&limit=10" | "&offset=10&limit=10" | "&offset=20&limit=10" => {
                    let offset: usize = rest.strip_prefix("&offset=").map_or(0, |r| r[..2].parse().unwrap());
                    let items: Vec<Value> = (offset..offset + 10).map(track_n).collect();
                    let next = format!(
                        "https://api.spotify.com/v1/search?type=track&q=daft%20punk&offset={}&limit=10",
                        offset + 10
                    );
                    reply(
                        200,
                        json!({"tracks": {"items": items, "next": next, "total": 1000}}).to_string(),
                    )
                }
                _ => reply(500, "unexpected"),
            }
        })
        .await;
        let api = SpotifyApi::with_base(client(), &base);
        let tracks = api.search("tok", "  daft punk ", 25).await.unwrap();
        assert_eq!(tracks.len(), 25);
        assert_eq!(tracks[0].id, "spotify:track:id0");
        assert_eq!(tracks[24].id, "spotify:track:id24");
        assert_eq!(log_of(&log).len(), 4);
        assert!(api.search("tok", "   ", 10).await.unwrap().is_empty());
        assert_eq!(log_of(&log).len(), 4);
    }

    #[tokio::test]
    async fn liked_tracks_and_playlists_paginate() {
        let (base, _log) = serve(|_, path| match path {
            "/me/tracks?limit=50" => {
                let mut older = old_playlist_item();
                older["added_at"] = json!("2020-01-01T00:00:00Z");
                reply(
                    200,
                    page(
                        vec![new_playlist_item(), older],
                        Some("https://api.spotify.com/v1/me/tracks?offset=50&limit=50"),
                    ),
                )
            }
            "/me/tracks?offset=50&limit=50" => reply(200, page(vec![local_item()], None)),
            "/me/playlists?limit=50" => reply(
                200,
                page(
                    vec![
                        json!({"id": "a", "name": "A", "items": {"total": 3}, "owner": {"display_name": "Me"}, "snapshot_id": "s1"}),
                        Value::Null,
                    ],
                    Some("https://api.spotify.com/v1/me/playlists?offset=50&limit=50"),
                ),
            ),
            "/me/playlists?offset=50&limit=50" => reply(
                200,
                page(vec![json!({"id": "b", "name": "B", "tracks": {"total": 7}, "owner": {"display_name": "Other"}})], None),
            ),
            _ => reply(500, "unexpected"),
        })
        .await;
        let api = SpotifyApi::with_base(client(), &base);
        let liked = api.liked_tracks("tok").await.unwrap();
        assert_eq!(liked.len(), 2);
        assert_eq!(liked[0].added_at, ADDED);
        assert_eq!(liked[1].added_at, parse_rfc3339("2020-01-01T00:00:00Z").unwrap());

        let lists = api.playlists("tok").await.unwrap();
        let summary: Vec<_> = lists
            .iter()
            .map(|p| (p.id.as_str(), p.total, p.owner.as_str()))
            .collect();
        assert_eq!(summary, [("a", 3, "Me"), ("b", 7, "Other")]);
    }

    #[test]
    fn artist_hits() {
        let v = json!({
            "id": "4tZwfgrHOc3mvqYlEYSvVi",
            "name": "Daft Punk",
            "genres": ["filter house", "electro"],
            "images": [{"url": "https://i.scdn.co/image/big", "width": 640, "height": 640},
                       {"url": "https://i.scdn.co/image/mid", "width": 320, "height": 320}],
            "followers": {"total": 9_800_000}
        });
        let hit = parse_artist_hit(&v).unwrap();
        assert_eq!(hit.key, "spotify:artist:4tZwfgrHOc3mvqYlEYSvVi");
        assert_eq!(hit.image.as_deref(), Some("https://i.scdn.co/image/mid"));
        assert_eq!(hit.subtitle, "9.8M followers");
        // Dev-mode responses no longer include followers.
        let v = json!({"id": "x1", "name": "Somebody", "genres": ["indie pop"], "images": []});
        assert_eq!(parse_artist_hit(&v).unwrap().subtitle, "Indie pop");
        assert!(parse_artist_hit(&json!({"id": "", "name": "x"})).is_none());
    }
}
