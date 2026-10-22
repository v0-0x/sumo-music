//! Search, artist / album / playlist pages and the Last.fm profile.

use super::*;

impl Service {
    // ---------------------------------------------------------------- search, last.fm

    /// Searches Spotify and SoundCloud at the same time; each shows up as soon as it answers.
    pub(super) fn search(&mut self, query: String) {
        let q = query.trim().to_string();
        let spotify = (!q.is_empty())
            .then(|| self.web_auth().map(|auth| (auth, self.spotify_api.clone())))
            .flatten();
        let sc = (!q.is_empty() && self.cfg.soundcloud.enabled).then(|| self.soundcloud.clone());
        self.shared.feed.write().unwrap().search = SearchState {
            query: q.clone(),
            spotify_pending: spotify.is_some(),
            soundcloud_pending: sc.is_some(),
            ..Default::default()
        };
        self.shared.repaint();
        if let Some((auth, api)) = spotify {
            let tx = self.internal_tx.clone();
            let q = q.clone();
            tokio::spawn(async move {
                let search = async { api.search_all(&auth.token().await?, &q, 20).await };
                // Never leave the results spinning.
                let r = tokio::time::timeout(Duration::from_secs(15), search)
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("Spotify didn't answer in time")));
                let (tracks, hits) = match r {
                    Ok(mut hits) => (Ok(std::mem::take(&mut hits.tracks)), hits),
                    Err(e) => (Err(e), Default::default()),
                };
                let _ = tx.send(Internal::SearchResults {
                    query: q,
                    source: Source::Spotify,
                    tracks,
                    artists: hits.artists,
                    albums: hits.albums,
                    playlists: hits.playlists,
                });
            });
        }
        if let Some(sc) = sc {
            let tx = self.internal_tx.clone();
            tokio::spawn(async move {
                let (tracks, users, albums, playlists) = tokio::join!(
                    sc.search(&q, 20),
                    sc.search_users(&q, 8),
                    sc.search_sets(&q, true, 10),
                    sc.search_sets(&q, false, 10)
                );
                fn quietly<T>(what: &str, r: Result<Vec<T>>) -> Vec<T> {
                    r.unwrap_or_else(|e| {
                        tracing::warn!("SoundCloud {what} search: {e:#}");
                        Vec::new()
                    })
                }
                let artists = quietly("artist", users);
                let albums = quietly("album", albums);
                let playlists = quietly("playlist", playlists);
                let ok = tracks.is_ok();
                let _ = tx.send(Internal::SearchResults {
                    query: q,
                    source: Source::SoundCloud,
                    tracks,
                    artists,
                    albums,
                    playlists,
                });
                if ok {
                    if let Ok(id) = sc.client_id().await {
                        let _ = tx.send(Internal::SoundCloudClientId(id));
                    }
                }
            });
        }
    }

    /// Seeds SoundCloud's client_id from the last run and checks it in the background, so the
    /// first search doesn't have to scrape soundcloud.com first.
    pub(super) fn warm_up_soundcloud(&mut self) {
        let saved = self.db.get_kv("soundcloud_client_id").unwrap_or_default();
        let sc = self.soundcloud.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            sc.seed_client_id(&saved).await;
            if saved.is_empty() {
                match sc.client_id().await {
                    Ok(id) => {
                        let _ = tx.send(Internal::SoundCloudClientId(id));
                    }
                    Err(e) => tracing::debug!("SoundCloud warm-up: {e:#}"),
                }
            }
        });
    }

    // ---------------------------------------------------------------- pages

    pub(super) fn set_page(&self, page: PageState) {
        let mut feed = self.shared.feed.write().unwrap();
        if feed.page.key == page.key {
            feed.page = page;
        }
        drop(feed);
        self.shared.repaint();
    }

    pub(super) fn page_failed(&self, key: &str, error: impl Into<String>) {
        self.set_page(PageState {
            key: key.to_string(),
            error: Some(error.into()),
            ..Default::default()
        });
    }

    pub(super) fn open_page(&mut self, key: String) {
        let key = key.trim().to_string();
        if let Some(cached) = self.page_cache.iter().find(|p| p.key == key) {
            self.shared.feed.write().unwrap().page = cached.clone();
            self.shared.repaint();
            return;
        }
        self.shared.feed.write().unwrap().page = PageState {
            key: key.clone(),
            loading: true,
            ..Default::default()
        };
        self.shared.repaint();
        match links::target(&key) {
            Some(target) => self.load_page(key, target),
            None => self.page_failed(&key, "That isn't a Spotify, SoundCloud or Apple Music link"),
        }
    }

    pub(super) fn load_page(&mut self, key: String, target: Target) {
        let tx = self.internal_tx.clone();
        let send = move |key: String, result: Result<PageState>| {
            let _ = tx.send(Internal::PageLoaded { key, result });
        };
        match target {
            // The UI draws these straight from the library and never asks the service.
            Target::LocalArtist(_) => self.page_failed(&key, "Open library artists from Your Library"),
            Target::ArtistName(name) => {
                let soundcloud = self.cfg.soundcloud.enabled.then(|| self.soundcloud.clone());
                let spotify = self.spotify_lookup();
                tokio::spawn(async move {
                    let (on_spotify, on_soundcloud) = tokio::join!(
                        async {
                            match &spotify {
                                Some(l) => spotify_artist(l, &name).await,
                                None => None,
                            }
                        },
                        async {
                            match &soundcloud {
                                Some(sc) => soundcloud_artist(sc, &name).await,
                                None => None,
                            }
                        },
                    );
                    let result = if on_spotify.is_none() && on_soundcloud.is_none() {
                        Err(anyhow!("{name} isn't on Spotify or SoundCloud under that name"))
                    } else {
                        let mut page = PageState {
                            kind: "Artist".into(),
                            title: name,
                            round: true,
                            ..Default::default()
                        };
                        for found in [on_spotify, on_soundcloud].into_iter().flatten() {
                            page.image = page.image.or(found.image);
                            page.external_url = page.external_url.or(Some(found.url));
                            page.source = page.source.or(Some(found.source));
                            add_songs(&mut page.tracks, found.tracks);
                        }
                        page.subtitle = artist_subtitle(&page.tracks);
                        Ok(page)
                    };
                    send(key, result);
                });
            }
            Target::Spotify(kind, id) => {
                if !self.cfg.spotify.enabled || !self.spotify_auth.has_login() {
                    self.page_failed(&key, "Log in to Spotify in Settings to open Spotify pages");
                    return;
                }
                let Some(session) = self.spotify_session() else {
                    self.spotify_pending_page = Some(key);
                    return;
                };
                // Artist pages also get the artist's SoundCloud uploads.
                let soundcloud =
                    (kind == LinkKind::Artist && self.cfg.soundcloud.enabled).then(|| self.soundcloud.clone());
                tokio::spawn(async move {
                    let result = tokio::time::timeout(Duration::from_secs(45), spotify_page(session, kind, &id))
                        .await
                        .unwrap_or_else(|_| Err(anyhow!("Spotify took too long to answer")));
                    match (result, soundcloud) {
                        (Ok(mut page), Some(sc)) => {
                            page.tracks = dedupe_songs(page.tracks);
                            page.merging = Some(Source::SoundCloud);
                            send(key.clone(), Ok(page.clone()));
                            if let Some(found) = soundcloud_artist(&sc, &page.title).await {
                                if add_songs(&mut page.tracks, found.tracks) > 0 {
                                    page.subtitle = artist_subtitle(&page.tracks);
                                }
                            }
                            page.merging = None;
                            send(key, Ok(page));
                        }
                        (result, _) => send(key, result),
                    }
                });
            }
            Target::SoundCloudUser(id) => {
                let sc = self.soundcloud.clone();
                let spotify = self.spotify_lookup();
                tokio::spawn(async move {
                    let result = async {
                        let (user, tracks) = tokio::join!(sc.user(id), sc.user_tracks(id));
                        Ok(soundcloud_user_page(&user?, tracks?))
                    }
                    .await;
                    with_spotify_songs(result, spotify, |page| send(key.clone(), page)).await;
                });
            }
            Target::SoundCloudUrl(url) => {
                let sc = self.soundcloud.clone();
                let spotify = self.spotify_lookup();
                tokio::spawn(async move {
                    let result = async {
                        Ok(match sc.resolve_url(&url).await? {
                            ScResolved::User(user) => {
                                let tracks = sc.user_tracks(user.id).await?;
                                soundcloud_user_page(&user, tracks)
                            }
                            ScResolved::Track(t) => PageState {
                                kind: "Song".into(),
                                source: Some(Source::SoundCloud),
                                title: t.title.clone(),
                                subtitle: t.artist.clone(),
                                image: t.art.clone(),
                                external_url: Some(url),
                                tracks: vec![t],
                                ..Default::default()
                            },
                            ScResolved::Playlist { playlist: p, album, by } => PageState {
                                kind: if album { "Album" } else { "Playlist" }.into(),
                                source: Some(Source::SoundCloud),
                                subtitle: match (album, by.is_empty()) {
                                    (_, true) => songs_text(p.tracks.len()),
                                    (true, false) => format!("Album · {by} · {}", songs_text(p.tracks.len())),
                                    (false, false) => format!("by {by} · {}", songs_text(p.tracks.len())),
                                },
                                artist: if album { by } else { String::new() },
                                title: p.name,
                                image: p.art,
                                external_url: Some(url),
                                tracks: p.tracks,
                                ..Default::default()
                            },
                        })
                    }
                    .await;
                    with_spotify_songs(result, spotify, |page| send(key.clone(), page)).await;
                });
            }
            Target::AppleMusic { kind, storefront, id } => {
                let http = self.http.clone();
                let configured = self.cfg.apple_music.developer_token.trim().to_string();
                let cache = self.apple_dev_token.clone();
                tokio::spawn(async move {
                    let result = async {
                        let token = {
                            let mut cached = cache.lock().await;
                            match (configured.is_empty(), cached.clone()) {
                                (false, _) => configured,
                                (true, Some(t)) => t,
                                (true, None) => {
                                    let t = apple_music::scrape_developer_token(&http).await?;
                                    *cached = Some(t.clone());
                                    t
                                }
                            }
                        };
                        let api = AppleMusicApi::new(http, &token, "", &storefront);
                        let page = match kind {
                            LinkKind::Artist => api.catalog_artist(&id).await,
                            LinkKind::Album => api.catalog_album(&id).await,
                            LinkKind::Playlist => api.catalog_playlist(&id).await,
                            LinkKind::Track => api.catalog_song(&id).await,
                        };
                        if page.as_ref().is_err_and(apple_music::is_auth_error) {
                            // A scraped token may have expired: scrape a new one next time.
                            *cache.lock().await = None;
                        }
                        let page = page?;
                        Ok(PageState {
                            kind: links::kind_label(kind).into(),
                            source: Some(Source::AppleMusic),
                            title: page.title,
                            subtitle: page.subtitle,
                            image: page.image,
                            round: kind == LinkKind::Artist,
                            tracks: page.tracks,
                            external_url: links::web_url(&format!(
                                "applemusic:{}:{storefront}:{id}",
                                links::kind_name(kind)
                            )),
                            ..Default::default()
                        })
                    }
                    .await;
                    send(key, result);
                });
            }
            Target::Short(url) => {
                let http = self.http.clone();
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let result = expand_short_link(&http, &url).await;
                    let _ = tx.send(Internal::ShortLink { key, result });
                });
            }
        }
    }
}

