//! Apple Music library import.
//!
//! Apple Music streams are DRM protected and can't be played on Linux, so Sumo only imports
//! the user's library and playlists as *metadata*. Songs that point at an existing local file
//! become [`Source::Local`] tracks straight away; everything else becomes a [`Source::AppleMusic`]
//! track that is resolved to a local / Spotify / SoundCloud match at play time.
//!
//! Two import paths are supported:
//! - [`import_library_xml`]: an iTunes / Apple Music "Library.xml" export
//!   (File > Library > Export Library on macOS / Windows). No account needed.
//! - [`AppleMusicApi`]: the Apple Music web API (`/v1/me/library/...`), which needs a developer
//!   token (see [`scrape_developer_token`]) and a user token from a signed-in web player session.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use regex::Regex;
use reqwest::{header, StatusCode};
use serde_json::Value as Json;

use crate::model::{ImportedPlaylist, Source, Track};

/// Everything imported from an Apple Music library.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppleLibrary {
    pub tracks: Vec<Track>,
    pub playlists: Vec<ImportedPlaylist>,
}

// ---------------------------------------------------------------------------------------------
// Library.xml import
// ---------------------------------------------------------------------------------------------

/// Track flags that mark non-music items we don't import.
const SKIPPED_TRACK_FLAGS: [&str; 6] = ["Podcast", "Movie", "TV Show", "Music Video", "Has Video", "iTunesU"];

/// Import an iTunes / Apple Music "Library.xml" export (File > Library > Export Library on macOS/Windows).
pub fn import_library_xml(path: &Path) -> Result<AppleLibrary> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read Apple Music library export {}", path.display()))?;
    // Reading (and freeing) the plist recurses once per level: a crafted file nested
    // thousands deep would crash the app, while a real export is a handful of levels.
    if !nesting_ok(&bytes, 64) {
        bail!("{} is not an iTunes / Apple Music library export", path.display());
    }
    let root = plist::Value::from_reader_xml(std::io::Cursor::new(bytes))
        .with_context(|| format!("failed to read Apple Music library export {}", path.display()))?;
    let library = parse_library_plist(&root)
        .with_context(|| format!("{} is not an iTunes / Apple Music library export", path.display()))?;
    tracing::info!(
        tracks = library.tracks.len(),
        playlists = library.playlists.len(),
        local = library.tracks.iter().filter(|t| t.source == Source::Local).count(),
        "imported Apple Music library export"
    );
    Ok(library)
}

/// Whether an XML plist nests its arrays and dictionaries at most `max` deep.
fn nesting_ok(xml: &[u8], max: usize) -> bool {
    let mut depth = 0usize;
    let mut rest = xml;
    while let Some(at) = rest.iter().position(|&b| b == b'<') {
        rest = &rest[at + 1..];
        let end = rest.iter().position(|&b| b == b'>').unwrap_or(rest.len());
        let tag = &rest[..end];
        let opens = tag == b"array" || tag == b"dict" || tag.starts_with(b"array ") || tag.starts_with(b"dict ");
        if opens {
            depth += 1;
            if depth > max {
                return false;
            }
        } else if tag == b"/array" || tag == b"/dict" {
            depth = depth.saturating_sub(1);
        }
        rest = &rest[end.min(rest.len())..];
    }
    true
}

fn parse_library_plist(root: &plist::Value) -> Result<AppleLibrary> {
    let root = root.as_dictionary().context("the root element is not a dictionary")?;
    let tracks_dict = root
        .get("Tracks")
        .and_then(plist::Value::as_dictionary)
        .context("missing \"Tracks\" dictionary")?;

    // Library track id -> index into `tracks`, so playlists can reference them.
    let mut index: HashMap<i64, usize> = HashMap::with_capacity(tracks_dict.len());
    let mut tracks = Vec::with_capacity(tracks_dict.len());
    for (key, value) in tracks_dict.iter() {
        let Some(dict) = value.as_dictionary() else {
            continue;
        };
        let Some(track_id) = int(dict, "Track ID").or_else(|| key.trim().parse().ok()) else {
            continue;
        };
        if let Some(track) = xml_track(dict, track_id) {
            index.insert(track_id, tracks.len());
            tracks.push(track);
        }
    }

    let playlists = root
        .get("Playlists")
        .and_then(plist::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(plist::Value::as_dictionary)
                .filter_map(|dict| xml_playlist(dict, &index, &tracks))
                .collect()
        })
        .unwrap_or_default();

    Ok(AppleLibrary { tracks, playlists })
}

