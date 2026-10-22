//! SoundCloud provider built on the (undocumented) `api-v2.soundcloud.com` API that the
//! soundcloud.com web app uses.
//!
//! * The API needs a `client_id`. Unless the user configured one, it is scraped from the JS
//!   bundles referenced by the soundcloud.com homepage and cached in memory. When the API starts
//!   rejecting it (401/403) the cache is dropped and the id is scraped again once.
//! * An optional `oauth_token` (the cookie of a logged-in browser session) is sent as
//!   `Authorization: OAuth <token>` and unlocks private likes/playlists and Go+ streams.
//! * `stream_url` turns a track into a direct media URL (progressive MP3 or an HLS playlist)
//!   that mpv can play.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use reqwest::header::{HeaderMap, ACCEPT, AUTHORIZATION, RETRY_AFTER, USER_AGENT};
use reqwest::{StatusCode, Url};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::library::tags::Metadata;
use crate::model::{ArtistHit, CollectionHit, ImportedPlaylist, Source, Track};

const API_BASE: &str = "https://api-v2.soundcloud.com";
const WEB_BASE: &str = "https://soundcloud.com/";
/// soundcloud.com serves its regular page (with the asset bundles) to browsers; use a browser
/// user agent for the scraping requests only.
const BROWSER_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// Maximum number of liked tracks imported.
const MAX_LIKES: usize = 5000;
/// Maximum number of own / liked playlists imported (each).
const MAX_PLAYLISTS: usize = 1000;
/// Track ids per `GET /tracks?ids=` request.
const TRACK_BATCH: usize = 50;
/// Safety net against pagination loops.
const MAX_PAGES: usize = 200;
/// How often a 429 response is retried.
const MAX_429_RETRIES: u32 = 3;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(2);
/// A client_id scraped less than this long ago is considered fresh: a 401/403 then means the
/// resource itself is forbidden, so re-scraping would not help.
const MIN_RESCRAPE_INTERVAL: Duration = Duration::from_secs(30);

/// A SoundCloud account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScUser {
    pub id: u64,
    pub username: String,
    /// Avatar URL (500x500), `None` for the default avatar.
    pub avatar: Option<String>,
    pub permalink_url: String,
    pub followers: Option<u64>,
}

/// What a soundcloud.com URL points at.
#[derive(Debug, Clone)]
pub enum ScResolved {
    User(ScUser),
    Track(Track),
    /// A playlist or an album (`album`), with whoever made it.
    Playlist {
        playlist: ImportedPlaylist,
        album: bool,
        by: String,
    },
}

/// SoundCloud api-v2 client. Cheap to share behind an `Arc`; all methods take `&self`.
pub struct SoundCloud {
    http: reqwest::Client,
    /// Scraped client_id and when it was scraped.
    client_id: Mutex<Option<(String, Instant)>>,
    client_id_override: Option<String>,
    oauth_token: Option<String>,
    /// Set once SoundCloud rejected the OAuth token; anonymous requests then stop sending it.
    token_rejected: AtomicBool,
}

impl SoundCloud {
    /// Empty strings mean "not set".
    pub fn new(http: reqwest::Client, client_id_override: &str, oauth_token: &str) -> Self {
        let client_id_override = Some(client_id_override.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        // Accept the token with or without the "OAuth " prefix the web app uses.
        let token = oauth_token.trim();
        let token = token.strip_prefix("OAuth ").unwrap_or(token).trim();
        let oauth_token = Some(token).filter(|s| !s.is_empty()).map(str::to_owned);
        Self {
            http,
            client_id: Mutex::new(None),
            client_id_override,
            oauth_token,
            token_rejected: AtomicBool::new(false),
        }
    }

    /// Returns API client_id: override if set, else scraped from soundcloud.com and cached in memory.
    pub async fn client_id(&self) -> Result<String> {
        if let Some(id) = &self.client_id_override {
            return Ok(id.clone());
        }
        // Holding the lock while scraping makes concurrent callers wait for a single scrape.
        let mut cached = self.client_id.lock().await;
        if let Some((id, _)) = cached.as_ref() {
            return Ok(id.clone());
        }
        let id = self.scrape_client_id().await?;
        *cached = Some((id.clone(), Instant::now()));
        Ok(id)
    }

    /// Resolves a profile URL (or bare username) to the user.
    pub async fn resolve_user(&self, profile_url: &str) -> Result<ScUser> {
        let url = normalize_profile_url(profile_url)?;
        let v = self
            .get_json(&api("/resolve"), &[("url", url.as_str())])
            .await
            .with_context(|| format!("couldn't resolve SoundCloud profile {url}"))?;
        match v.get("kind").and_then(Value::as_str) {
            Some("user") | None => {}
            Some(kind) => bail!("{url} is a SoundCloud {kind}, not a profile"),
        }
        parse_user(&v).with_context(|| format!("unexpected response resolving {url}"))
    }

    /// The account the OAuth token belongs to.
    pub async fn me(&self) -> Result<ScUser> {
        if self.oauth_token.is_none() {
            bail!("no SoundCloud OAuth token configured");
        }
        let v = self
            .request_json(&api("/me"), &[], true)
            .await
            .context("couldn't fetch the SoundCloud account for the OAuth token")?;
        parse_user(&v).context("unexpected response from SoundCloud /me")
    }

    /// All liked tracks (up to 5000), newest first. `added_at` is the time of the like.
    pub async fn likes(&self, user_id: u64) -> Result<Vec<Track>> {
        let items = self
            .paginate(
                &api(&format!("/users/{user_id}/track_likes")),
                &[("limit", "200"), ("linked_partitioning", "1")],
                MAX_LIKES,
            )
            .await
            .context("couldn't fetch SoundCloud likes")?;

        // Likes normally carry full track objects; fetch any compact ones in bulk.
        let mut full: HashMap<u64, Track> = HashMap::new();
        let mut order: Vec<(u64, i64)> = Vec::with_capacity(items.len());
        let mut missing = Vec::new();
        for item in &items {
            let Some(t) = item.get("track") else { continue };
            let Some(id) = json_id(t) else { continue };
            let liked_at = item
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(parse_timestamp)
                .unwrap_or(0);
            match parse_track(t) {
                Some(track) => {
                    full.insert(id, track);
                }
                None => missing.push(id),
            }
            order.push((id, liked_at));
        }
        if !missing.is_empty() {
            self.fetch_tracks(&missing, &[], &mut full).await?;
        }

        let mut seen = HashSet::new();
        let tracks: Vec<Track> = order
            .into_iter()
            .filter(|(id, _)| seen.insert(*id))
            .filter_map(|(id, liked_at)| {
                let mut t = full.get(&id)?.clone();
                t.added_at = liked_at;
                Some(t)
            })
            .collect();
        info!("SoundCloud: {} liked tracks for user {user_id}", tracks.len());
        Ok(tracks)
    }

    /// The user's own playlists followed by the playlists they liked, with full track lists.
    /// A playlist that can't be loaded (e.g. a liked playlist that went private) is skipped.
    pub async fn playlists(&self, user_id: u64) -> Result<Vec<ImportedPlaylist>> {
        let params = [("limit", "50"), ("linked_partitioning", "1")];
        let own = match self
            .paginate(
                &api(&format!("/users/{user_id}/playlists_without_albums")),
                &params,
                MAX_PLAYLISTS,
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!("SoundCloud playlists_without_albums failed ({e:#}); falling back to /playlists");
                self.paginate(&api(&format!("/users/{user_id}/playlists")), &params, MAX_PLAYLISTS)
                    .await
                    .context("couldn't fetch SoundCloud playlists")?
            }
        };
        let liked: Vec<Value> = match self
            .paginate(
                &api(&format!("/users/{user_id}/playlist_likes")),
                &params,
                MAX_PLAYLISTS,
            )
            .await
        {
            Ok(items) => items
                .into_iter()
                .filter_map(|mut item| item.get_mut("playlist").map(Value::take))
                .collect(),
            Err(e) => {
                warn!("SoundCloud: couldn't fetch liked playlists: {e:#}");
                Vec::new()
            }
        };

        let mut seen = HashSet::new();
        let mut cache: HashMap<u64, Track> = HashMap::new();
        let mut out = Vec::new();
        for pl in own.into_iter().chain(liked) {
            let Some(id) = json_id(&pl) else { continue };
            if !seen.insert(id) {
                continue;
            }
            match self.load_playlist(pl, &mut cache).await {
                Ok(p) => out.push(p),
                Err(e) => warn!("SoundCloud: skipping playlist {id}: {e:#}"),
            }
        }
        info!("SoundCloud: {} playlists for user {user_id}", out.len());
        Ok(out)
    }