/// Why the profile page couldn't load something, in a few words.
pub(super) fn friendly_lastfm_error(e: &anyhow::Error) -> String {
    let offline = e.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|r| r.is_connect() || r.is_timeout())
    });
    if offline {
        return "Can't reach Last.fm right now".into();
    }
    match e.downcast_ref::<crate::integrations::lastfm::LastfmError>() {
        Some(crate::integrations::lastfm::LastfmError::Api { code: 29, .. }) => {
            "Last.fm asked to slow down; trying again in a minute".into()
        }
        Some(crate::integrations::lastfm::LastfmError::Api { code: 10 | 26, .. }) => {
            "Last.fm doesn't accept the API key in Settings".into()
        }
        Some(crate::integrations::lastfm::LastfmError::Api { code: 17, .. }) => {
            "This profile's listening history is private on Last.fm".into()
        }
        Some(crate::integrations::lastfm::LastfmError::Api { message, .. }) => message.clone(),
        _ => format!("{e:#}"),
    }
}

/// `a` and `b` taking turns (what's left of the longer one at the end).
pub(super) fn interleave<T>(a: Vec<T>, b: Vec<T>) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut a, mut b) = (a.into_iter(), b.into_iter());
    loop {
        match (a.next(), b.next()) {
            (None, None) => return out,
            (x, y) => out.extend(x.into_iter().chain(y)),
        }
    }
}