/// Convert one entry of the "Tracks" dictionary. `None` for podcasts, videos, radio streams etc.
fn xml_track(d: &plist::Dictionary, track_id: i64) -> Option<Track> {
    if SKIPPED_TRACK_FLAGS.iter().any(|key| flag(d, key)) {
        return None;
    }
    let kind = string(d, "Kind").unwrap_or_default().to_lowercase();
    if kind.contains("podcast") || kind.contains("video") {
        return None;
    }
    // Internet radio stations ("Track Type" = "URL", http Location) aren't songs.
    if string(d, "Track Type") == Some("URL") {
        return None;
    }

    let local_path = string(d, "Location")
        .and_then(file_url_to_path)
        .filter(|p| Path::new(p).is_file());

    let title = non_empty(string(d, "Name"))
        .map(str::to_owned)
        .or_else(|| {
            local_path
                .as_deref()
                .and_then(|p| Path::new(p).file_stem())
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Unknown".to_owned());
    let artist = non_empty(string(d, "Artist"))
        .or_else(|| non_empty(string(d, "Album Artist")))
        .unwrap_or_default()
        .to_owned();
    let album = non_empty(string(d, "Album")).unwrap_or_default().to_owned();
    let duration_ms = int(d, "Total Time").unwrap_or(0).max(0) as u64;
    let track_no = int(d, "Track Number")
        .filter(|n| *n > 0)
        .and_then(|n| u32::try_from(n).ok());
    let added_at = d
        .get("Date Added")
        .and_then(plist::Value::as_date)
        .map(plist_date_to_unix)
        .unwrap_or(0);

    let (id, source, uri) = match local_path {
        Some(path) => (Track::local_id(&path), Source::Local, path),
        None => {
            let pid = non_empty(string(d, "Persistent ID"))
                .map(str::to_owned)
                .unwrap_or_else(|| track_id.to_string());
            (Track::applemusic_id(&pid), Source::AppleMusic, String::new())
        }
    };

    Some(Track {
        album_artist: String::new(),
        disc_no: None,
        id,
        source,
        title,
        artist,
        album,
        duration_ms,
        track_no,
        art: None,
        uri,
        added_at,
    })
}

/// Convert one entry of the "Playlists" array. `None` for built-in, folder and empty playlists.
fn xml_playlist(d: &plist::Dictionary, index: &HashMap<i64, usize>, tracks: &[Track]) -> Option<ImportedPlaylist> {
    let hidden = d.get("Visible").and_then(plist::Value::as_boolean) == Some(false);
    if flag(d, "Master") || flag(d, "Folder") || hidden || d.contains_key("Distinguished Kind") {
        return None;
    }

    // Smart playlists are kept: the export contains their evaluated items.
    let items: Vec<Track> = d
        .get("Playlist Items")
        .and_then(plist::Value::as_array)?
        .iter()
        .filter_map(plist::Value::as_dictionary)
        .filter_map(|item| int(item, "Track ID"))
        .filter_map(|id| index.get(&id).map(|&i| tracks[i].clone()))
        .collect();
    if items.is_empty() {
        return None;
    }

    let remote_id = non_empty(string(d, "Playlist Persistent ID"))
        .map(str::to_owned)
        .or_else(|| int(d, "Playlist ID").map(|id| id.to_string()))?;

    Some(ImportedPlaylist {
        remote_id,
        name: non_empty(string(d, "Name")).unwrap_or("Untitled Playlist").to_owned(),
        description: non_empty(string(d, "Description")).unwrap_or_default().to_owned(),
        art: None,
        tracks: items,
    })
}

/// Decode a `file://` URL from a library export into a filesystem path.
///
/// - `file://localhost/Users/x/Music/A%20B.m4a` -> `/Users/x/Music/A B.m4a`
/// - `file:///home/x/a.flac` -> `/home/x/a.flac`
/// - `file://localhost/C:/Users/x/a.mp3` -> `/C:/Users/x/a.mp3` (kept as is; never exists on Linux)
///
/// Returns `None` for anything that isn't a `file:` URL (http radio streams etc).
pub fn file_url_to_path(url: &str) -> Option<String> {
    let url = url.trim();
    let scheme = url.get(..7)?;
    if !scheme.eq_ignore_ascii_case("file://") {
        return None;
    }
    let rest = &url[7..];
    // Real '?' / '#' in file names are percent-encoded, so literal ones start a query/fragment.
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let path = match rest.find('/') {
        Some(0) => rest.to_owned(),
        Some(i) => {
            let host = &rest[..i];
            if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
                rest[i..].to_owned()
            } else {
                // A network share (file://server/share/...).
                format!("//{rest}")
            }
        }
        None => return None,
    };
    let decoded = urlencoding::decode_binary(path.as_bytes());
    let decoded = String::from_utf8_lossy(&decoded).into_owned();
    (decoded.len() > 1).then_some(decoded)
}

fn flag(d: &plist::Dictionary, key: &str) -> bool {
    d.get(key).and_then(plist::Value::as_boolean).unwrap_or(false)
}

fn string<'a>(d: &'a plist::Dictionary, key: &str) -> Option<&'a str> {
    d.get(key).and_then(plist::Value::as_string)
}

fn int(d: &plist::Dictionary, key: &str) -> Option<i64> {
    let v = d.get(key)?;
    v.as_signed_integer()
        .or_else(|| v.as_unsigned_integer().map(|u| i64::try_from(u).unwrap_or(i64::MAX)))
        .or_else(|| v.as_string().and_then(|s| s.trim().parse().ok()))
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn plist_date_to_unix(date: plist::Date) -> i64 {
    let time: SystemTime = date.into();
    time.duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Apple Music web API
// ---------------------------------------------------------------------------------------------

const API_BASE: &str = "https://api.music.apple.com";
const WEB_PLAYER: &str = "https://music.apple.com";
/// Maximum page size the library endpoints accept.
const PAGE_LIMIT: u32 = 100;
/// Safety net against a `next` loop: 5000 pages * 100 = 500k items.
const MAX_PAGES: usize = 5000;
/// Retries for 429 / 5xx responses.
const MAX_RETRIES: u32 = 3;
/// Edge length of the cover art requested from Apple's artwork templates.
const ART_SIZE: u32 = 300;
/// The web player bundle is served to browsers; look like one when scraping it.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0";

/// Returned (inside `anyhow::Error`) when Apple rejects the tokens.
#[derive(Debug)]
struct AuthError(StatusCode);

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Apple Music rejected the request ({}): the developer token or the Music-User-Token is \
             invalid or has expired. Sign in to music.apple.com again to get a fresh user token \
             (and re-fetch the developer token).",
            self.0
        )
    }
}

impl std::error::Error for AuthError {}

/// True when Apple rejected the tokens (as opposed to a network or parsing problem).
pub fn is_auth_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<AuthError>().is_some()
}

/// Client for the user's Apple Music library (`/v1/me/library/...`).
#[derive(Clone)]
pub struct AppleMusicApi {
    http: reqwest::Client,
    developer_token: String,
    user_token: String,
    storefront: String,
}

impl fmt::Debug for AppleMusicApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the tokens.
        f.debug_struct("AppleMusicApi")
            .field("storefront", &self.storefront)
            .finish_non_exhaustive()
    }
}

impl AppleMusicApi {
    pub fn new(http: reqwest::Client, developer_token: &str, user_token: &str, storefront: &str) -> Self {
        let storefront = storefront.trim();
        Self {
            http,
            developer_token: developer_token.trim().trim_start_matches("Bearer ").trim().to_owned(),
            user_token: user_token.trim().to_owned(),
            storefront: if storefront.is_empty() {
                "us".to_owned()
            } else {
                storefront.to_lowercase()
            },
        }
    }

    /// The catalog storefront (country code) this account uses, e.g. "us".
    #[cfg(test)]
    pub fn storefront(&self) -> &str {
        &self.storefront
    }

    /// Every song in the user's library.
    pub async fn library_songs(&self) -> Result<Vec<Track>> {
        let items = self
            .collect_pages(&format!("/v1/me/library/songs?limit={PAGE_LIMIT}"))
            .await
            .context("failed to fetch the Apple Music library songs")?;
        let tracks: Vec<Track> = items.iter().filter_map(parse_library_song).collect();
        tracing::info!(songs = tracks.len(), "fetched Apple Music library songs");
        Ok(tracks)
    }