    /// Track search. One request (no paging) so results show up fast.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let page_size = limit.min(200).to_string();
        let v = self
            .get_json(&api("/search/tracks"), &[("q", query), ("limit", page_size.as_str())])
            .await
            .with_context(|| format!("SoundCloud search for \"{query}\" failed"))?;
        Ok(v.get("collection")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_track).take(limit).collect())
            .unwrap_or_default())
    }

    /// Artists / users matching `query`.
    pub async fn search_users(&self, query: &str, limit: usize) -> Result<Vec<ArtistHit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let limit_s = limit.min(50).to_string();
        let v = self
            .get_json(&api("/search/users"), &[("q", query), ("limit", limit_s.as_str())])
            .await
            .with_context(|| format!("SoundCloud artist search for \"{query}\" failed"))?;
        Ok(v.get("collection")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_user).map(|u| artist_hit(&u)).collect())
            .unwrap_or_default())
    }

    /// Albums (`albums`) or playlists matching `query`.
    pub async fn search_sets(&self, query: &str, albums: bool, limit: usize) -> Result<Vec<CollectionHit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let path = if albums {
            "/search/albums"
        } else {
            "/search/playlists_without_albums"
        };
        let limit_s = limit.min(50).to_string();
        let v = self
            .get_json(&api(path), &[("q", query), ("limit", limit_s.as_str())])
            .await
            .with_context(|| format!("SoundCloud playlist search for \"{query}\" failed"))?;
        Ok(v.get("collection")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_set_hit).take(limit).collect())
            .unwrap_or_default())
    }

    /// One user by numeric id.
    pub async fn user(&self, id: u64) -> Result<ScUser> {
        let v = self
            .get_json(&api(&format!("/users/{id}")), &[])
            .await
            .with_context(|| format!("couldn't load SoundCloud user {id}"))?;
        parse_user(&v).context("unexpected SoundCloud user response")
    }

    /// A user's popular tracks first, then the rest of their uploads (newest first).
    pub async fn user_tracks(&self, id: u64) -> Result<Vec<Track>> {
        let top_url = api(&format!("/users/{id}/toptracks"));
        let uploads_url = api(&format!("/users/{id}/tracks"));
        let (top, uploads) = tokio::join!(
            self.get_json(&top_url, &[("limit", "20")]),
            self.paginate(&uploads_url, &[("limit", "100"), ("linked_partitioning", "1")], 300),
        );
        let top: Vec<Track> = top
            .ok()
            .and_then(|v| v.get("collection").and_then(Value::as_array).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(parse_track)
            .collect();
        let uploads: Vec<Track> = uploads
            .with_context(|| format!("couldn't load tracks of SoundCloud user {id}"))?
            .iter()
            .filter_map(parse_track)
            .collect();
        let mut seen: HashSet<String> = HashSet::new();
        Ok(top
            .into_iter()
            .chain(uploads)
            .filter(|t| seen.insert(t.id.clone()))
            .collect())
    }

    /// Resolves any soundcloud.com URL (profile, track or playlist/album).
    pub async fn resolve_url(&self, url: &str) -> Result<ScResolved> {
        let v = self
            .get_json(&api("/resolve"), &[("url", url)])
            .await
            .with_context(|| format!("couldn't open {url} on SoundCloud"))?;
        match v.get("kind").and_then(Value::as_str) {
            Some("user") => Ok(ScResolved::User(parse_user(&v).context("unexpected user response")?)),
            Some("track") => Ok(ScResolved::Track(parse_track(&v).context("unexpected track response")?)),
            Some("playlist") | Some("system-playlist") => {
                let album = is_album(&v);
                let by = v
                    .pointer("/user/username")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let mut cache = HashMap::new();
                Ok(ScResolved::Playlist {
                    playlist: self.load_playlist(v, &mut cache).await?,
                    album,
                    by,
                })
            }
            other => bail!("{url} isn't a SoundCloud profile, track or playlist ({other:?})"),
        }
    }

    /// Seeds the client_id cache (e.g. from disk) so the first request doesn't need to scrape.
    /// A seeded id that gets rejected is re-scraped right away.
    pub async fn seed_client_id(&self, id: &str) {
        let id = id.trim();
        if self.client_id_override.is_some() || id.len() != 32 {
            return;
        }
        let mut cached = self.client_id.lock().await;
        if cached.is_none() {
            let old = Instant::now()
                .checked_sub(MIN_RESCRAPE_INTERVAL * 2)
                .unwrap_or_else(Instant::now);
            *cached = Some((id.to_string(), old));
        }
    }

    /// Direct stream URL for mpv. Accepts a Track whose id is "soundcloud:<numeric id>" (uri = permalink URL).
    pub async fn stream_url(&self, track: &Track) -> Result<String> {
        let json = self.track_json(track).await?;
        let title = json
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(track.title.as_str())
            .to_owned();

        let policy = json.get("policy").and_then(Value::as_str).unwrap_or("");
        if policy.eq_ignore_ascii_case("BLOCK") {
            bail!("\"{title}\" is not available in your region (blocked by SoundCloud)");
        }
        let transcodings: &[Value] = json
            .get("media")
            .and_then(|m| m.get("transcodings"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let candidates = ranked_transcodings(transcodings);
        if candidates.is_empty() {
            bail!(
                "\"{title}\" has no playable stream on SoundCloud \
                 (not available in your region / Go+ only)"
            );
        }
        if candidates[0].get("snipped").and_then(Value::as_bool) == Some(true) {
            warn!("SoundCloud: only a 30 second preview of \"{title}\" is available (Go+ track)");
        }
        let (url, _) = self
            .first_stream(&json, &candidates)
            .await
            .with_context(|| format!("couldn't get a stream URL for \"{title}\""))?;
        Ok(url)
    }

    /// The URL of the first of `candidates` (transcodings, best first) SoundCloud hands out.
    /// Some tracks still list streams that no longer exist (HTTP 404 for many "progressive"
    /// MP3s), so the next one is tried. Returns the URL and the transcoding's protocol.
    async fn first_stream(&self, json: &Value, candidates: &[&Value]) -> Result<(String, String)> {
        let mut params = Vec::new();
        if let Some(auth) = json.get("track_authorization").and_then(Value::as_str) {
            params.push(("track_authorization", auth));
        }
        let mut first_error = None;
        for chosen in candidates {
            let Some(endpoint) = chosen.get("url").and_then(Value::as_str) else {
                continue;
            };
            // (Computed outside the macros: tracing's macros shadow `Value`.)
            let protocol = chosen
                .pointer("/format/protocol")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let mime = chosen
                .pointer("/format/mime_type")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            match self.get_json(endpoint, &params).await {
                Ok(resp) => {
                    if let Some(url) = resp.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()) {
                        debug!("SoundCloud: streaming as {protocol} {mime}");
                        return Ok((url.to_owned(), protocol));
                    }
                    first_error.get_or_insert_with(|| anyhow!("SoundCloud returned no stream URL"));
                }
                Err(e) => {
                    debug!("SoundCloud: {protocol} {mime} stream unavailable ({e:#}); trying the next one");
                    first_error.get_or_insert(e);
                }
            }
        }
        Err(first_error.unwrap_or_else(|| anyhow!("no stream to try")))
    }

    // ---------------------------------------------------------------------------------------
    // Downloads
    // ---------------------------------------------------------------------------------------

    /// Saves `track` into `dir`: the uploader's original file when they allow downloads,
    /// otherwise the stream SoundCloud plays (encrypted Go+ streams and 30 second previews are
    /// refused). Returns the saved file and the song's details for tagging. `progress` is
    /// called with 0..=1 while downloading.
    pub async fn download(
        &self,
        track: &Track,
        dir: &Path,
        progress: &(dyn Fn(f32) + Send + Sync),
    ) -> Result<ScDownload> {
        let json = self.track_json(track).await?;
        let title = json
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(track.title.as_str())
            .to_owned();
        if json
            .get("policy")
            .and_then(Value::as_str)
            .is_some_and(|p| p.eq_ignore_ascii_case("BLOCK"))
        {
            bail!("\"{title}\" is not available in your region");
        }
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("couldn't create {}", dir.display()))?;
        let id = json_id(&json).unwrap_or_default();
        let part = dir.join(format!(".multimusic-{id}.part"));
        let stem = file_stem(&track.artist, &title);

        let downloadable = json.get("downloadable").and_then(Value::as_bool) == Some(true)
            && json.get("has_downloads_left").and_then(Value::as_bool) != Some(false);
        let mut kind = DownloadKind::Stream;
        let saved = if downloadable {
            match self.download_original(id, &part, progress).await {
                Ok(()) => {
                    kind = DownloadKind::Original;
                    Ok(())
                }
                Err(e) => {
                    warn!("SoundCloud: original file of \"{title}\" unavailable ({e:#}); saving the stream");
                    self.download_stream(&json, &title, &part, progress).await
                }
            }
        } else {
            self.download_stream(&json, &title, &part, progress).await
        };
        if let Err(e) = saved {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        // Only files that are audio in a known format are kept, under that format's name:
        // anything else (a playlist, a web page) could make the player fetch or open other
        // things when it is played.
        let mut ext = sniff_file_ext(&part).await;
        if ext.is_none() && kind == DownloadKind::Original {
            warn!("SoundCloud: original file of \"{title}\" isn't audio Sumo knows; saving the stream");
            let _ = tokio::fs::remove_file(&part).await;
            kind = DownloadKind::Stream;
            if let Err(e) = self.download_stream(&json, &title, &part, progress).await {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(e);
            }
            ext = sniff_file_ext(&part).await;
        }
        let Some(ext) = ext else {
            let _ = tokio::fs::remove_file(&part).await;
            bail!("\"{title}\" didn't download as an audio file");
        };
        let path = {
            let (part, folder, stem) = (part.clone(), dir.to_path_buf(), stem.clone());
            tokio::task::spawn_blocking(move || save_unique(&part, &folder, &stem, ext))
                .await?
                .with_context(|| format!("couldn't save \"{title}\" in {}", dir.display()))?
        };
        info!("SoundCloud: saved \"{title}\" to {}", path.display());
        Ok(ScDownload {
            path,
            kind,
            meta: download_metadata(&json, track),
        })
    }

    /// The file the uploader put up for download.
    async fn download_original(&self, id: u64, part: &Path, progress: &(dyn Fn(f32) + Send + Sync)) -> Result<()> {
        let v = self.get_json(&api(&format!("/tracks/{id}/download")), &[]).await?;
        let url = v
            .get("redirectUri")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .context("SoundCloud returned no download link")?;
        save_body(&self.http, url, part, progress).await
    }

    /// The regular stream: progressive MP3, or the segments of an HLS stream joined together.
    async fn download_stream(
        &self,
        json: &Value,
        title: &str,
        part: &Path,
        progress: &(dyn Fn(f32) + Send + Sync),
    ) -> Result<()> {
        let transcodings: &[Value] = json
            .get("media")
            .and_then(|m| m.get("transcodings"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let candidates = ranked_download_transcodings(transcodings);
        if candidates.is_empty() {
            bail!("\"{title}\" can't be downloaded: SoundCloud only offers it encrypted (Go+) or not at all");
        }
        let full: Vec<&Value> = candidates
            .into_iter()
            .filter(|t| t.get("snipped").and_then(Value::as_bool) != Some(true))
            .collect();
        if full.is_empty() {
            bail!("\"{title}\" can't be downloaded: only a 30 second preview is available (Go+ track)");
        }
        let (url, protocol) = self.first_stream(json, &full).await?;
        if protocol == "progressive" {
            return save_body(&self.http, &url, part, progress).await;
        }
        let playlist = self.fetch_text(&url).await?;
        let hls = parse_hls(&playlist, &url)?;
        let mut file = tokio::fs::File::create(part).await?;
        let total = hls.segments.len() + usize::from(hls.init.is_some());
        for (i, segment) in hls.init.iter().chain(hls.segments.iter()).enumerate() {
            let resp = self
                .http
                .get(segment)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .with_context(|| format!("downloading part {} of {total}", i + 1))?;
            // A part is a few seconds of audio.
            let bytes = crate::http::read_capped(resp, 64 << 20).await?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &bytes).await?;
            progress((i + 1) as f32 / total as f32);
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------------------------

    /// Full api-v2 JSON of a track: by numeric id, falling back to resolving the permalink.
    async fn track_json(&self, track: &Track) -> Result<Value> {
        let numeric = track
            .id
            .strip_prefix("soundcloud:")
            .and_then(|rest| rest.rsplit(':').next())
            .and_then(|s| s.parse::<u64>().ok());
        let permalink = Some(track.uri.as_str()).filter(|u| u.starts_with("http"));

        let by_id = match numeric {
            Some(id) => match self.get_json(&api(&format!("/tracks/{id}")), &[]).await {
                Ok(v) => return Ok(v),
                Err(e) => Some(e),
            },
            None => None,
        };
        let Some(url) = permalink else {
            return Err(
                by_id.unwrap_or_else(|| anyhow!("\"{}\" ({}) is not a SoundCloud track", track.title, track.id))
            );
        };
        if let Some(e) = &by_id {
            debug!(
                "SoundCloud: /tracks lookup for {} failed ({e:#}); resolving {url}",
                track.id
            );
        }
        match self.get_json(&api("/resolve"), &[("url", url)]).await {
            Ok(v) if v.get("kind").and_then(Value::as_str) == Some("track") => Ok(v),
            Ok(_) => Err(anyhow!("{url} is not a SoundCloud track")),
            Err(e) => Err(by_id.unwrap_or(e)),
        }
        .with_context(|| format!("couldn't load \"{}\" from SoundCloud", track.title))
    }

    /// Fills in a playlist's track list (fetching compact track stubs in bulk) and converts it.
    async fn load_playlist(&self, mut pl: Value, cache: &mut HashMap<u64, Track>) -> Result<ImportedPlaylist> {
        let id = json_id(&pl).context("playlist without id")?;
        let track_count = pl.get("track_count").and_then(Value::as_u64).unwrap_or(0);
        let has_tracks = pl
            .get("tracks")
            .and_then(Value::as_array)
            .is_some_and(|t| !t.is_empty());
        if !has_tracks && track_count > 0 {
            pl = self
                .get_json(&api(&format!("/playlists/{id}")), &[])
                .await
                .context("couldn't fetch playlist")?;
        }

        let entries = pl
            .get("tracks")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut order = Vec::with_capacity(entries.len());
        let mut missing = Vec::new();
        for entry in entries {
            let Some(tid) = json_id(entry) else { continue };
            if let Some(track) = parse_track(entry) {
                cache.insert(tid, track);
            } else if !cache.contains_key(&tid) {
                missing.push(tid);
            }
            order.push(tid);
        }
        if !missing.is_empty() {
            let pid = id.to_string();
            let mut extra = vec![("playlistId", pid.as_str())];
            if let Some(secret) = pl.get("secret_token").and_then(Value::as_str) {
                extra.push(("playlistSecretToken", secret));
            }
            self.fetch_tracks(&missing, &extra, cache).await?;
        }
        Ok(assemble_playlist(&pl, &order, cache))
    }

    /// Fetches full track objects for `ids` (in batches) into `into`. Unavailable tracks are
    /// silently absent from SoundCloud's answer.
    async fn fetch_tracks(&self, ids: &[u64], extra: &[(&str, &str)], into: &mut HashMap<u64, Track>) -> Result<()> {
        let mut unique: Vec<u64> = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        for chunk in unique.chunks(TRACK_BATCH) {
            let joined = chunk.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
            let mut params = vec![("ids", joined.as_str())];
            params.extend_from_slice(extra);
            let v = self
                .get_json(&api("/tracks"), &params)
                .await
                .context("couldn't fetch SoundCloud tracks")?;
            let list = v
                .as_array()
                .or_else(|| v.get("collection").and_then(Value::as_array))
                .map(Vec::as_slice)
                .unwrap_or_default();
            for t in list {
                if let (Some(id), Some(track)) = (json_id(t), parse_track(t)) {
                    into.insert(id, track);
                }
            }
        }
        Ok(())
    }

    /// Collects `collection` items of a linked-partitioning endpoint, following `next_href`.
    async fn paginate(&self, url: &str, params: &[(&str, &str)], cap: usize) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut page = self.get_json(url, params).await?;
        let mut current = url.to_owned();
        let mut empty_pages = 0;
        for _ in 0..MAX_PAGES {
            let next = page
                .get("next_href")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let items = match page.get_mut("collection").map(Value::take) {
                Some(Value::Array(items)) => items,
                _ => match page {
                    Value::Array(items) => items,
                    _ => Vec::new(),
                },
            };
            empty_pages = if items.is_empty() { empty_pages + 1 } else { 0 };
            out.extend(items);
            if out.len() >= cap {
                out.truncate(cap);
                return Ok(out);
            }
            match next {
                Some(next) if next != current && empty_pages < 2 => {
                    page = self.get_json(&next, &[]).await?;
                    current = next;
                }
                _ => return Ok(out),
            }
        }
        warn!(
            "SoundCloud: stopped paginating {} after {MAX_PAGES} pages",
            url_path(url)
        );
        Ok(out)
    }

    async fn get_json(&self, url: &str, params: &[(&str, &str)]) -> Result<Value> {
        self.request_json(url, params, false).await
    }

    /// GET an api-v2 URL with `client_id` (+ OAuth header when available), handling client_id
    /// expiry (401/403 → re-scrape once), rejected OAuth tokens and rate limiting.
    async fn request_json(&self, url: &str, params: &[(&str, &str)], require_auth: bool) -> Result<Value> {
        let what = url_path(url);
        let mut rescraped = false;
        loop {
            let cid = self.client_id().await?;
            let full = build_url(url, params, &cid)?;
            // Links come from SoundCloud's answers (next pages, streams): the login only goes
            // to SoundCloud itself.
            let token = self.token(require_auth).filter(|_| is_soundcloud_api(&full));
            let mut req = self.http.get(full).header(ACCEPT, "application/json");
            if let Some(token) = token {
                req = req.header(AUTHORIZATION, format!("OAuth {token}"));
            }
            let resp = self.send_throttled(req, &what).await?;
            let status = resp.status();
            if status.is_success() {
                return resp
                    .json::<Value>()
                    .await
                    .with_context(|| format!("invalid JSON from SoundCloud {what}"));
            }

            let denied = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
            if denied && !rescraped && self.client_id_override.is_none() {
                rescraped = true;
                if self.invalidate_client_id(&cid).await {
                    warn!("SoundCloud: HTTP {status} for {what}; refreshing client_id and retrying");
                    continue;
                }
            }
            if status == StatusCode::UNAUTHORIZED && token.is_some() && !require_auth {
                warn!(
                    "SoundCloud rejected the OAuth token (expired?); continuing without it. \
                     Update the oauth_token in the settings."
                );
                self.token_rejected.store(true, Ordering::Relaxed);
                continue;
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(status_error(status, &what, &body, token.is_some()));
        }
    }

    /// The OAuth token to send, if any.
    fn token(&self, require_auth: bool) -> Option<&str> {
        let token = self.oauth_token.as_deref()?;
        (require_auth || !self.token_rejected.load(Ordering::Relaxed)).then_some(token)
    }

    /// Sends a request, waiting out 429 responses (Retry-After, default 2s) up to 3 times.
    async fn send_throttled(&self, req: reqwest::RequestBuilder, what: &str) -> Result<reqwest::Response> {
        let mut attempts = 0;
        loop {
            let attempt = req.try_clone().context("request can't be retried")?;
            let resp = attempt
                .send()
                .await
                .with_context(|| format!("request to SoundCloud {what} failed"))?;
            if resp.status() == StatusCode::TOO_MANY_REQUESTS && attempts < MAX_429_RETRIES {
                attempts += 1;
                let wait = retry_after(resp.headers());
                warn!("SoundCloud rate limit on {what}; retrying in {wait:?} ({attempts}/{MAX_429_RETRIES})");
                tokio::time::sleep(wait).await;
                continue;
            }
            return Ok(resp);
        }
    }

    /// Drops the cached client_id after it was rejected. Returns whether retrying makes sense.
    async fn invalidate_client_id(&self, rejected: &str) -> bool {
        let mut cached = self.client_id.lock().await;
        match cached.as_ref() {
            Some((id, scraped)) if id == rejected => {
                if scraped.elapsed() < MIN_RESCRAPE_INTERVAL {
                    // Just scraped: the id is fine, access to the resource itself is denied.
                    return false;
                }
                *cached = None;
                true
            }
            // Another request already refreshed (or is refreshing) it.
            _ => true,
        }
    }

    async fn scrape_client_id(&self) -> Result<String> {
        info!("SoundCloud: scraping an API client_id from soundcloud.com");
        let html = self
            .fetch_text(WEB_BASE)
            .await
            .context("couldn't load soundcloud.com to find an API client_id")?;
        if let Some(id) = extract_client_id(&html) {
            return Ok(id);
        }
        let scripts = extract_script_urls(&html);
        if scripts.is_empty() {
            bail!("no script bundles found on soundcloud.com; set a client_id in the settings");
        }
        // The id usually lives in one of the last bundles.
        for url in scripts.iter().rev() {
            match self.fetch_text(url).await {
                Ok(js) => {
                    if let Some(id) = extract_client_id(&js) {
                        debug!("SoundCloud: found client_id in {url}");
                        return Ok(id);
                    }
                }
                Err(e) => debug!("SoundCloud: couldn't fetch {url}: {e:#}"),
            }
        }
        bail!(
            "couldn't find an API client_id in soundcloud.com's scripts; \
             set a client_id in the settings"
        )
    }

    async fn fetch_text(&self, url: &str) -> Result<String> {
        let req = self.http.get(url).header(USER_AGENT, BROWSER_UA);
        let resp = self.send_throttled(req, url).await?;
        let resp = resp.error_for_status()?;
        Ok(resp.text().await?)
    }
}

// -------------------------------------------------------------------------------------------
// JSON mapping
// -------------------------------------------------------------------------------------------

/// Maps an api-v2 track JSON object to a [`Track`]. Returns `None` for compact stubs (objects
/// without a title) and for non-track objects.
pub fn parse_track(v: &Value) -> Option<Track> {
    if v.get("kind").and_then(Value::as_str).is_some_and(|k| k != "track") {
        return None;
    }
    let id = json_id(v)?;
    let title = v.get("title")?.as_str()?.trim().to_owned();
    let user = v.get("user");
    let meta = v.get("publisher_metadata");

    let artist = meta
        .and_then(|m| non_empty(m.get("artist")))
        .or_else(|| user.and_then(|u| non_empty(u.get("username"))))
        .unwrap_or_default()
        .to_owned();
    let album = meta
        .and_then(|m| non_empty(m.get("album_title")))
        .unwrap_or_default()
        .to_owned();
    let duration_ms = v
        .get("full_duration")
        .and_then(Value::as_u64)
        .filter(|&d| d > 0)
        .or_else(|| v.get("duration").and_then(Value::as_u64))
        .unwrap_or(0);
    let art = non_empty(v.get("artwork_url"))
        .or_else(|| user.and_then(|u| non_empty(u.get("avatar_url"))))
        .filter(|u| !is_default_avatar(u))
        .map(upscale_art);
    let uri = non_empty(v.get("permalink_url")).unwrap_or_default().to_owned();

    Some(Track {
        album_artist: String::new(),
        disc_no: None,
        id: Track::soundcloud_id(id),
        source: Source::SoundCloud,
        title,
        artist,
        album,
        duration_ms,
        track_no: None,
        art,
        uri,
        added_at: 0,
    })
}

fn parse_user(v: &Value) -> Option<ScUser> {
    let id = json_id(v)?;
    let username = non_empty(v.get("username"))?.to_owned();
    let avatar = non_empty(v.get("avatar_url"))
        .filter(|u| !is_default_avatar(u))
        .map(upscale_art);
    let permalink_url = non_empty(v.get("permalink_url"))
        .map(str::to_owned)
        .or_else(|| non_empty(v.get("permalink")).map(|p| format!("https://soundcloud.com/{p}")))
        .unwrap_or_default();
    Some(ScUser {
        id,
        username,
        avatar,
        permalink_url,
        followers: v.get("followers_count").and_then(Value::as_u64),
    })
}

/// Whether a SoundCloud set is an album (or EP, single, compilation) rather than a playlist.
fn is_album(v: &Value) -> bool {
    v.get("is_album").and_then(Value::as_bool).unwrap_or(false)
        || matches!(
            v.get("set_type").and_then(Value::as_str),
            Some("album" | "ep" | "single" | "compilation")
        )
}

/// A playlist or album in search results.
pub fn parse_set_hit(v: &Value) -> Option<CollectionHit> {
    let url = non_empty(v.get("permalink_url"))?.to_owned();
    let title = non_empty(v.get("title"))?.trim().to_owned();
    let user = v.get("user");
    // Sets without their own artwork show their first song's, then the uploader's picture.
    let image = non_empty(v.get("artwork_url"))
        .or_else(|| {
            v.get("tracks")
                .and_then(Value::as_array)
                .and_then(|t| t.iter().find_map(|t| non_empty(t.get("artwork_url"))))
        })
        .or_else(|| {
            user.and_then(|u| non_empty(u.get("avatar_url")))
                .filter(|u| !is_default_avatar(u))
        })
        .map(upscale_art);
    let year = ["release_date", "display_date", "created_at"]
        .iter()
        .find_map(|k| non_empty(v.get(*k)))
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse().ok());
    let album = is_album(v);
    Some(CollectionHit {
        key: url,
        title,
        by: user
            .and_then(|u| non_empty(u.get("username")))
            .unwrap_or_default()
            .to_owned(),
        image,
        source: Source::SoundCloud,
        album,
        songs: v.get("track_count").and_then(Value::as_u64).map(|n| n as u32),
        year: if album { year } else { None },
    })
}

/// A user as an artist search result.
pub fn artist_hit(u: &ScUser) -> ArtistHit {
    ArtistHit {
        key: format!("soundcloud:user:{}", u.id),
        name: u.username.clone(),
        image: u.avatar.clone(),
        source: Source::SoundCloud,
        subtitle: match u.followers {
            Some(n) => format!("{} followers", crate::model::human_count(n)),
            None => "SoundCloud artist".into(),
        },
    }
}

/// Builds the playlist from its JSON and the (ordered) track ids, looking the tracks up in
/// `tracks`. Ids that couldn't be loaded are dropped.
fn assemble_playlist(pl: &Value, order: &[u64], tracks: &HashMap<u64, Track>) -> ImportedPlaylist {
    let tracks: Vec<Track> = order.iter().filter_map(|id| tracks.get(id).cloned()).collect();
    let art = non_empty(pl.get("artwork_url"))
        .map(upscale_art)
        .or_else(|| tracks.iter().find_map(|t| t.art.clone()));
    ImportedPlaylist {
        remote_id: json_id(pl).map(|id| id.to_string()).unwrap_or_default(),
        name: non_empty(pl.get("title")).unwrap_or("Untitled playlist").to_owned(),
        description: non_empty(pl.get("description")).unwrap_or_default().to_owned(),
        art,
        tracks,
    }
}

/// Numeric `id` field (SoundCloud ids are numbers, tolerate numeric strings).
fn json_id(v: &Value) -> Option<u64> {
    match v.get("id")? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn non_empty(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// SoundCloud image URLs come in fixed sizes; "-large" is 100x100, "-t500x500" is 500x500.
fn upscale_art(url: &str) -> String {
    url.replace("-large.", "-t500x500.")
}

fn is_default_avatar(url: &str) -> bool {
    url.contains("default_avatar")
}

/// A saved song.
pub struct ScDownload {
    pub path: PathBuf,
    pub kind: DownloadKind,
    pub meta: Metadata,
}

/// Tags for a downloaded song, from its api-v2 JSON (release details come from the
/// `publisher_metadata` labels and distributors fill in).
fn download_metadata(v: &Value, track: &Track) -> Metadata {
    let parsed = parse_track(v);
    let pm = v.get("publisher_metadata");
    let text = |value: Option<&Value>| non_empty(value).unwrap_or_default().trim().to_string();
    let artist = parsed
        .as_ref()
        .map(|t| t.artist.clone())
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| track.artist.clone());
    let title = parsed
        .as_ref()
        .map(|t| t.title.clone())
        .unwrap_or_else(|| track.title.clone());
    let album = [
        pm.and_then(|m| m.get("album_title")),
        pm.and_then(|m| m.get("release_title")),
    ]
    .into_iter()
    .map(text)
    .find(|a| !a.is_empty())
    .unwrap_or_default();
    // "2019-05-03T00:00:00Z" → "2019-05-03"; the release date when the uploader set one.
    let date = ["release_date", "display_date", "created_at"]
        .iter()
        .map(|k| text(v.get(*k)))
        .find(|d| d.len() >= 4 && d[..4].chars().all(|c| c.is_ascii_digit()))
        .map(|d| d.chars().take(10).collect())
        .unwrap_or_default();
    let copyright = [
        pm.and_then(|m| m.get("p_line_for_display")),
        pm.and_then(|m| m.get("p_line")),
        pm.and_then(|m| m.get("c_line_for_display")),
        pm.and_then(|m| m.get("c_line")),
    ]
    .into_iter()
    .map(text)
    .find(|c| !c.is_empty())
    .unwrap_or_default();
    let artwork = non_empty(v.get("artwork_url"))
        .or_else(|| v.get("user").and_then(|u| non_empty(u.get("avatar_url"))))
        .filter(|u| !is_default_avatar(u));
    // The full-size upload first, then 500x500.
    let cover_urls = artwork
        .map(|u| {
            let mut urls = Vec::new();
            if u.contains("-large.") {
                urls.push(u.replace("-large.", "-original."));
            }
            urls.push(upscale_art(u));
            urls
        })
        .unwrap_or_default();
    Metadata {
        title,
        album_artist: artist.clone(),
        artist,
        album,
        genre: text(v.get("genre")),
        date,
        isrc: text(pm.and_then(|m| m.get("isrc"))),
        label: [v.get("label_name"), pm.and_then(|m| m.get("publisher"))]
            .into_iter()
            .map(text)
            .find(|l| !l.is_empty())
            .unwrap_or_default(),
        copyright,
        url: text(v.get("permalink_url")),
        cover_urls,
        ..Metadata::default()
    }
}

/// Where a downloaded file came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadKind {
    /// The uploader's own file (they allow downloads); often lossless.
    Original,
    /// The stream SoundCloud plays.
    Stream,
}

/// Streams worth saving, best first: MP3 (plays and tags everywhere), then Opus, then AAC.
/// Encrypted streams are left out; previews come last (and are then refused).
fn ranked_download_transcodings(transcodings: &[Value]) -> Vec<&Value> {
    rank_transcodings(transcodings, |protocol, mime| match (protocol, mime) {
        ("hls", m) if m.starts_with("audio/mpeg") => Some(0),
        ("progressive", m) if m.starts_with("audio/mpeg") => Some(1),
        ("hls", m) if m.starts_with("audio/ogg") => Some(2),
        ("hls", m) if m.starts_with("audio/mp4") => Some(3),
        _ => None,
    })
}

/// Transcodings in order of preference: `format` ranks a (protocol, mime type), `None` = never.
/// Within a format "hq" comes first; 30 second previews come after every full stream, and
/// encrypted (DRM) streams are never used.
fn rank_transcodings(transcodings: &[Value], format: impl Fn(&str, &str) -> Option<u32>) -> Vec<&Value> {
    let mut ranked: Vec<(u32, &Value)> = transcodings
        .iter()
        .filter_map(|t| {
            t.get("url").and_then(Value::as_str).filter(|u| !u.is_empty())?;
            let protocol = t.pointer("/format/protocol").and_then(Value::as_str).unwrap_or("");
            let mime = t.pointer("/format/mime_type").and_then(Value::as_str).unwrap_or("");
            if protocol.contains("encrypted") {
                return None;
            }
            let format = format(protocol, mime)?;
            let snipped = t.get("snipped").and_then(Value::as_bool).unwrap_or(false);
            let hq = t.get("quality").and_then(Value::as_str) == Some("hq");
            Some((u32::from(snipped) * 100 + format * 2 + u32::from(!hq), t))
        })
        .collect();
    ranked.sort_by_key(|(r, _)| *r);
    ranked.into_iter().map(|(_, t)| t).collect()
}

/// Segments of an HLS media playlist (plus the fMP4 init segment, if any).
#[derive(Debug, PartialEq, Eq)]
struct Hls {
    init: Option<String>,
    segments: Vec<String>,
}

fn parse_hls(text: &str, base: &str) -> Result<Hls> {
    let base = Url::parse(base).context("bad playlist URL")?;
    let resolve = |uri: &str| -> Result<String> { Ok(base.join(uri).context("bad segment URL")?.to_string()) };
    let mut init = None;
    let mut segments = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            if !attrs.contains("METHOD=NONE") {
                bail!("the stream is encrypted (DRM), so it can't be saved");
            }
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            let uri = attrs
                .split("URI=\"")
                .nth(1)
                .and_then(|r| r.split('"').next())
                .context("HLS init segment without URI")?;
            init = Some(resolve(uri)?);
        } else if !line.is_empty() && !line.starts_with('#') {
            segments.push(resolve(line)?);
        }
    }
    if segments.is_empty() {
        bail!("the stream playlist is empty");
    }
    Ok(Hls { init, segments })
}

/// Streams a response body into `path`, reporting progress when the size is known.
async fn save_body(
    http: &reqwest::Client,
    url: &str,
    path: &Path,
    progress: &(dyn Fn(f32) + Send + Sync),
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    // Files can be large (WAV originals): no overall time limit, but a stalled
    // connection gives up after a minute.
    let mut resp = http
        .get(url)
        .timeout(Duration::from_secs(4 * 3600))
        .send()
        .await
        .context("download request failed")?
        .error_for_status()?;
    let total = resp.content_length().filter(|n| *n > 0);
    let mut file = tokio::fs::File::create(path).await?;
    let mut done = 0u64;
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(60), resp.chunk())
            .await
            .map_err(|_| anyhow!("the download stalled"))??;
        let Some(chunk) = chunk else { break };
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        // Hours of studio-quality WAV stay well below this.
        if done > 4 << 30 {
            bail!("the file is too big");
        }
        if let Some(total) = total {
            progress((done as f32 / total as f32).min(1.0));
        }
    }
    file.flush().await?;
    if done == 0 {
        bail!("the download was empty");
    }
    Ok(())
}