pub(super) fn rank_artists(artists: &mut [ArtistHit], query: &str) {
    let q = query.trim().to_lowercase();
    artists.sort_by_cached_key(|a| {
        let name = a.name.to_lowercase();
        let rank = if name == q {
            0
        } else if name.starts_with(&q) {
            1
        } else if name.contains(&q) {
            2
        } else {
            3
        };
        (rank, a.source != Source::Spotify)
    });
}

pub(super) async fn spotify_page(
    session: librespot_core::session::Session,
    kind: LinkKind,
    id: &str,
) -> Result<PageState> {
    use crate::providers::spotify_internal as si;
    let data = match kind {
        LinkKind::Artist => si::artist_page(&session, id).await?,
        LinkKind::Album => si::album_page(&session, id).await?,
        LinkKind::Playlist => si::playlist_page(&session, id).await?,
        LinkKind::Track => si::track_page(&session, id).await?,
    };
    Ok(PageState {
        kind: links::kind_label(kind).into(),
        source: Some(Source::Spotify),
        title: data.title,
        subtitle: data.subtitle,
        artist: data.artist,
        image: data.image,
        round: kind == LinkKind::Artist,
        tracks: data.tracks,
        external_url: links::web_url(&format!("spotify:{}:{id}", links::kind_name(kind))),
        ..Default::default()
    })
}