    /// Every playlist in the user's library, with full track lists.
    pub async fn library_playlists(&self) -> Result<Vec<ImportedPlaylist>> {
        let items = self
            .collect_pages(&format!("/v1/me/library/playlists?limit={PAGE_LIMIT}"))
            .await
            .context("failed to fetch the Apple Music library playlists")?;

        let mut playlists = Vec::with_capacity(items.len());
        for item in &items {
            let Some(mut playlist) = parse_library_playlist(item) else {
                continue;
            };
            match self.playlist_tracks(&playlist.remote_id).await {
                Ok(tracks) => playlist.tracks = tracks,
                Err(e) if e.downcast_ref::<AuthError>().is_some() => return Err(e),
                Err(e) => {
                    tracing::warn!(playlist = %playlist.name, "skipping Apple Music playlist: {e:#}");
                    continue;
                }
            }
            playlists.push(playlist);
        }
        tracing::info!(playlists = playlists.len(), "fetched Apple Music library playlists");
        Ok(playlists)
    }

    async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        let path = format!(
            "/v1/me/library/playlists/{}/tracks?limit={PAGE_LIMIT}",
            urlencoding::encode(playlist_id)
        );
        // A 404 here just means the playlist is empty.
        let items = self.collect_pages(&path).await?;
        Ok(items.iter().filter_map(parse_library_song).collect())
    }

    /// An artist's top songs (catalog, no user token needed).
    pub async fn catalog_artist(&self, id: &str) -> Result<CatalogPage> {
        let sf = &self.storefront;
        let id = urlencoding::encode(id);
        let artist = self
            .get_json(&format!("/v1/catalog/{sf}/artists/{id}"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Apple Music artist not found"))?;
        let attrs = first_attributes(&artist);
        let mut songs = self
            .collect_pages(&format!("/v1/catalog/{sf}/artists/{id}/view/top-songs?limit=20"))
            .await
            .unwrap_or_default();
        if songs.is_empty() {
            songs = self
                .collect_pages(&format!("/v1/catalog/{sf}/artists/{id}/songs?limit=20"))
                .await?;
        }
        let tracks: Vec<Track> = songs.iter().filter_map(parse_library_song).collect();
        Ok(CatalogPage {
            title: attr_text(attrs, "name"),
            subtitle: format!("Artist · {} songs", tracks.len()),
            image: attrs.and_then(json_artwork),
            tracks,
        })
    }

    pub async fn catalog_album(&self, id: &str) -> Result<CatalogPage> {
        let sf = &self.storefront;
        let id = urlencoding::encode(id);
        let album = self
            .get_json(&format!("/v1/catalog/{sf}/albums/{id}"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Apple Music album not found"))?;
        let attrs = first_attributes(&album);
        let tracks: Vec<Track> = self
            .collect_pages(&format!("/v1/catalog/{sf}/albums/{id}/tracks?limit=100"))
            .await?
            .iter()
            .filter_map(parse_library_song)
            .collect();
        Ok(CatalogPage {
            title: attr_text(attrs, "name"),
            subtitle: format!("Album · {} · {} songs", attr_text(attrs, "artistName"), tracks.len()),
            image: attrs.and_then(json_artwork),
            tracks,
        })
    }

    pub async fn catalog_playlist(&self, id: &str) -> Result<CatalogPage> {
        let sf = &self.storefront;
        let id = urlencoding::encode(id);
        let playlist = self
            .get_json(&format!("/v1/catalog/{sf}/playlists/{id}"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Apple Music playlist not found"))?;
        let attrs = first_attributes(&playlist);
        let tracks: Vec<Track> = self
            .collect_pages(&format!("/v1/catalog/{sf}/playlists/{id}/tracks?limit=100"))
            .await?
            .iter()
            .filter_map(parse_library_song)
            .collect();
        let curator = attr_text(attrs, "curatorName");
        Ok(CatalogPage {
            title: attr_text(attrs, "name"),
            subtitle: if curator.is_empty() {
                format!("Playlist · {} songs", tracks.len())
            } else {
                format!("Playlist · {curator} · {} songs", tracks.len())
            },
            image: attrs.and_then(json_artwork),
            tracks,
        })
    }

    pub async fn catalog_song(&self, id: &str) -> Result<CatalogPage> {
        let sf = &self.storefront;
        let song = self
            .get_json(&format!("/v1/catalog/{sf}/songs/{}", urlencoding::encode(id)))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Apple Music song not found"))?;
        let track = song
            .get("data")
            .and_then(Json::as_array)
            .and_then(|d| d.first())
            .and_then(parse_library_song)
            .ok_or_else(|| anyhow::anyhow!("unexpected Apple Music song response"))?;
        Ok(CatalogPage {
            title: track.title.clone(),
            subtitle: format!("Song · {}", track.artist),
            image: track.art.clone(),
            tracks: vec![track],
        })
    }

    /// Fetch `first` and every following `next` page, returning all `data` items.
    async fn collect_pages(&self, first: &str) -> Result<Vec<Json>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut next = Some(first.to_owned());
        while let Some(url) = next.take() {
            if !seen.insert(url.clone()) || seen.len() > MAX_PAGES {
                tracing::warn!(%url, "stopping Apple Music pagination (loop or too many pages)");
                break;
            }
            let Some(mut page) = self.get_json(&url).await? else {
                break;
            };
            if let Some(Json::Array(data)) = page.get_mut("data").map(Json::take) {
                out.extend(data);
            }
            next = page.get("next").and_then(Json::as_str).map(next_page_url);
        }
        Ok(out)
    }

    /// GET an API path (or absolute URL). `Ok(None)` for 404 / empty responses.
    async fn get_json(&self, path_or_url: &str) -> Result<Option<Json>> {
        let url = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            path_or_url.to_owned()
        } else {
            format!("{API_BASE}{path_or_url}")
        };

        let mut attempt = 0;
        loop {
            let mut req = self
                .http
                .get(&url)
                .header(header::AUTHORIZATION, format!("Bearer {}", self.developer_token));
            // Catalog lookups work without a user token.
            if !self.user_token.is_empty() {
                req = req.header("Music-User-Token", &self.user_token);
            }
            let resp = req
                .header(header::ORIGIN, WEB_PLAYER)
                .header(header::REFERER, format!("{WEB_PLAYER}/"))
                .header(header::ACCEPT, "application/json")
                .send()
                .await
                .with_context(|| format!("request to {url} failed"))?;
            let status = resp.status();

            if status.is_success() {
                let body = resp.text().await.with_context(|| format!("failed to read {url}"))?;
                if body.trim().is_empty() {
                    return Ok(None);
                }
                let json =
                    serde_json::from_str(&body).with_context(|| format!("invalid JSON from Apple Music ({url})"))?;
                return Ok(Some(json));
            }

            match status {
                StatusCode::NOT_FOUND => return Ok(None),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Err(AuthError(status).into()),
                StatusCode::TOO_MANY_REQUESTS
                | StatusCode::INTERNAL_SERVER_ERROR
                | StatusCode::BAD_GATEWAY
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT
                    if attempt < MAX_RETRIES =>
                {
                    attempt += 1;
                    let wait = resp
                        .headers()
                        .get(header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(|s| Duration::from_secs(s.min(30)))
                        .unwrap_or_else(|| Duration::from_millis(500 << attempt));
                    tracing::debug!(%status, ?wait, %url, "Apple Music API busy, retrying");
                    tokio::time::sleep(wait).await;
                }
                _ => {
                    let body = resp.text().await.unwrap_or_default();
                    let snippet: String = body.chars().take(300).collect();
                    bail!("Apple Music API returned {status} for {url}: {snippet}");
                }
            }
        }
    }
}

/// Header and songs of a catalog artist / album / playlist page.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogPage {
    pub title: String,
    pub subtitle: String,
    pub image: Option<String>,
    pub tracks: Vec<Track>,
}

fn first_attributes(v: &Json) -> Option<&Json> {
    v.get("data")?.as_array()?.first()?.get("attributes")
}

fn attr_text(attrs: Option<&Json>, key: &str) -> String {
    attrs
        .and_then(|a| a.get(key))
        .and_then(Json::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Apple's `next` links are relative and drop the `limit` parameter (falling back to 25 per
/// page); put it back so pagination keeps using full pages.
fn next_page_url(next: &str) -> String {
    let has_limit = next
        .split_once('?')
        .is_some_and(|(_, q)| q.split('&').any(|kv| kv.starts_with("limit=")));
    if has_limit {
        next.to_owned()
    } else {
        let sep = if next.contains('?') { '&' } else { '?' };
        format!("{next}{sep}limit={PAGE_LIMIT}")
    }
}

/// Map one `library-songs` resource from the Apple Music API to a [`Track`].
///
/// Returns `None` for music videos and malformed entries.
pub fn parse_library_song(v: &Json) -> Option<Track> {
    if v.get("type")
        .and_then(Json::as_str)
        .is_some_and(|t| t.contains("music-videos"))
    {
        return None;
    }
    let id = v
        .get("id")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let attrs = v.get("attributes")?;
    let text = |key: &str| attrs.get(key).and_then(Json::as_str).map(str::trim).unwrap_or_default();

    let title = text("name");
    if title.is_empty() {
        return None;
    }
    Some(Track {
        album_artist: String::new(),
        disc_no: None,
        id: Track::applemusic_id(id),
        source: Source::AppleMusic,
        title: title.to_owned(),
        artist: text("artistName").to_owned(),
        album: text("albumName").to_owned(),
        duration_ms: attrs.get("durationInMillis").and_then(Json::as_u64).unwrap_or(0),
        track_no: attrs
            .get("trackNumber")
            .and_then(Json::as_u64)
            .filter(|n| *n > 0)
            .and_then(|n| u32::try_from(n).ok()),
        art: json_artwork(attrs),
        uri: String::new(),
        added_at: attrs
            .get("dateAdded")
            .and_then(Json::as_str)
            .and_then(parse_iso8601)
            .unwrap_or(0),
    })
}

/// Map one `library-playlists` resource (without its tracks).
fn parse_library_playlist(v: &Json) -> Option<ImportedPlaylist> {
    let id = v
        .get("id")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let attrs = v.get("attributes");
    let name = attrs
        .and_then(|a| a.get("name"))
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Untitled Playlist");
    let description = attrs
        .and_then(|a| a.get("description"))
        .and_then(|d| {
            d.get("standard")
                .or(d.get("short"))
                .and_then(Json::as_str)
                .or(d.as_str())
        })
        .map(str::trim)
        .unwrap_or_default();
    Some(ImportedPlaylist {
        remote_id: id.to_owned(),
        name: name.to_owned(),
        description: description.to_owned(),
        art: attrs.and_then(json_artwork),
        tracks: Vec::new(),
    })
}

fn json_artwork(attrs: &Json) -> Option<String> {
    let url = attrs.get("artwork")?.get("url")?.as_str()?.trim();
    (!url.is_empty()).then(|| artwork_url(url, ART_SIZE))
}

/// Fill in an Apple artwork URL template (`.../{w}x{h}bb.jpg`) for a square image of `size` px.
pub fn artwork_url(template: &str, size: u32) -> String {
    let size = size.to_string();
    template
        .replace("{w}", &size)
        .replace("{h}", &size)
        .replace("{c}", "bb")
        .replace("{f}", "jpg")
}

/// Parse an ISO 8601 / RFC 3339 timestamp ("2023-01-15T12:34:56Z", "2023-01-15T12:34:56.789+01:00",
/// "2023-01-15") into unix seconds.
fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = match s.split_once(['T', 't', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut parts = date.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut secs = crate::clock::days_from_civil(year, month, day) * 86_400;

    if let Some(time) = time {
        let (clock, offset) = if let Some(clock) = time.strip_suffix(['Z', 'z']) {
            (clock, 0)
        } else if let Some(i) = time.rfind(['+', '-']) {
            (&time[..i], parse_utc_offset(&time[i..])?)
        } else {
            (time, 0)
        };
        let clock = clock.split('.').next().unwrap_or_default();
        let mut hms = clock.split(':');
        let h: i64 = hms.next()?.parse().ok()?;
        let m: i64 = hms.next().unwrap_or("0").parse().ok()?;
        let sec: i64 = hms.next().unwrap_or("0").parse().ok()?;
        if h > 24 || m > 59 || sec > 60 {
            return None;
        }
        secs += h * 3600 + m * 60 + sec - offset;
    }
    Some(secs)
}

/// "+01:00" / "-0530" / "+02" -> offset in seconds.
fn parse_utc_offset(s: &str) -> Option<i64> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.len() < 2 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let h: i64 = digits[..2].parse().ok()?;
    let m: i64 = digits.get(2..4).map_or(Some(0), |m| m.parse().ok())?;
    Some(sign * (h * 3600 + m * 60))
}

// ---------------------------------------------------------------------------------------------
// Developer token scraping
// ---------------------------------------------------------------------------------------------

static BUNDLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:https://music\.apple\.com)?/assets/[A-Za-z0-9._~-]+\.js"#).expect("valid regex"));

static JWT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"eyJh[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+").expect("valid regex"));

/// How many non-`index` bundles to try before giving up.
const MAX_FALLBACK_BUNDLES: usize = 8;

/// Best effort: scrape the public web player's developer token (JWT) from music.apple.com's JS bundle.
pub async fn scrape_developer_token(http: &reqwest::Client) -> Result<String> {
    let page_url = format!("{WEB_PLAYER}/us/browse");
    let html = fetch_text(http, &page_url).await?;
    let bundles = find_bundle_urls(&html);
    if bundles.is_empty() {
        bail!("no JavaScript bundle found on {page_url}; the Apple Music web player layout may have changed");
    }
    for bundle in &bundles {
        match fetch_text(http, bundle).await {
            Ok(js) => {
                if let Some(token) = find_jwt(&js) {
                    tracing::info!(%bundle, "found Apple Music developer token");
                    return Ok(token);
                }
            }
            Err(e) => tracing::warn!("failed to fetch Apple Music web player bundle: {e:#}"),
        }
    }
    bail!(
        "no developer token found in the Apple Music web player bundles ({} checked); \
         paste one manually instead",
        bundles.len()
    )
}

async fn fetch_text(http: &reqwest::Client, url: &str) -> Result<String> {
    let resp = http
        .get(url)
        .header(header::USER_AGENT, BROWSER_UA)
        .header(header::ACCEPT, "*/*")
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("{url} returned {status}");
    }
    resp.text().await.with_context(|| format!("failed to read {url}"))
}

/// Absolute URLs of the web player's JS bundles referenced by `html`, `index` bundles first
/// (that's where the token lives), then a few others as a fallback. De-duplicated.
fn find_bundle_urls(html: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let (mut index, mut other) = (Vec::new(), Vec::new());
    for m in BUNDLE_RE.find_iter(html) {
        let path = m.as_str();
        let url = if path.starts_with('/') {
            format!("{WEB_PLAYER}{path}")
        } else {
            path.to_owned()
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        if path.contains("/assets/index") {
            index.push(url);
        } else if other.len() < MAX_FALLBACK_BUNDLES {
            other.push(url);
        }
    }
    index.extend(other);
    index
}

/// The first JWT-looking string in `js`.
fn find_jwt(js: &str) -> Option<String> {
    JWT_RE.find(js).map(|m| m.as_str().to_owned())
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique, self-cleaning temp directory.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "multimusic-apple-music-test-{}-{}-{}",
                std::process::id(),
                nanos,
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Percent-encode a path the way iTunes does in "Location" (each segment encoded, '/' kept).
    fn file_url(path: &Path) -> String {
        let encoded: Vec<String> = path
            .to_str()
            .unwrap()
            .split('/')
            .map(|seg| urlencoding::encode(seg).into_owned())
            .collect();
        format!("file://localhost{}", encoded.join("/"))
    }

    fn library_xml(local_location: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple Computer//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Major Version</key><integer>1</integer>
	<key>Minor Version</key><integer>1</integer>
	<key>Application Version</key><string>1.4.5.7</string>
	<key>Date</key><date>2024-05-01T10:00:00Z</date>
	<key>Features</key><integer>5</integer>
	<key>Show Content Ratings</key><true/>
	<key>Music Folder</key><string>file:///Users/alice/Music/Music/Media.localized/</string>
	<key>Library Persistent ID</key><string>0123456789ABCDEF</string>
	<key>Tracks</key>
	<dict>
		<key>101</key>
		<dict>
			<key>Track ID</key><integer>101</integer>
			<key>Name</key><string>Local Song</string>
			<key>Album Artist</key><string>Album Guy</string>
			<key>Album</key><string>Home Recordings</string>
			<key>Kind</key><string>Apple Lossless audio file</string>
			<key>Size</key><integer>31415926</integer>
			<key>Total Time</key><integer>215000</integer>
			<key>Track Number</key><integer>3</integer>
			<key>Date Added</key><date>2020-01-02T03:04:05Z</date>
			<key>Persistent ID</key><string>A1A1A1A1A1A1A1A1</string>
			<key>Track Type</key><string>File</string>
			<key>Location</key><string>{local_location}</string>
		</dict>
		<key>102</key>
		<dict>
			<key>Track ID</key><integer>102</integer>
			<key>Name</key><string>Gone Song</string>
			<key>Artist</key><string>Missing Band</string>
			<key>Album Artist</key><string>Someone Else</string>
			<key>Album</key><string>Lost &amp; Found</string>
			<key>Kind</key><string>Purchased AAC audio file</string>
			<key>Total Time</key><integer>180500</integer>
			<key>Persistent ID</key><string>ABCDEF0123456789</string>
			<key>Track Type</key><string>File</string>
			<key>Location</key><string>file://localhost/Users/alice/Music/Gone%20Song.m4a</string>
		</dict>
		<key>103</key>
		<dict>
			<key>Track ID</key><integer>103</integer>
			<key>Name</key><string>Cloud Song</string>
			<key>Artist</key><string>Streamer</string>
			<key>Album</key><string>Online</string>
			<key>Kind</key><string>Apple Music AAC audio file</string>
			<key>Total Time</key><integer>200000</integer>
			<key>Track Number</key><integer>1</integer>
			<key>Date Added</key><date>2023-06-15T12:00:00Z</date>
			<key>Persistent ID</key><string>1111222233334444</string>
			<key>Track Type</key><string>Remote</string>
			<key>Apple Music</key><true/>
		</dict>
		<key>104</key>
		<dict>
			<key>Track ID</key><integer>104</integer>
			<key>Name</key><string>Episode 1</string>
			<key>Artist</key><string>Some Podcast</string>
			<key>Kind</key><string>MPEG audio file</string>
			<key>Persistent ID</key><string>DEADBEEFDEADBEEF</string>
			<key>Podcast</key><true/>
			<key>Track Type</key><string>File</string>
			<key>Location</key><string>{local_location}</string>
		</dict>
		<key>105</key>
		<dict>
			<key>Track ID</key><integer>105</integer>
			<key>Name</key><string>Some Video</string>
			<key>Artist</key><string>Video Star</string>
			<key>Kind</key><string>Purchased MPEG-4 video file</string>
			<key>Persistent ID</key><string>5555666677778888</string>
			<key>Has Video</key><true/>
			<key>Music Video</key><true/>
			<key>Track Type</key><string>File</string>
		</dict>
		<key>106</key>
		<dict>
			<key>Track ID</key><integer>106</integer>
			<key>Name</key><string>Windows Song</string>
			<key>Artist</key><string>Win Artist</string>
			<key>Kind</key><string>MPEG audio file</string>
			<key>Total Time</key><integer>99000</integer>
			<key>Persistent ID</key><string>9999AAAABBBBCCCC</string>
			<key>Track Type</key><string>File</string>
			<key>Location</key><string>file://localhost/C:/Users/bob/Music/Win%20Song.mp3</string>
		</dict>
	</dict>
	<key>Playlists</key>
	<array>
		<dict>
			<key>Name</key><string>Library</string>
			<key>Description</key><string></string>
			<key>Master</key><true/>
			<key>Playlist ID</key><integer>10</integer>
			<key>Playlist Persistent ID</key><string>LIBRARY000000000</string>
			<key>Visible</key><false/>
			<key>All Items</key><true/>
			<key>Playlist Items</key>
			<array>
				<dict><key>Track ID</key><integer>101</integer></dict>
				<dict><key>Track ID</key><integer>102</integer></dict>
				<dict><key>Track ID</key><integer>103</integer></dict>
			</array>
		</dict>
		<dict>
			<key>Name</key><string>Music</string>
			<key>Playlist ID</key><integer>11</integer>
			<key>Playlist Persistent ID</key><string>MUSIC00000000000</string>
			<key>Distinguished Kind</key><integer>4</integer>
			<key>Music</key><true/>
			<key>All Items</key><true/>
			<key>Playlist Items</key>
			<array>
				<dict><key>Track ID</key><integer>101</integer></dict>
				<dict><key>Track ID</key><integer>103</integer></dict>
			</array>
		</dict>
		<dict>
			<key>Name</key><string>My Folder</string>
			<key>Playlist ID</key><integer>12</integer>
			<key>Playlist Persistent ID</key><string>FOLDER0000000000</string>
			<key>Folder</key><true/>
			<key>All Items</key><true/>
			<key>Playlist Items</key>
			<array>
				<dict><key>Track ID</key><integer>102</integer></dict>
			</array>
		</dict>
		<dict>
			<key>Name</key><string>Road Trip</string>
			<key>Description</key><string>Songs for the car</string>
			<key>Playlist ID</key><integer>13</integer>
			<key>Playlist Persistent ID</key><string>AAAABBBBCCCCDDDD</string>
			<key>Parent Persistent ID</key><string>FOLDER0000000000</string>
			<key>All Items</key><true/>
			<key>Playlist Items</key>
			<array>
				<dict><key>Track ID</key><integer>103</integer></dict>
				<dict><key>Track ID</key><integer>104</integer></dict>
				<dict><key>Track ID</key><integer>101</integer></dict>
				<dict><key>Track ID</key><integer>999</integer></dict>
				<dict><key>Track ID</key><integer>102</integer></dict>
			</array>
		</dict>
		<dict>
			<key>Name</key><string>Smart Faves</string>
			<key>Playlist ID</key><integer>77</integer>
			<key>All Items</key><true/>
			<key>Smart Info</key>
			<data>
			AQEAAwAAAAIAAAAZAAAAAAAAAAcAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
			</data>
			<key>Smart Criteria</key>
			<data>
			U0xzdAABAAEAAAACAAAAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
			</data>
			<key>Playlist Items</key>
			<array>
				<dict><key>Track ID</key><integer>106</integer></dict>
			</array>
		</dict>
		<dict>
			<key>Name</key><string>Empty</string>
			<key>Playlist ID</key><integer>14</integer>
			<key>Playlist Persistent ID</key><string>EMPTY00000000000</string>
			<key>All Items</key><true/>
		</dict>
		<dict>
			<key>Name</key><string>Only Podcasts</string>
			<key>Playlist ID</key><integer>15</integer>
			<key>Playlist Persistent ID</key><string>PODS000000000000</string>
			<key>All Items</key><true/>
			<key>Playlist Items</key>
			<array>
				<dict><key>Track ID</key><integer>104</integer></dict>
			</array>
		</dict>
		<dict>
			<key>Name</key><string>Podcasts</string>
			<key>Playlist ID</key><integer>16</integer>
			<key>Playlist Persistent ID</key><string>PODCASTS00000000</string>
			<key>Distinguished Kind</key><integer>10</integer>
			<key>Podcasts</key><true/>
			<key>All Items</key><true/>
			<key>Playlist Items</key>
			<array>
				<dict><key>Track ID</key><integer>104</integer></dict>
			</array>
		</dict>
	</array>
</dict>
</plist>
"#
        )
    }

    #[test]
    fn deeply_nested_files_are_refused() {
        let ok =
            b"<plist><dict><key>Tracks</key><dict/><key>Playlists</key><array><dict></dict></array></dict></plist>";
        assert!(nesting_ok(ok, 64));
        let deep = format!(
            "<plist>{}{}</plist>",
            "<array>".repeat(200_000),
            "</array>".repeat(200_000)
        );
        assert!(!nesting_ok(deep.as_bytes(), 64));
        let dir = TempDir::new();
        let file = dir.0.join("Library.xml");
        std::fs::write(&file, deep).unwrap();
        assert!(import_library_xml(&file).is_err());
    }

    #[test]
    fn imports_library_xml() {
        let dir = TempDir::new();
        // A real audio file whose name needs percent-encoding.
        let audio = dir.0.join("My Song #1 (Ünïcødé).m4a");
        std::fs::write(&audio, b"not really audio").unwrap();
        let audio_str = audio.to_str().unwrap().to_owned();
        let location = file_url(&audio);
        assert!(location.contains("%20") && location.contains("%23"), "{location}");

        let xml_path = dir.0.join("Library.xml");
        std::fs::write(&xml_path, library_xml(&location)).unwrap();
        let lib = import_library_xml(&xml_path).unwrap();

        // Podcast (104) and music video (105) are skipped; file order is kept.
        let ids: Vec<&str> = lib.tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                Track::local_id(&audio_str).as_str(),
                "applemusic:ABCDEF0123456789",
                "applemusic:1111222233334444",
                "applemusic:9999AAAABBBBCCCC",
            ]
        );

        // Existing file -> Local with the decoded path.
        let local = &lib.tracks[0];
        assert_eq!(local.source, Source::Local);
        assert_eq!(local.uri, audio_str);
        assert_eq!(local.title, "Local Song");
        assert_eq!(local.artist, "Album Guy", "artist falls back to album artist");
        assert_eq!(local.album, "Home Recordings");
        assert_eq!(local.duration_ms, 215_000);
        assert_eq!(local.track_no, Some(3));
        assert_eq!(local.added_at, 1_577_934_245);
        assert_eq!(local.art, None);

        // Missing file -> Apple Music placeholder.
        let gone = &lib.tracks[1];
        assert_eq!(gone.source, Source::AppleMusic);
        assert_eq!(gone.uri, "");
        assert_eq!(gone.artist, "Missing Band");
        assert_eq!(gone.album, "Lost & Found");
        assert_eq!(gone.track_no, None);
        assert_eq!(gone.added_at, 0);

        // No Location at all (Apple Music cloud track).
        let cloud = &lib.tracks[2];
        assert_eq!(cloud.source, Source::AppleMusic);
        assert_eq!(cloud.title, "Cloud Song");
        assert_eq!(cloud.added_at, 1_686_830_400);

        // Windows path never exists on Linux.
        assert_eq!(lib.tracks[3].source, Source::AppleMusic);

        // Only "Road Trip" and the smart playlist survive.
        let names: Vec<&str> = lib.playlists.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Road Trip", "Smart Faves"]);

        let road = &lib.playlists[0];
        assert_eq!(road.remote_id, "AAAABBBBCCCCDDDD");
        assert_eq!(road.description, "Songs for the car");
        assert_eq!(road.art, None);
        // Original order, podcast (104) and unknown id (999) dropped.
        let road_ids: Vec<&str> = road.tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            road_ids,
            [
                "applemusic:1111222233334444",
                Track::local_id(&audio_str).as_str(),
                "applemusic:ABCDEF0123456789",
            ]
        );
        assert_eq!(road.tracks[1], lib.tracks[0]);

        let smart = &lib.playlists[1];
        assert_eq!(smart.remote_id, "77", "falls back to Playlist ID");
        assert_eq!(smart.tracks.len(), 1);
        assert_eq!(smart.tracks[0].title, "Windows Song");
    }

    #[test]
    fn rejects_non_library_files() {
        let dir = TempDir::new();
        let path = dir.0.join("NotALibrary.xml");
        std::fs::write(
            &path,
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><array><string>x</string></array></plist>"#,
        )
        .unwrap();
        assert!(import_library_xml(&path).is_err());
        assert!(import_library_xml(&dir.0.join("missing.xml")).is_err());
    }

    #[test]
    fn decodes_file_urls() {
        assert_eq!(
            file_url_to_path("file://localhost/Users/x/Music/A%20B.m4a").as_deref(),
            Some("/Users/x/Music/A B.m4a")
        );
        assert_eq!(
            file_url_to_path("file:///home/x/Music/Caf%C3%A9%20%26%20Bar%20%2325.flac").as_deref(),
            Some("/home/x/Music/Café & Bar #25.flac")
        );
        assert_eq!(
            file_url_to_path("file://localhost/C:/Users/bob/Music/Win%20Song.mp3").as_deref(),
            Some("/C:/Users/bob/Music/Win Song.mp3")
        );
        // '+' is a literal plus in file URLs, not a space.
        assert_eq!(file_url_to_path("FILE:///a+b.mp3").as_deref(), Some("/a+b.mp3"));
        assert_eq!(
            file_url_to_path("file://nas/share/Music/a.mp3").as_deref(),
            Some("//nas/share/Music/a.mp3")
        );
        assert_eq!(file_url_to_path("http://radio.example.com/stream"), None);
        assert_eq!(file_url_to_path("file://localhost"), None);
        assert_eq!(file_url_to_path(""), None);
    }

    fn sample_song() -> Json {
        serde_json::json!({
            "id": "i.PkdZbQXsXwm6P0",
            "type": "library-songs",
            "href": "/v1/me/library/songs/i.PkdZbQXsXwm6P0",
            "attributes": {
                "albumName": "Random Access Memories",
                "artistName": "Daft Punk",
                "artwork": {
                    "width": 1200,
                    "height": 1200,
                    "url": "https://is1-ssl.mzstatic.com/image/thumb/Music115/v4/e8/43/5f/e8435ffa/886443919266.jpg/{w}x{h}bb.jpg"
                },
                "dateAdded": "2021-03-04T05:06:07Z",
                "discNumber": 1,
                "durationInMillis": 369_626,
                "genreNames": ["Electronic"],
                "hasLyrics": true,
                "name": "Get Lucky (feat. Pharrell Williams & Nile Rodgers)",
                "playParams": {
                    "id": "i.PkdZbQXsXwm6P0",
                    "isLibrary": true,
                    "kind": "song",
                    "catalogId": "617154366",
                    "reporting": true
                },
                "releaseDate": "2013-04-19",
                "trackNumber": 8
            }
        })
    }

    #[test]
    fn maps_library_song_json() {
        let track = parse_library_song(&sample_song()).unwrap();
        assert_eq!(
            track,
            Track {
                album_artist: String::new(),
                disc_no: None,
                id: "applemusic:i.PkdZbQXsXwm6P0".into(),
                source: Source::AppleMusic,
                title: "Get Lucky (feat. Pharrell Williams & Nile Rodgers)".into(),
                artist: "Daft Punk".into(),
                album: "Random Access Memories".into(),
                duration_ms: 369_626,
                track_no: Some(8),
                art: Some(
                    "https://is1-ssl.mzstatic.com/image/thumb/Music115/v4/e8/43/5f/e8435ffa/886443919266.jpg/300x300bb.jpg"
                        .into()
                ),
                uri: String::new(),
                added_at: 1_614_834_367,
            }
        );

        // Minimal entry: missing optional fields fall back to defaults.
        let minimal = serde_json::json!({ "id": "i.abc", "type": "library-songs", "attributes": { "name": "X" } });
        let t = parse_library_song(&minimal).unwrap();
        assert_eq!(
            (t.artist.as_str(), t.duration_ms, t.track_no, t.art, t.added_at),
            ("", 0, None, None, 0)
        );

        // Music videos and malformed entries are rejected.
        let mut video = sample_song();
        video["type"] = "library-music-videos".into();
        assert!(parse_library_song(&video).is_none());
        assert!(parse_library_song(&serde_json::json!({ "id": "i.x" })).is_none());
        assert!(parse_library_song(&serde_json::json!({ "attributes": { "name": "x" } })).is_none());
        assert!(parse_library_song(&serde_json::json!({ "id": "i.x", "attributes": { "name": "" } })).is_none());
    }

    #[test]
    fn maps_library_playlist_json() {
        let v = serde_json::json!({
            "id": "p.MoGJYM3CYXW09B",
            "type": "library-playlists",
            "attributes": {
                "canEdit": true,
                "name": "Workout",
                "description": { "standard": "Fast stuff" },
                "artwork": { "url": "https://is1-ssl.mzstatic.com/image/thumb/Features/{w}x{h}{c}.{f}" },
                "playParams": { "id": "p.MoGJYM3CYXW09B", "kind": "playlist", "isLibrary": true }
            }
        });
        let p = parse_library_playlist(&v).unwrap();
        assert_eq!(p.remote_id, "p.MoGJYM3CYXW09B");
        assert_eq!(p.name, "Workout");
        assert_eq!(p.description, "Fast stuff");
        assert_eq!(
            p.art.as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/Features/300x300bb.jpg")
        );
        assert!(p.tracks.is_empty());
    }

    #[test]
    fn templates_artwork_urls() {
        assert_eq!(
            artwork_url("https://example.com/a/{w}x{h}bb.jpg", 300),
            "https://example.com/a/300x300bb.jpg"
        );
        assert_eq!(
            artwork_url("https://example.com/{w}x{h}{c}.{f}", 64),
            "https://example.com/64x64bb.jpg"
        );
        // Non-template URLs (user uploaded playlist art) are left alone.
        assert_eq!(
            artwork_url("https://example.com/fixed.jpg", 300),
            "https://example.com/fixed.jpg"
        );
    }

    #[test]
    fn parses_iso8601() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2021-03-04T05:06:07Z"), Some(1_614_834_367));
        assert_eq!(parse_iso8601("2021-03-04T05:06:07.123Z"), Some(1_614_834_367));
        assert_eq!(parse_iso8601("2021-03-04T06:06:07+01:00"), Some(1_614_834_367));
        assert_eq!(parse_iso8601("2021-03-04T00:06:07-0500"), Some(1_614_834_367));
        assert_eq!(parse_iso8601("2024-02-29"), Some(1_709_164_800));
        assert_eq!(parse_iso8601("garbage"), None);
        assert_eq!(parse_iso8601("2021-13-01"), None);
    }

    #[test]
    fn keeps_page_limit_on_next_links() {
        assert_eq!(
            next_page_url("/v1/me/library/songs?offset=100"),
            "/v1/me/library/songs?offset=100&limit=100"
        );
        assert_eq!(
            next_page_url("/v1/me/library/songs?limit=100&offset=200"),
            "/v1/me/library/songs?limit=100&offset=200"
        );
        assert_eq!(next_page_url("/v1/x"), "/v1/x?limit=100");
    }

    #[test]
    fn finds_bundles_and_jwt() {
        let html = r#"<html><head>
            <link rel="modulepreload" href="/assets/vendor~1a2b3c.js">
            <script type="module" crossorigin src="/assets/index~8f7e6d5c4b.js"></script>
            <script nomodule src="https://music.apple.com/assets/index-legacy-9a8b7c.js"></script>
            <script type="module" src="/assets/index~8f7e6d5c4b.js"></script>
        </head></html>"#;
        assert_eq!(
            find_bundle_urls(html),
            [
                "https://music.apple.com/assets/index~8f7e6d5c4b.js",
                "https://music.apple.com/assets/index-legacy-9a8b7c.js",
                "https://music.apple.com/assets/vendor~1a2b3c.js",
            ]
        );

        let jwt = "eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6IldlYlBsYXlLaWQifQ.eyJpc3MiOiJBTVBXZWJQbGF5IiwiaWF0IjoxNzAwMDAwMDAwfQ.c2lnbmF0dXJlX2hlcmU-_x";
        let js = format!(r#"const a="eyJx.not.it";const cfg={{token:"{jwt}",other:"eyJhbGciOiJub25lIn0.e30.sig2"}};"#);
        assert_eq!(find_jwt(&js).as_deref(), Some(jwt));
        assert_eq!(find_jwt("no token here"), None);
    }

    #[test]
    fn api_debug_hides_tokens() {
        let api = AppleMusicApi::new(reqwest::Client::new(), "Bearer dev-secret ", " user-secret", "");
        assert_eq!(api.developer_token, "dev-secret");
        assert_eq!(api.user_token, "user-secret");
        assert_eq!(api.storefront(), "us");
        let dbg = format!("{api:?}");
        assert!(!dbg.contains("secret"), "{dbg}");
    }

    #[test]
    fn catalog_helpers() {
        let v = serde_json::json!({"data": [{"id": "159260351", "type": "artists",
            "attributes": {"name": "Taylor Swift",
                "artwork": {"url": "https://is1-ssl.mzstatic.com/image/thumb/x/{w}x{h}bb.jpg"}}}]});
        let attrs = first_attributes(&v);
        assert_eq!(attr_text(attrs, "name"), "Taylor Swift");
        assert_eq!(attr_text(attrs, "missing"), "");
        assert_eq!(
            attrs.and_then(json_artwork).as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/x/300x300bb.jpg")
        );
        let song = serde_json::json!({"id": "1440935808", "type": "songs", "attributes": {
            "name": "Style", "artistName": "Taylor Swift", "albumName": "1989",
            "durationInMillis": 231000, "trackNumber": 3}});
        let t = parse_library_song(&song).unwrap();
        assert_eq!(t.id, "applemusic:1440935808");
        assert_eq!(t.duration_ms, 231_000);
    }
}