/// File extension for audio data, from its first bytes.
fn sniff_ext(head: &[u8]) -> Option<&'static str> {
    let at = |i: usize, sig: &[u8]| head.get(i..i + sig.len()) == Some(sig);
    Some(if at(0, b"fLaC") {
        "flac"
    } else if at(0, b"RIFF") && at(8, b"WAVE") {
        "wav"
    } else if at(0, b"FORM") && (at(8, b"AIFF") || at(8, b"AIFC")) {
        "aiff"
    } else if at(0, b"OggS") {
        if head.windows(8).any(|w| w == b"OpusHead") {
            "opus"
        } else {
            "ogg"
        }
    } else if at(4, b"ftyp") {
        "m4a"
    } else if at(0, b"ID3") || (head.len() > 1 && head[0] == 0xFF && head[1] & 0xE0 == 0xE0) {
        "mp3"
    } else {
        return None;
    })
}

async fn sniff_file_ext(path: &Path) -> Option<&'static str> {
    use tokio::io::AsyncReadExt;
    let mut head = [0u8; 64];
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let n = file.read(&mut head).await.ok()?;
    sniff_ext(&head[..n])
}

/// "Artist - Title" as a safe file name (the title alone when it already names the artist).
pub fn file_stem(artist: &str, title: &str) -> String {
    let (artist, title) = (artist.trim(), title.trim());
    let name = if artist.is_empty() || title.contains(" - ") || title.to_lowercase().starts_with(&artist.to_lowercase())
    {
        title.to_string()
    } else {
        format!("{artist} - {title}")
    };
    let mut clean = String::new();
    for c in name.chars() {
        let c = if c.is_control() || r#"/\:*?"<>|"#.contains(c) {
            '_'
        } else {
            c
        };
        // File names are limited to 255 bytes; leave room for " (2)" and the extension.
        if clean.len() + c.len_utf8() > 200 {
            break;
        }
        clean.push(c);
    }
    let clean = clean.trim().trim_matches('.').trim().to_string();
    if clean.is_empty() {
        return "SoundCloud track".into();
    }
    // Names Windows keeps for devices, even with an extension ("CON.mp3").
    let base = clean.split('.').next().unwrap_or("").trim().to_ascii_uppercase();
    let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit());
    if reserved {
        format!("_{clean}")
    } else {
        clean
    }
}