/// The songs a pasted line stands for: a Spotify or SoundCloud song, album or playlist link, or
/// a music file (a path or a `file://` link from a file manager). `None` when it can't be found.
pub(super) async fn songs_behind_link(
    line: &str,
    session: Option<&librespot_core::session::Session>,
    soundcloud: &SoundCloud,
    http: &reqwest::Client,
    shared: &Shared,
) -> Option<Vec<Track>> {
    let line = line.trim();
    if let Some(path) = local_path(line) {
        if !library::scanner::is_audio_file(&path) || !path.is_file() {
            return None;
        }
        let id = Track::local_id(&path.to_string_lossy());
        if let Some(known) = shared.library.read().unwrap().get(&id).cloned() {
            return Some(vec![known]);
        }
        let read = tokio::task::spawn_blocking(move || library::scanner::read_track(&path, None, now_unix()));
        return read.await.ok().map(|t| vec![t]);
    }
    let mut target = links::target(line)?;
    if let Target::Short(url) = &target {
        target = links::target(&expand_short_link(http, url).await.ok()?)?;
    }
    match target {
        Target::Spotify(kind @ (LinkKind::Track | LinkKind::Album | LinkKind::Playlist), id) => {
            let page = spotify_page(session?.clone(), kind, &id);
            let page = tokio::time::timeout(Duration::from_secs(45), page).await.ok()?.ok()?;
            Some(page.tracks)
        }
        Target::SoundCloudUrl(url) => match soundcloud.resolve_url(&url).await.ok()? {
            ScResolved::Track(t) => Some(vec![t]),
            ScResolved::Playlist { playlist, .. } => Some(playlist.tracks),
            ScResolved::User(_) => None,
        },
        _ => None,
    }
}