/// `dir/stem.ext`, or `dir/stem (2).ext` and so on if that exists.
/// Moves a finished file to the first free "stem.ext" / "stem (2).ext" … in `dir`. The name is
/// claimed by creating it, so two downloads finishing at once never take the same one.
pub fn save_unique(file: &Path, dir: &Path, stem: &str, ext: &str) -> std::io::Result<PathBuf> {
    let names = std::iter::once(format!("{stem}.{ext}")).chain((2..1000).map(|n| format!("{stem} ({n}).{ext}")));
    for name in names {
        let target = dir.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&target) {
            Ok(_) => {
                // Replaces the empty file just made.
                return match std::fs::rename(file, &target) {
                    Ok(()) => Ok(target),
                    Err(e) => {
                        let _ = std::fs::remove_file(&target);
                        Err(e)
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("too many files with this name"))
}

/// Streams mpv can play, best first: HLS MP3 > HLS Opus > progressive MP3 > HLS AAC > anything
/// else. ("Progressive" MP3s are often still listed but gone.) Previews come after every full
/// stream, and DRM encrypted streams are never used.
fn ranked_transcodings(transcodings: &[Value]) -> Vec<&Value> {
    rank_transcodings(transcodings, |protocol, mime| {
        Some(match (protocol, mime) {
            ("hls", m) if m.starts_with("audio/mpeg") => 0,
            ("hls", m) if m.starts_with("audio/ogg") => 1,
            ("progressive", m) if m.starts_with("audio/mpeg") => 2,
            ("hls", m) if m.starts_with("audio/mp4") => 3,
            _ => 4,
        })
    })
}

// -------------------------------------------------------------------------------------------
// client_id scraping
// -------------------------------------------------------------------------------------------

static SCRIPT_SRC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<script\b[^>]*?\bsrc\s*=\s*["']([^"']+)["']"#).expect("valid regex"));

static CLIENT_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"client_id\s*[:=]\s*"([a-zA-Z0-9]{32})"|client_id=([a-zA-Z0-9]{32})\b"#).expect("valid regex")
});

/// Asset bundle URLs (`https://a-v2.sndcdn.com/assets/*.js`) referenced by the homepage, in
/// page order.
fn extract_script_urls(html: &str) -> Vec<String> {
    SCRIPT_SRC_RE
        .captures_iter(html)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str())
        .filter(|src| src.contains("sndcdn.com/assets/") && src.contains(".js"))
        .map(|src| match src.strip_prefix("//") {
            Some(rest) => format!("https://{rest}"),
            None => src.to_owned(),
        })
        .collect()
}

fn extract_client_id(text: &str) -> Option<String> {
    let caps = CLIENT_ID_RE.captures(text)?;
    caps.get(1).or_else(|| caps.get(2)).map(|m| m.as_str().to_owned())
}

// -------------------------------------------------------------------------------------------
// Small helpers
// -------------------------------------------------------------------------------------------

fn api(path: &str) -> String {
    // Tests of the whole app point this at a stand-in for SoundCloud.
    static BASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let base = BASE.get_or_init(|| std::env::var("MULTIMUSIC_SOUNDCLOUD_API").unwrap_or_else(|_| API_BASE.to_string()));
    format!("{base}{path}")
}

/// Whether `url` is SoundCloud's (or the stand-in tests point Sumo at), so it may get the login.
fn is_soundcloud_api(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    (url.scheme() == "https" && (host == "soundcloud.com" || host.ends_with(".soundcloud.com")))
        || url.as_str().starts_with(&api(""))
}

/// `url` with `params` and `client_id` added. Any `client_id` already present (e.g. in a
/// `next_href`) is replaced so a refreshed id is always used.
fn build_url(url: &str, params: &[(&str, &str)], client_id: &str) -> Result<Url> {
    let mut url = Url::parse(url).with_context(|| format!("invalid SoundCloud URL: {url}"))?;
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "client_id")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (k, v) in &kept {
            query.append_pair(k, v);
        }
        for (k, v) in params {
            query.append_pair(k, v);
        }
        query.append_pair("client_id", client_id);
    }
    Ok(url)
}

/// The path of a URL, for logs and errors (never includes query parameters / credentials).
fn url_path(url: &str) -> String {
    Url::parse(url)
        .map(|u| u.path().to_owned())
        .unwrap_or_else(|_| url.split('?').next().unwrap_or(url).to_owned())
}

fn retry_after(headers: &HeaderMap) -> Duration {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.clamp(1, 60)))
        .unwrap_or(DEFAULT_RETRY_AFTER)
}

fn status_error(status: StatusCode, what: &str, body: &str, sent_token: bool) -> anyhow::Error {
    let code = status.as_u16();
    match status {
        StatusCode::UNAUTHORIZED if sent_token => anyhow!(
            "SoundCloud rejected the OAuth token (HTTP 401) for {what}; it may have expired, \
             copy a fresh oauth_token cookie from soundcloud.com"
        ),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => anyhow!(
            "SoundCloud denied access (HTTP {code}) to {what}; it may be private or not \
             available in your region"
        ),
        StatusCode::NOT_FOUND => anyhow!("not found on SoundCloud (HTTP 404): {what}"),
        StatusCode::TOO_MANY_REQUESTS => {
            anyhow!("SoundCloud rate limit hit for {what}; try again in a minute")
        }
        _ => {
            let snippet: String = body.trim().chars().take(200).collect();
            anyhow!("SoundCloud API error (HTTP {code}) for {what}: {snippet}")
        }
    }
}