/// What Last.fm gets for a song. A SoundCloud upload that is also released on Spotify is sent
/// with Spotify's artist, title and album: those are the names Last.fm knows, and the album is
/// where Last.fm and apps like .fmbot take the cover from. So is a local file without tags,
/// whose names were only guessed from the file name. Remembered per song.
pub(super) async fn lastfm_track(spotify: Option<(Arc<SpotifyAuth>, Arc<SpotifyApi>)>, track: Track) -> Track {
    static RELEASES: OnceLock<std::sync::Mutex<HashMap<String, Option<Track>>>> = OnceLock::new();
    let mut track = track;
    match track.source {
        // Every SoundCloud song: even uploads with an album often credit "A x B" or a label,
        // where Spotify has the names Last.fm knows.
        Source::SoundCloud => {}
        Source::Local => {
            let path = PathBuf::from(&track.uri);
            let missing = tokio::task::spawn_blocking(move || library::scanner::untagged(&path)).await;
            let Ok(missing) = missing else { return track };
            if missing.album {
                // The folder's name is only a guess: Spotify's album or Last.fm's is better.
                track.album.clear();
            }
            if !missing.names {
                return track;
            }
        }
        _ => return track,
    }
    let Some((auth, api)) = spotify else { return track };
    let cache = RELEASES.get_or_init(Default::default);
    let known = cache.lock().unwrap().get(&track.id).cloned();
    let release = match known {
        Some(release) => release,
        None => {
            let (artist, title) = crate::integrations::lyrics::song_names(&track);
            let unknown = artist == library::scanner::UNKNOWN_ARTIST;
            let wanted = Track {
                artist: if unknown { String::new() } else { artist.clone() },
                title: title.clone(),
                ..track.clone()
            };
            let query = if unknown {
                title.clone()
            } else {
                format!("{artist} {title}")
            };
            let found = match auth.token().await {
                Ok(token) => api.search(&token, &query, 10).await.ok(),
                Err(_) => None,
            };
            let Some(results) = found else {
                // Couldn't ask: try again next time.
                return track;
            };
            let release = if unknown {
                same_title_and_length(&wanted, &results)
            } else {
                best_match(&wanted, &results)
            };
            let release = release.filter(|t| !t.album.trim().is_empty());
            let mut cache = cache.lock().unwrap();
            if cache.len() > 2000 {
                cache.clear();
            }
            cache.insert(track.id.clone(), release.clone());
            release
        }
    };
    match release {
        Some(release) => {
            tracing::debug!(
                "lastfm: {} - {} is {} - {} ({}) on Spotify",
                track.artist,
                track.title,
                release.artist,
                release.title,
                release.album
            );
            // Still a SoundCloud (or local) song to Last.fm: a SoundCloud song's album may be
            // swapped for one with a cover.
            Track {
                source: track.source,
                ..release
            }
        }
        None => track,
    }
}

/// For a song with no known artist: the candidate with the same title and nearly the same
/// length.
pub(super) fn same_title_and_length(target: &Track, candidates: &[Track]) -> Option<Track> {
    let title = normalize_title(&target.title);
    if title.is_empty() || target.duration_ms == 0 {
        return None;
    }
    candidates
        .iter()
        .filter(|c| normalize_title(&c.title) == title && c.duration_ms > 0)
        .map(|c| (c, (target.duration_ms as i64 - c.duration_ms as i64).unsigned_abs()))
        .filter(|(_, diff)| *diff <= 3_000)
        .min_by_key(|(_, diff)| *diff)
        .map(|(c, _)| c.clone())
}

/// Spotify access for finding an artist there by name.
pub(super) struct SpotifyLookup {
    pub(super) auth: Arc<SpotifyAuth>,
    pub(super) api: Arc<SpotifyApi>,
    pub(super) session: librespot_core::session::Session,
}

/// An artist found on one service by name.
pub(super) struct ArtistFound {
    pub(super) source: Source,
    pub(super) image: Option<String>,
    pub(super) url: String,
    pub(super) tracks: Vec<Track>,
}

/// Same artist name, ignoring case, spaces and punctuation ("Yung Lean" = "yunglean").
pub(super) fn same_artist_name(a: &str, b: &str) -> bool {
    let squash = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    };
    let a = squash(a);
    !a.is_empty() && a == squash(b)
}

/// The artist's SoundCloud profile (exact name only, so nobody else's) and their uploads.
pub(super) async fn soundcloud_artist(sc: &SoundCloud, name: &str) -> Option<ArtistFound> {
    let users = sc.search_users(name, 8).await.ok()?;
    let hit = users.into_iter().find(|u| same_artist_name(&u.name, name))?;
    let id: u64 = hit.key.strip_prefix("soundcloud:user:")?.parse().ok()?;
    let tracks = sc.user_tracks(id).await.ok()?;
    Some(ArtistFound {
        source: Source::SoundCloud,
        url: links::web_url(&hit.key).unwrap_or_default(),
        image: hit.image,
        tracks,
    })
}

/// The artist on Spotify (exact name only) and their popular songs and latest releases.
pub(super) async fn spotify_artist(l: &SpotifyLookup, name: &str) -> Option<ArtistFound> {
    let token = l.auth.token().await.ok()?;
    let (_, artists) = l.api.search_with_artists(&token, name, 5).await.ok()?;
    let hit = artists
        .into_iter()
        .find(|a| a.source == Source::Spotify && same_artist_name(&a.name, name))?;
    let id = hit.key.strip_prefix("spotify:artist:")?.to_string();
    let page = crate::providers::spotify_internal::artist_page(&l.session, &id)
        .await
        .ok()?;
    Some(ArtistFound {
        source: Source::Spotify,
        url: format!("https://open.spotify.com/artist/{id}"),
        image: page.image.or(hit.image),
        tracks: dedupe_songs(page.tracks),
    })
}