/// Turns a profile URL, `soundcloud.com/name` or a bare username into
/// `https://soundcloud.com/<name>`. Other hosts (e.g. on.soundcloud.com short links) are
/// passed through for `/resolve` to follow.
fn normalize_profile_url(input: &str) -> Result<String> {
    let s = input.trim().trim_start_matches('@');
    if s.is_empty() {
        bail!("no SoundCloud profile URL configured");
    }
    let with_scheme = if s.contains("://") {
        s.to_owned()
    } else if s.split('/').next().is_some_and(|first| first.contains('.')) {
        format!("https://{s}")
    } else {
        format!("https://soundcloud.com/{s}")
    };
    let url = Url::parse(&with_scheme).with_context(|| format!("invalid SoundCloud profile URL: {input}"))?;
    match url.host_str() {
        Some("soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com") => {
            let name = url
                .path_segments()
                .and_then(|mut segs| segs.find(|s| !s.is_empty()))
                .with_context(|| format!("{input} doesn't contain a SoundCloud username"))?;
            Ok(format!("https://soundcloud.com/{name}"))
        }
        Some(_) => Ok(url.to_string()),
        None => bail!("invalid SoundCloud profile URL: {input}"),
    }
}

/// Parses SoundCloud timestamps ("2024-02-29T12:00:00Z", "2014/05/24 18:30:05 +0000") to Unix
/// seconds (UTC).
fn parse_timestamp(s: &str) -> Option<i64> {
    let nums: Vec<i64> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .take(6)
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let &[y, mo, d, h, mi, sec] = nums.as_slice() else {
        return None;
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_track_fixture() -> Value {
        json!({
            "artwork_url": "https://i1.sndcdn.com/artworks-000067273316-smsiqx-large.jpg",
            "caption": null,
            "comment_count": 1532,
            "created_at": "2008-07-02T22:09:23Z",
            "description": "From the Soulhack album",
            "duration": 30000,
            "full_duration": 213890,
            "genre": "Electronica",
            "id": 293,
            "kind": "track",
            "media": {
                "transcodings": [
                    {
                        "url": "https://api-v2.soundcloud.com/media/soundcloud:tracks:293/0ff8d3c1/stream/hls",
                        "preset": "mp3_1_0",
                        "duration": 30000,
                        "snipped": true,
                        "format": { "protocol": "hls", "mime_type": "audio/mpeg" },
                        "quality": "sq"
                    }
                ]
            },
            "monetization_model": "SUB_HIGH_TIER",
            "permalink": "flickermood",
            "permalink_url": "https://soundcloud.com/forss/flickermood",
            "policy": "SNIP",
            "publisher_metadata": {
                "id": 293,
                "urn": "soundcloud:tracks:293",
                "artist": "Forss",
                "album_title": "Soulhack",
                "contains_music": true,
                "isrc": "SEWNV0800101"
            },
            "streamable": true,
            "title": "Flickermood",
            "track_authorization": "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.e30.sig",
            "urn": "soundcloud:tracks:293",
            "user_id": 183,
            "user": {
                "avatar_url": "https://i1.sndcdn.com/avatars-000005478225-7ec8jg-large.jpg",
                "first_name": "",
                "id": 183,
                "kind": "user",
                "permalink": "forss",
                "permalink_url": "https://soundcloud.com/forss",
                "username": "Forss",
                "verified": true
            }
        })
    }

    #[test]
    fn set_search_hits() {
        let album = serde_json::json!({
            "permalink_url": "https://soundcloud.com/bladee/sets/eversince", "title": "Eversince",
            "artwork_url": null, "track_count": 13, "is_album": true, "set_type": "album",
            "release_date": "2016-05-04T00:00:00Z", "user": {"username": "bladee"},
            "tracks": [{"artwork_url": "https://i1.sndcdn.com/artworks-1-large.jpg"}]
        });
        let a = parse_set_hit(&album).unwrap();
        assert_eq!(a.key, "https://soundcloud.com/bladee/sets/eversince");
        assert!(a.album);
        assert_eq!(a.year, Some(2016));
        assert_eq!(
            a.image.as_deref(),
            Some("https://i1.sndcdn.com/artworks-1-t500x500.jpg")
        );
        assert_eq!(a.subtitle(), "bladee · 2016");
        let list = serde_json::json!({
            "permalink_url": "https://soundcloud.com/someone/sets/late-night", "title": "late night",
            "track_count": 1, "set_type": "", "user": {"username": "someone",
            "avatar_url": "https://i1.sndcdn.com/avatars-x-large.jpg"}
        });
        let p = parse_set_hit(&list).unwrap();
        assert!(!p.album);
        assert_eq!(p.year, None);
        assert_eq!(p.image.as_deref(), Some("https://i1.sndcdn.com/avatars-x-t500x500.jpg"));
        assert_eq!(p.subtitle(), "by someone · 1 song");
        assert!(parse_set_hit(&serde_json::json!({"title": "no link"})).is_none());
    }

    #[test]
    fn parse_track_maps_full_object() {
        let t = parse_track(&full_track_fixture()).expect("track");
        assert_eq!(
            t,
            Track {
                album_artist: String::new(),
                disc_no: None,
                id: "soundcloud:293".into(),
                source: Source::SoundCloud,
                title: "Flickermood".into(),
                artist: "Forss".into(),
                album: "Soulhack".into(),
                duration_ms: 213_890,
                track_no: None,
                art: Some("https://i1.sndcdn.com/artworks-000067273316-smsiqx-t500x500.jpg".into()),
                uri: "https://soundcloud.com/forss/flickermood".into(),
                added_at: 0,
            }
        );
    }

    #[test]
    fn parse_track_falls_back_to_uploader_and_avatar() {
        let v = json!({
            "artwork_url": null,
            "duration": 184_000,
            "id": 1_234_567_890u64,
            "kind": "track",
            "permalink_url": "https://soundcloud.com/some-dj/live-mix",
            "publisher_metadata": { "id": 1_234_567_890u64, "urn": "soundcloud:tracks:1234567890", "artist": "", "contains_music": true },
            "title": "Live Mix @ Somewhere ",
            "user": {
                "avatar_url": "https://i1.sndcdn.com/avatars-abcDEF123-xyz-large.png",
                "id": 42,
                "kind": "user",
                "permalink_url": "https://soundcloud.com/some-dj",
                "username": "Some DJ"
            }
        });
        let t = parse_track(&v).expect("track");
        assert_eq!(t.id, "soundcloud:1234567890");
        assert_eq!(t.title, "Live Mix @ Somewhere");
        assert_eq!(t.artist, "Some DJ");
        assert_eq!(t.album, "");
        assert_eq!(t.duration_ms, 184_000);
        assert_eq!(
            t.art.as_deref(),
            Some("https://i1.sndcdn.com/avatars-abcDEF123-xyz-t500x500.png")
        );
        assert_eq!(t.uri, "https://soundcloud.com/some-dj/live-mix");

        // Default avatars are no cover art.
        let mut v = v;
        v["user"]["avatar_url"] = json!("https://a1.sndcdn.com/images/default_avatar_large.png");
        assert_eq!(parse_track(&v).unwrap().art, None);
    }

    #[test]
    fn parse_track_rejects_stubs_and_non_tracks() {
        let stub = json!({ "id": 1_548_320_001u64, "kind": "track", "monetization_model": "NOT_APPLICABLE", "policy": "ALLOW" });
        assert_eq!(parse_track(&stub), None);
        let playlist = json!({ "id": 99, "kind": "playlist", "title": "Not a track" });
        assert_eq!(parse_track(&playlist), None);
        assert_eq!(parse_track(&json!({ "title": "no id" })), None);
    }

    #[test]
    fn parse_user_maps_fields() {
        let u = parse_user(&full_track_fixture()["user"]).unwrap();
        assert_eq!(
            u,
            ScUser {
                id: 183,
                username: "Forss".into(),
                avatar: Some("https://i1.sndcdn.com/avatars-000005478225-7ec8jg-t500x500.jpg".into()),
                permalink_url: "https://soundcloud.com/forss".into(),
                followers: None,
            }
        );
        let mut with_followers = full_track_fixture()["user"].clone();
        with_followers["followers_count"] = serde_json::json!(1234);
        assert_eq!(parse_user(&with_followers).unwrap().followers, Some(1234));
    }

    #[test]
    fn artist_hits_from_users() {
        let mut v = full_track_fixture()["user"].clone();
        v["followers_count"] = serde_json::json!(2_500_000);
        let hit = artist_hit(&parse_user(&v).unwrap());
        assert_eq!(hit.key, "soundcloud:user:183");
        assert_eq!(hit.name, "Forss");
        assert_eq!(hit.subtitle, "2.5M followers");
        assert_eq!(hit.source, Source::SoundCloud);
    }

    #[test]
    fn extracts_client_id_from_js() {
        let js = r#"(window.webpackJsonp=window.webpackJsonp||[]).push([[49],{123:function(e,t,n){"use strict";var r=n(4);e.exports={env:"production",client_id:"a3e059563d7fd3372b49b37f00a00bcf",api_host:"https://api-v2.soundcloud.com"}}}]);"#;
        assert_eq!(
            extract_client_id(js).as_deref(),
            Some("a3e059563d7fd3372b49b37f00a00bcf")
        );

        let assign = r#"var o={};o.client_id = "Zx9YwVuTsRqPoNmLkJiHgFeDcBa98765";"#;
        assert_eq!(
            extract_client_id(assign).as_deref(),
            Some("Zx9YwVuTsRqPoNmLkJiHgFeDcBa98765")
        );

        let query =
            r#"fetch("https://api-v2.soundcloud.com/me?client_id=0123456789abcdefABCDEF0123456789&app_version=1")"#;
        assert_eq!(
            extract_client_id(query).as_deref(),
            Some("0123456789abcdefABCDEF0123456789")
        );

        // Wrong lengths don't match.
        assert_eq!(extract_client_id(r#"client_id:"tooshort""#), None);
        assert_eq!(extract_client_id("client_id=0123456789abcdefABCDEF0123456789XYZ"), None);
        assert_eq!(extract_client_id("nothing to see here"), None);
    }

    #[test]
    fn extracts_asset_scripts_from_html() {
        let html = r#"<!DOCTYPE html><html><head>
<script src="https://www.google.com/recaptcha/api.js"></script>
<link rel="stylesheet" href="https://a-v2.sndcdn.com/assets/app-1f2e3d.css">
</head><body>
<script>window.__sc_hydration = [];</script>
<script crossorigin src="https://a-v2.sndcdn.com/assets/0-8a7d7b0e.js"></script>
<script crossorigin src="https://a-v2.sndcdn.com/assets/2-45b2bd1a.js"></script>
<script src="//a-v2.sndcdn.com/assets/49-4786a8a0.js" crossorigin></script>
</body></html>"#;
        assert_eq!(
            extract_script_urls(html),
            vec![
                "https://a-v2.sndcdn.com/assets/0-8a7d7b0e.js",
                "https://a-v2.sndcdn.com/assets/2-45b2bd1a.js",
                "https://a-v2.sndcdn.com/assets/49-4786a8a0.js",
            ]
        );
    }

    fn transcoding(protocol: &str, mime: &str, snipped: bool, quality: &str) -> Value {
        json!({
            "url": format!("https://api-v2.soundcloud.com/media/soundcloud:tracks:1/abc/stream/{protocol}?m={mime}"),
            "preset": "x",
            "duration": 213890,
            "snipped": snipped,
            "format": { "protocol": protocol, "mime_type": mime },
            "quality": quality
        })
    }

    fn picked(list: &[Value]) -> Option<(String, String, bool)> {
        ranked_transcodings(list).first().map(|t| {
            (
                t["format"]["protocol"].as_str().unwrap().to_owned(),
                t["format"]["mime_type"].as_str().unwrap().to_owned(),
                t["snipped"].as_bool().unwrap(),
            )
        })
    }

    #[test]
    fn transcoding_preference() {
        let aac = transcoding("hls", r#"audio/mp4; codecs="mp4a.40.2""#, false, "sq");
        let hls_mp3 = transcoding("hls", "audio/mpeg", false, "sq");
        let prog_mp3 = transcoding("progressive", "audio/mpeg", false, "sq");
        let opus = transcoding("hls", r#"audio/ogg; codecs="opus""#, false, "sq");
        let drm = transcoding("ctr-encrypted-hls", r#"audio/mp4; codecs="mp4a.40.2""#, false, "hq");

        let all = [
            drm.clone(),
            aac.clone(),
            hls_mp3.clone(),
            prog_mp3.clone(),
            opus.clone(),
        ];
        // HLS MP3 first; the rest stay as fallbacks in order.
        let order: Vec<(String, String)> = ranked_transcodings(&all)
            .iter()
            .map(|t| {
                (
                    t["format"]["protocol"].as_str().unwrap().to_owned(),
                    t["format"]["mime_type"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(order[0], ("hls".to_string(), "audio/mpeg".to_string()));
        assert!(order[1].1.starts_with("audio/ogg"));
        assert_eq!(order[2].0, "progressive");
        assert!(order[3].1.starts_with("audio/mp4"));
        assert_eq!(order.len(), 4);

        let no_mp3 = [aac.clone(), opus.clone()];
        assert!(picked(&no_mp3).unwrap().1.starts_with("audio/ogg"));

        let mp3_or_aac = [aac.clone(), hls_mp3.clone()];
        assert_eq!(picked(&mp3_or_aac).unwrap().1, "audio/mpeg");

        assert!(picked(&[drm.clone(), aac.clone()]).unwrap().1.starts_with("audio/mp4"));

        // Unknown formats are still better than nothing; encrypted ones are never used.
        let odd = transcoding("hls", "audio/flac", false, "sq");
        assert_eq!(picked(&[drm.clone(), odd]).unwrap().1, "audio/flac");
        assert_eq!(picked(std::slice::from_ref(&drm)), None);
        assert_eq!(picked(&[]), None);

        // Within a format, hq wins.
        let aac_hq = transcoding("hls", r#"audio/mp4; codecs="mp4a.40.2""#, false, "hq");
        let both = [aac.clone(), aac_hq];
        assert_eq!(ranked_transcodings(&both)[0]["quality"], "hq");
    }

    #[test]
    fn snipped_transcodings_are_a_last_resort() {
        let snip_prog = transcoding("progressive", "audio/mpeg", true, "sq");
        let full_aac = transcoding("hls", "audio/mp4", false, "sq");
        assert!(!picked(&[snip_prog.clone(), full_aac]).unwrap().2);
        assert_eq!(
            picked(&[snip_prog]).unwrap(),
            ("progressive".into(), "audio/mpeg".into(), true)
        );
    }

    #[test]
    fn login_only_goes_to_soundcloud() {
        let ok = |u: &str| is_soundcloud_api(&Url::parse(u).unwrap());
        assert!(ok("https://api-v2.soundcloud.com/me"));
        assert!(ok("https://api.soundcloud.com/tracks/1/streams"));
        assert!(!ok("http://api-v2.soundcloud.com/me"));
        assert!(!ok("https://soundcloud.com.evil.example/me"));
        assert!(!ok("https://evil.example/?u=api-v2.soundcloud.com"));
    }

    #[test]
    fn build_url_replaces_client_id() {
        let next =
            "https://api-v2.soundcloud.com/users/183/track_likes?offset=1709208000000%2C123&limit=200&client_id=OLD";
        let url = build_url(next, &[], "NEW").unwrap();
        assert_eq!(
            url.as_str(),
            "https://api-v2.soundcloud.com/users/183/track_likes?offset=1709208000000%2C123&limit=200&client_id=NEW"
        );
        let url = build_url(&api("/search/tracks"), &[("q", "daft punk & co"), ("limit", "5")], "ID").unwrap();
        assert_eq!(
            url.as_str(),
            "https://api-v2.soundcloud.com/search/tracks?q=daft+punk+%26+co&limit=5&client_id=ID"
        );
        assert_eq!(url_path(url.as_str()), "/search/tracks");
    }

    #[test]
    fn playlist_assembly_restores_order() {
        let pl = json!({
            "id": 1_234_567,
            "kind": "playlist",
            "title": "Late night",
            "description": null,
            "artwork_url": null,
            "track_count": 3,
            "tracks": [ { "id": 2 }, { "id": 1 }, { "id": 3 } ]
        });
        let mut tracks = HashMap::new();
        for (id, art) in [
            (1u64, None),
            (2, Some("https://i1.sndcdn.com/a-t500x500.jpg")),
            (3, None),
        ] {
            let mut v = full_track_fixture();
            v["id"] = json!(id);
            v["title"] = json!(format!("T{id}"));
            let mut t = parse_track(&v).unwrap();
            t.art = art.map(str::to_owned);
            tracks.insert(id, t);
        }
        // Track 3 was unavailable.
        tracks.remove(&3);
        let p = assemble_playlist(&pl, &[2, 1, 3], &tracks);
        assert_eq!(p.remote_id, "1234567");
        assert_eq!(p.name, "Late night");
        assert_eq!(p.description, "");
        assert_eq!(p.art.as_deref(), Some("https://i1.sndcdn.com/a-t500x500.jpg"));
        let titles: Vec<_> = p.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["T2", "T1"]);
    }

    #[test]
    fn profile_url_normalization() {
        let n = |s: &str| normalize_profile_url(s).unwrap();
        assert_eq!(n("forss"), "https://soundcloud.com/forss");
        assert_eq!(n("@forss"), "https://soundcloud.com/forss");
        assert_eq!(n("soundcloud.com/forss/"), "https://soundcloud.com/forss");
        assert_eq!(
            n("https://m.soundcloud.com/forss/likes?ref=x#top"),
            "https://soundcloud.com/forss"
        );
        assert_eq!(n(" https://soundcloud.com/forss "), "https://soundcloud.com/forss");
        assert_eq!(n("https://on.soundcloud.com/AbCdE"), "https://on.soundcloud.com/AbCdE");
        assert!(normalize_profile_url("").is_err());
        assert!(normalize_profile_url("https://soundcloud.com/").is_err());
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(parse_timestamp("2023-11-05T08:15:30.123Z"), Some(1_699_172_130));
        assert_eq!(parse_timestamp("2014/05/24 18:30:05 +0000"), Some(1_400_956_205));
        assert_eq!(parse_timestamp("1969-12-31T23:59:59Z"), Some(-1));
        assert_eq!(parse_timestamp("garbage"), None);
        assert_eq!(parse_timestamp("2024-13-01T00:00:00Z"), None);
    }

    #[test]
    fn constructor_trims_settings() {
        let sc = SoundCloud::new(reqwest::Client::new(), "  ", " OAuth 2-123456-789-abc ");
        assert_eq!(sc.client_id_override, None);
        assert_eq!(sc.oauth_token.as_deref(), Some("2-123456-789-abc"));
        assert_eq!(sc.token(false), Some("2-123456-789-abc"));
        sc.token_rejected.store(true, Ordering::Relaxed);
        assert_eq!(sc.token(false), None);
        assert_eq!(sc.token(true), Some("2-123456-789-abc"));

        let sc = SoundCloud::new(reqwest::Client::new(), "abc", "");
        assert_eq!(sc.oauth_token, None);
        let id = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(sc.client_id())
            .unwrap();
        assert_eq!(id, "abc");
    }

    #[test]
    fn futures_are_send() {
        fn assert_send<T: Send>(_: &T) {}
        let sc = SoundCloud::new(reqwest::Client::new(), "", "");
        let t = parse_track(&full_track_fixture()).unwrap();
        assert_send(&sc.client_id());
        assert_send(&sc.resolve_user("forss"));
        assert_send(&sc.me());
        assert_send(&sc.likes(1));
        assert_send(&sc.playlists(1));
        assert_send(&sc.search("x", 1));
        assert_send(&sc.stream_url(&t));
    }

    /// Manual smoke test against the real API: `cargo test soundcloud -- --ignored`.
    #[tokio::test]
    #[ignore = "needs network access to soundcloud.com"]
    async fn smoke_public_profile() {
        let sc = SoundCloud::new(crate::http::client(), "", "");
        let user = sc.resolve_user("https://soundcloud.com/forss").await.unwrap();
        assert_eq!(user.id, 183);
        let likes = sc.likes(user.id).await.unwrap();
        println!("{} likes", likes.len());
        let playlists = sc.playlists(user.id).await.unwrap();
        for p in &playlists {
            println!("playlist {}: {} tracks", p.name, p.tracks.len());
        }
        let found = sc.search("forss flickermood", 5).await.unwrap();
        assert!(!found.is_empty());
        let url = sc.stream_url(&found[0]).await.unwrap();
        assert!(url.starts_with("https://"), "{url}");
    }

    #[test]
    fn download_picks_mp3_and_never_encrypted_streams() {
        let t = |protocol: &str, mime: &str, snipped: bool| {
            json!({
                "url": format!("https://api-v2.soundcloud.com/media/{protocol}/{mime}/{snipped}"),
                "format": { "protocol": protocol, "mime_type": mime },
                "snipped": snipped,
            })
        };
        let all = vec![
            t("ctr-encrypted-hls", "audio/mp4; codecs=\"mp4a.40.2\"", false),
            t("hls", "audio/ogg; codecs=\"opus\"", false),
            t("hls", "audio/mpeg", false),
            t("progressive", "audio/mpeg", true),
        ];
        let pick = |list: &[Value]| {
            ranked_download_transcodings(list)
                .first()
                .map(|v| v["url"].as_str().unwrap().to_string())
        };
        // A full MP3 stream beats a progressive preview and Opus.
        assert!(pick(&all).unwrap().ends_with("/hls/audio/mpeg/false"));
        assert!(pick(&all[..2]).unwrap().contains("/hls/audio/ogg"));
        // Only a preview left: it is picked (and then refused by the caller).
        assert!(pick(&all[3..]).unwrap().ends_with("/true"));
        // Encrypted streams are never used.
        assert_eq!(pick(&all[..1]), None);
    }

    #[test]
    fn hls_playlists() {
        let text = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:10.0,\nseg/0.m4s?x=1\n\
                    #EXTINF:5.0,\nhttps://cdn.example/abs/1.m4s\n#EXT-X-ENDLIST\n";
        let hls = parse_hls(
            text,
            "https://cf-hls-media.sndcdn.com/playlist/abc/playlist.m3u8?Policy=p",
        )
        .unwrap();
        assert_eq!(
            hls.init.as_deref(),
            Some("https://cf-hls-media.sndcdn.com/playlist/abc/init.mp4")
        );
        assert_eq!(
            hls.segments,
            vec![
                "https://cf-hls-media.sndcdn.com/playlist/abc/seg/0.m4s?x=1".to_string(),
                "https://cdn.example/abs/1.m4s".to_string()
            ]
        );
        let plain = "#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:1,\na.mp3\n";
        assert_eq!(parse_hls(plain, "https://h/p.m3u8").unwrap().segments.len(), 1);
        let encrypted = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:1,\na.mp4\n";
        let err = parse_hls(encrypted, "https://h/p.m3u8").unwrap_err().to_string();
        assert!(err.contains("encrypted"), "{err}");
        assert!(parse_hls("#EXTM3U\n#EXT-X-ENDLIST\n", "https://h/p.m3u8").is_err());
    }

    #[test]
    fn sniffs_audio_formats() {
        assert_eq!(sniff_ext(b"fLaC\0\0\0\x22"), Some("flac"));
        assert_eq!(sniff_ext(b"RIFF\x24\0\0\0WAVEfmt "), Some("wav"));
        assert_eq!(sniff_ext(b"FORM\0\0\0\0AIFF"), Some("aiff"));
        assert_eq!(
            sniff_ext(b"OggS\0\x02\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x01\x13OpusHead"),
            Some("opus")
        );
        assert_eq!(sniff_ext(b"OggS\0\x02\0\0\x01vorbis"), Some("ogg"));
        assert_eq!(sniff_ext(b"\0\0\0\x20ftypM4A "), Some("m4a"));
        assert_eq!(sniff_ext(b"ID3\x04\0"), Some("mp3"));
        assert_eq!(sniff_ext(&[0xFF, 0xFB, 0x90, 0x64]), Some("mp3"));
        assert_eq!(sniff_ext(b"<html>"), None);
        assert_eq!(sniff_ext(b""), None);
    }

    #[test]
    fn download_file_names() {
        assert_eq!(file_stem("Flume", "Never Be Like You"), "Flume - Never Be Like You");
        // Titles that already name the artist are kept as they are.
        assert_eq!(
            file_stem("someuploader", "Flume - Never Be Like You"),
            "Flume - Never Be Like You"
        );
        assert_eq!(file_stem("Flume", "flume x chet faker"), "flume x chet faker");
        assert_eq!(file_stem("", "Song"), "Song");
        // Characters file systems (or other OSes) reject are replaced.
        assert_eq!(file_stem("A/B", "What?: \"Yes\" <3"), "A_B - What__ _Yes_ _3");
        assert_eq!(file_stem("", " ..."), "SoundCloud track");
        assert!(file_stem("x", &"long ".repeat(100)).len() <= 200);
        // Counted in bytes: 255 is the most a file name can have.
        let wide = file_stem("", &"歌".repeat(150));
        assert!(wide.len() <= 200 && wide.starts_with('歌'), "{}", wide.len());
        assert_eq!(file_stem("", "con"), "_con");
        assert_eq!(file_stem("", "COM1.final"), "_COM1.final");
        assert_eq!(file_stem("", "Console"), "Console");

        let dir = std::env::temp_dir().join(format!("multimusic-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = |name: &str, data: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            p
        };
        // Free names first, then numbered ones; saving claims the name, so files finishing
        // together never overwrite each other.
        let first = save_unique(&part("a.part", b"A"), &dir, "Song", "mp3").unwrap();
        assert_eq!(first, dir.join("Song.mp3"));
        let second = save_unique(&part("b.part", b"B"), &dir, "Song", "mp3").unwrap();
        assert_eq!(second, dir.join("Song (2).mp3"));
        let flac = save_unique(&part("c.part", b"C"), &dir, "Song", "flac").unwrap();
        assert_eq!(flac, dir.join("Song.flac"));
        assert_eq!(std::fs::read(&first).unwrap(), b"A");
        assert_eq!(std::fs::read(&second).unwrap(), b"B");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Serves a fake stream endpoint, HLS playlist and segments; returns the base URL and the
    /// request lines it saw.
    async fn media_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (b, log) = (base.clone(), seen.clone());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = head.lines().next().unwrap_or("").to_string();
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let (ctype, body): (&str, Vec<u8>) = match path.split('?').next().unwrap() {
                    "/stream/hls" => (
                        "application/json",
                        format!("{{\"url\":\"{b}/media/list.m3u8\"}}").into_bytes(),
                    ),
                    "/stream/progressive" => (
                        "application/json",
                        format!("{{\"url\":\"{b}/media/full.mp3\"}}").into_bytes(),
                    ),
                    "/media/list.m3u8" => (
                        "application/vnd.apple.mpegurl",
                        b"#EXTM3U\n#EXTINF:10,\nseg0.mp3\n#EXTINF:10,\nseg1.mp3\n#EXT-X-ENDLIST\n".to_vec(),
                    ),
                    "/media/seg0.mp3" => ("audio/mpeg", b"ID3\x04first-".to_vec()),
                    "/media/seg1.mp3" => ("audio/mpeg", b"second".to_vec()),
                    "/media/full.mp3" => ("audio/mpeg", [&[0xFF, 0xFB][..], &[7u8; 5000]].concat()),
                    _ => ("text/plain", Vec::new()),
                };
                let status = if body.is_empty() { "404 Not Found" } else { "200 OK" };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            }
        });
        (base, seen)
    }

    #[tokio::test]
    async fn downloads_streams_from_hls_and_progressive() {
        let (base, seen) = media_server().await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let sc = SoundCloud::new(http, "testid", "");
        let dir = std::env::temp_dir().join(format!("multimusic-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join(".part");
        let progress = std::sync::Mutex::new(Vec::new());
        let report = |p: f32| progress.lock().unwrap().push(p);

        let json = json!({
            "track_authorization": "tok",
            "media": { "transcodings": [
                { "url": format!("{base}/stream/encrypted"), "format": { "protocol": "ctr-encrypted-hls", "mime_type": "audio/mp4" } },
                { "url": format!("{base}/stream/hls"), "format": { "protocol": "hls", "mime_type": "audio/mpeg" } },
            ]},
        });
        sc.download_stream(&json, "Song", &part, &report).await.unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), b"ID3\x04first-second");
        assert_eq!(sniff_file_ext(&part).await, Some("mp3"));
        assert_eq!(*progress.lock().unwrap(), vec![0.5, 1.0]);
        assert!(seen
            .lock()
            .unwrap()
            .contains(&"/stream/hls?track_authorization=tok&client_id=testid".to_string()));

        // A stream SoundCloud still lists but no longer has (HTTP 404) is skipped.
        let json = json!({ "media": { "transcodings": [
            { "url": format!("{base}/stream/gone"), "quality": "hq", "format": { "protocol": "hls", "mime_type": "audio/mpeg" } },
            { "url": format!("{base}/stream/hls"), "quality": "sq", "format": { "protocol": "hls", "mime_type": "audio/mpeg" } },
        ]}});
        sc.download_stream(&json, "Song", &part, &report).await.unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), b"ID3\x04first-second");
        assert!(seen.lock().unwrap().iter().any(|p| p.starts_with("/stream/gone")));

        let json = json!({ "media": { "transcodings": [
            { "url": format!("{base}/stream/progressive"), "format": { "protocol": "progressive", "mime_type": "audio/mpeg" } },
        ]}});
        sc.download_stream(&json, "Song", &part, &report).await.unwrap();
        assert_eq!(std::fs::metadata(&part).unwrap().len(), 5002);
        assert_eq!(progress.lock().unwrap().last(), Some(&1.0));

        // Go+ previews and encrypted-only songs are refused, not saved as 30 second clips.
        let json = json!({ "media": { "transcodings": [
            { "url": format!("{base}/stream/progressive"), "snipped": true, "format": { "protocol": "progressive", "mime_type": "audio/mpeg" } },
        ]}});
        let err = sc.download_stream(&json, "Song", &part, &report).await.unwrap_err();
        assert!(err.to_string().contains("30 second preview"), "{err}");
        let json = json!({ "media": { "transcodings": [
            { "url": format!("{base}/stream/encrypted"), "format": { "protocol": "ctr-encrypted-hls", "mime_type": "audio/mp4" } },
        ]}});
        let err = sc.download_stream(&json, "Song", &part, &report).await.unwrap_err();
        assert!(err.to_string().contains("encrypted"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