/// Sends a SoundCloud profile page, then again with the artist's Spotify songs added.
pub(super) async fn with_spotify_songs(
    result: Result<PageState>,
    spotify: Option<SpotifyLookup>,
    send: impl Fn(Result<PageState>),
) {
    let (Ok(mut page), Some(spotify)) = (result.as_ref().map(Clone::clone), spotify) else {
        send(result);
        return;
    };
    if page.kind != "Artist" {
        send(Ok(page));
        return;
    }
    page.tracks = dedupe_songs(page.tracks);
    page.merging = Some(Source::Spotify);
    send(Ok(page.clone()));
    if let Some(found) = spotify_artist(&spotify, &page.title).await {
        if add_songs(&mut page.tracks, found.tracks) > 0 {
            page.subtitle = artist_subtitle(&page.tracks);
        }
    }
    page.merging = None;
    send(Ok(page));
}

/// "Artist · 42 songs on Spotify and SoundCloud".
pub(super) fn artist_subtitle(tracks: &[Track]) -> String {
    let on = |s: Source| tracks.iter().filter(|t| t.source == s).count();
    let (spotify, soundcloud) = (on(Source::Spotify), on(Source::SoundCloud));
    let total = tracks.len();
    match (spotify > 0, soundcloud > 0) {
        (true, true) => format!("Artist · {total} songs on Spotify and SoundCloud"),
        (true, false) => format!("Artist · {total} songs on Spotify"),
        (false, true) => format!("Artist · {total} songs on SoundCloud"),
        (false, false) => format!("Artist · {total} songs"),
    }
}

/// What makes two songs the same: the title as people know it ("Artist - Title [Free DL]"
/// uploads cleaned up, "(feat. …)" and "- Remastered" dropped; "(Live)", "(Remix)" kept).
pub(super) fn song_key(t: &Track) -> String {
    normalize_title(&crate::integrations::lastfm::scrobble_names(t).1)
}

/// Whether `a` and `b` are the same song: same title and about the same length (a little
/// looser across services, which encode differently).
#[cfg(test)]
pub(super) fn same_song(a: &Track, b: &Track) -> bool {
    same_song_keyed(&song_key(a), a, &song_key(b), b)
}

pub(super) fn same_song_keyed(ka: &str, a: &Track, kb: &str, b: &Track) -> bool {
    if ka.is_empty() || ka != kb {
        return false;
    }
    let tolerance = if a.source == b.source { 4_000 } else { 12_000 };
    a.duration_ms == 0 || b.duration_ms == 0 || a.duration_ms.abs_diff(b.duration_ms) <= tolerance
}

pub(super) fn soundcloud_user_page(user: &ScUser, tracks: Vec<Track>) -> PageState {
    let hit = soundcloud::artist_hit(user);
    PageState {
        kind: "Artist".into(),
        source: Some(Source::SoundCloud),
        title: user.username.clone(),
        subtitle: format!("{} · {} tracks", hit.subtitle, tracks.len()),
        image: user.avatar.clone(),
        round: true,
        tracks,
        external_url: (!user.permalink_url.is_empty()).then(|| user.permalink_url.clone()),
        ..Default::default()
    }
}

/// Follows a short link (spotify.link, on.soundcloud.com) to the page it stands for.
pub(super) async fn expand_short_link(http: &reqwest::Client, url: &str) -> Result<String> {
    let resp = http.get(url).send().await?.error_for_status()?;
    let landed = resp.url().to_string();
    if links::parse(&landed).is_some_and(|l| !matches!(l, links::Link::Short { .. })) {
        return Ok(landed);
    }
    // Some short links land on an HTML page that links (or script-redirects) to the target.
    let body = resp.text().await.unwrap_or_default();
    links::find_link(&body)
        .and_then(|link| links::web_url(&links::page_key(&link)))
        .ok_or_else(|| anyhow!("{url} didn't lead to a music page"))
}

pub(super) fn friendly_page_error(e: &anyhow::Error) -> String {
    if apple_music::is_auth_error(e) {
        "Apple Music refused the request. Try again, or paste a developer token in Settings → Apple Music.".into()
    } else {
        friendly_net_error(e)
    }
}
