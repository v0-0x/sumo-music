//! The library: local scans, and syncing playlists and likes from Spotify, SoundCloud and Apple Music.

use super::*;

impl Service {
    // ---------------------------------------------------------------- library

    pub(super) fn store_tracks(&mut self, tracks: &[Track]) {
        let new: Vec<Track> = {
            let lib = self.shared.library.read().unwrap();
            tracks
                .iter()
                .filter(|t| !lib.tracks.contains_key(&t.id))
                .cloned()
                .collect()
        };
        if new.is_empty() {
            return;
        }
        if let Err(e) = self.db.upsert_tracks(&new, &HashMap::new()) {
            self.shared.error(format!("Database error: {e:#}"));
        }
        let mut lib = self.shared.library.write().unwrap();
        for t in new {
            lib.tracks.insert(t.id.clone(), t);
        }
    }

    /// Saves/replaces playlists, deletes `remove`, then reindexes.
    pub(super) fn save_playlists(&mut self, playlists: Vec<Playlist>, remove: &[String]) {
        // The library is only locked while it changes; the UI waits on that lock, so the
        // database (only the playlists that changed) is written after.
        let (changed, order) = {
            let mut lib = self.shared.library.write().unwrap();
            let ids: HashSet<String> = playlists.iter().map(|p| p.id.clone()).collect();
            for p in playlists {
                match lib.playlists.iter_mut().find(|x| x.id == p.id) {
                    Some(existing) => *existing = p,
                    None => lib.playlists.push(p),
                }
            }
            lib.playlists.retain(|p| !remove.contains(&p.id));
            lib.reindex();
            let changed: Vec<(Playlist, i64)> = lib
                .playlists
                .iter()
                .enumerate()
                .filter(|(_, p)| ids.contains(&p.id))
                .map(|(i, p)| (p.clone(), i as i64))
                .collect();
            let order: Vec<String> = lib.playlists.iter().map(|p| p.id.clone()).collect();
            (changed, order)
        };
        self.shared.repaint();
        for (p, position) in &changed {
            if let Err(e) = self.db.save_playlist(p, *position) {
                tracing::error!("saving playlist {}: {e:#}", p.id);
            }
        }
        for id in remove {
            let _ = self.db.delete_playlist(id);
        }
        // Cheap (one number per playlist) and keeps the order right after adds and removals.
        if let Err(e) = self.db.set_playlist_order(&order) {
            tracing::error!("saving the playlist order: {e:#}");
        }
    }

    /// Saves the album on the open page to the library's albums (and to Spotify's library when
    /// signed in, like liked songs).
    pub(super) fn save_album(&mut self, key: &str) {
        let page = self.shared.feed.read().unwrap().page.clone();
        if page.key != key || page.kind != "Album" || page.loading || page.tracks.is_empty() {
            return;
        }
        let (kind, remote) = match (page.source, links::target(key)) {
            (Some(Source::Spotify), Some(links::Target::Spotify(LinkKind::Album, id))) => {
                (PlaylistKind::SpotifyAlbum, id)
            }
            (Some(Source::SoundCloud), _) => (PlaylistKind::SoundCloudAlbum, key.to_string()),
            _ => return,
        };
        let artist = if page.artist.is_empty() {
            page.tracks.first().map(|t| t.artist.clone()).unwrap_or_default()
        } else {
            page.artist.clone()
        };
        // SoundCloud songs often come without an album: they are on this one.
        let tracks: Vec<Track> = page
            .tracks
            .iter()
            .cloned()
            .map(|mut t| {
                if t.album.trim().is_empty() {
                    t.album = page.title.clone();
                }
                t
            })
            .collect();
        self.store_tracks(&tracks);
        self.save_playlists(
            vec![Playlist {
                id: saved_album_id(kind, &remote),
                name: page.title.clone(),
                kind,
                remote_id: Some(remote.clone()),
                description: artist,
                art: page.image.clone(),
                track_ids: page.tracks.iter().map(|t| t.id.clone()).collect(),
            }],
            &[],
        );
        self.shared.info(format!("Saved “{}” to your albums", page.title));
        if kind == PlaylistKind::SpotifyAlbum {
            self.spotify_album_saved(remote, true);
        }
    }

    /// Keeps Spotify's library in step with an album saved or removed here.
    pub(super) fn spotify_album_saved(&self, album_id: String, saved: bool) {
        let Some(auth) = self.user_web_auth() else { return };
        let api = self.spotify_api.clone();
        tokio::spawn(async move {
            if let Ok(token) = auth.token().await {
                if let Err(e) = api.set_album_saved(&token, &album_id, saved).await {
                    tracing::warn!("Spotify album sync failed: {e:#}");
                }
            }
        });
    }

    pub(super) fn toggle_like(&mut self, track: Track) {
        let liked = {
            let lib = self.shared.library.read().unwrap();
            lib.playlist(LIKED_ID).is_some_and(|p| p.track_ids.contains(&track.id))
        };
        self.set_liked(vec![track], !liked);
    }

    /// Saves songs to Liked Songs (or removes them), on Spotify and Last.fm too.
    pub(super) fn set_liked(&mut self, tracks: Vec<Track>, liked: bool) {
        self.store_tracks(&tracks);
        let (changed, playlist) = {
            let mut lib = self.shared.library.write().unwrap();
            if lib.playlist(LIKED_ID).is_none() {
                lib.playlists.insert(0, liked_playlist());
            }
            let p = lib.playlist_mut(LIKED_ID).expect("liked playlist");
            let mut changed: Vec<Track> = Vec::new();
            for t in tracks {
                let at = p.track_ids.iter().position(|id| id == &t.id);
                let new = !changed.iter().any(|c| c.id == t.id);
                match at {
                    Some(i) if !liked => {
                        p.track_ids.remove(i);
                        changed.push(t);
                    }
                    None if liked && new => changed.push(t),
                    _ => {}
                }
            }
            if liked {
                // The newest likes come first, in the order they were picked.
                p.track_ids.splice(0..0, changed.iter().map(|t| t.id.clone()));
            }
            (changed, p.clone())
        };
        if changed.is_empty() {
            return;
        }
        self.save_playlists(vec![playlist], &[]);
        if changed.len() > 1 {
            let n = songs_text(changed.len());
            self.shared.info(if liked {
                format!("Saved {n} to Liked Songs")
            } else {
                format!("Removed {n} from Liked Songs")
            });
        }

        let spotify: Vec<Track> = changed
            .iter()
            .filter(|t| t.source == Source::Spotify)
            .cloned()
            .collect();
        if let (false, Some(auth)) = (spotify.is_empty(), self.user_web_auth()) {
            let api = self.spotify_api.clone();
            let shared = self.shared.clone();
            tokio::spawn(async move {
                let mut failed = Vec::new();
                for t in &spotify {
                    let result = match auth.token().await {
                        Ok(token) => api.set_liked(&token, &t.uri, liked).await,
                        Err(e) => Err(e),
                    };
                    if let Err(e) = result {
                        tracing::warn!("Spotify like sync failed: {e:#}");
                        failed.push((t.title.clone(), e));
                    }
                }
                let verb = if liked { "like" } else { "unlike" };
                match failed.as_slice() {
                    [] => {}
                    [(title, e)] => shared.error(format!("Couldn't {verb} “{title}” on Spotify: {e:#}")),
                    [(_, e), ..] => shared.error(format!(
                        "Couldn't {verb} {} on Spotify: {e:#}",
                        songs_text(failed.len())
                    )),
                }
            });
        }
        // Loved on Last.fm too when logged in there (songs without an artist can't be).
        let lastfm: Vec<Track> = changed.into_iter().filter(|t| !t.artist.trim().is_empty()).collect();
        if let Some(lfm) = self
            .lastfm
            .clone()
            .filter(|l| l.is_authenticated() && !lastfm.is_empty())
        {
            let shared = self.shared.clone();
            tokio::spawn(async move {
                let mut failed = Vec::new();
                for t in &lastfm {
                    if let Err(e) = lfm.love(t, liked).await {
                        tracing::warn!("Last.fm love failed: {e:#}");
                        failed.push(t.title.clone());
                    }
                }
                let verb = if liked { "love" } else { "unlove" };
                match failed.as_slice() {
                    [] => {}
                    [title] => shared.error(format!("Couldn't {verb} “{title}” on Last.fm")),
                    _ => shared.error(format!("Couldn't {verb} {} on Last.fm", songs_text(failed.len()))),
                }
            });
        }
    }

    pub(super) fn start_scan(&mut self) {
        let folders = self.cfg.library.folders.clone();
        if folders.is_empty() {
            return;
        }
        let mut known = self.db.local_mtimes().unwrap_or_default();
        // Files read by an older version are read again once (it now guesses more from names).
        if self.db.get_kv("scan_version").as_deref() != Some(SCAN_VERSION) {
            known.values_mut().for_each(|mtime| *mtime = i64::MIN);
        }
        let shared = self.shared.clone();
        let tx = self.internal_tx.clone();
        shared.feed.write().unwrap().scan = Some((0, 0));
        tokio::task::spawn_blocking(move || {
            let progress = |done: usize, total: usize| {
                shared.feed.write().unwrap().scan = Some((done, total));
                shared.repaint();
            };
            let result = library::scanner::scan(&folders, &known, &progress);
            let _ = tx.send(Internal::ScanDone(result));
        });
    }

    pub(super) fn merge_scan(&mut self, result: library::scanner::ScanResult) {
        let changed = result.changed.len();
        if let Err(e) = self.db.upsert_tracks(&result.changed, &result.mtimes) {
            self.shared.error(format!("Database error: {e:#}"));
        }
        let _ = self.db.delete_tracks(&result.removed);
        let _ = self.db.set_kv("scan_version", SCAN_VERSION);
        {
            let mut lib = self.shared.library.write().unwrap();
            for id in &result.removed {
                lib.tracks.remove(id);
            }
            for mut t in result.changed {
                // As in the database: a file keeps the date it was first added.
                if let Some(old) = lib.tracks.get(&t.id).filter(|old| old.added_at != 0) {
                    t.added_at = old.added_at;
                }
                lib.tracks.insert(t.id.clone(), t);
            }
            lib.reindex();
        }
        self.shared.feed.write().unwrap().scan = None;
        // A scan reads every changed file's tags: hand that memory back.
        crate::memory::trim();
        if changed > 0 || !result.removed.is_empty() {
            self.shared.info(format!(
                "Library updated: {} files, {changed} new or changed, {} removed",
                result.total_files,
                result.removed.len()
            ));
        }
        self.shared.repaint();
    }

    /// Merges playlists imported from a service. Playlists of `kind` that aren't in `keep_ids`
    /// are removed when `exclusive` is set (i.e. deleted on the service).
    pub(super) fn merge_imported(
        &mut self,
        tracks: Vec<Track>,
        playlists: Vec<Playlist>,
        prune_kinds: &[PlaylistKind],
    ) {
        if let Err(e) = self.db.upsert_tracks(&tracks, &HashMap::new()) {
            self.shared.error(format!("Database error: {e:#}"));
            return;
        }
        {
            let mut lib = self.shared.library.write().unwrap();
            for t in tracks {
                lib.tracks.insert(t.id.clone(), t);
            }
        }
        let remove = {
            let lib = self.shared.library.read().unwrap();
            removed_playlists(&lib.playlists, &playlists, prune_kinds)
        };
        self.save_playlists(playlists, &remove);
        if !remove.is_empty() {
            if let Ok(n) = self.db.prune_orphans() {
                if n > 0 {
                    let mut lib = self.shared.library.write().unwrap();
                    let referenced: HashSet<String> =
                        lib.playlists.iter().flat_map(|p| p.track_ids.iter().cloned()).collect();
                    lib.tracks
                        .retain(|id, t| t.source == Source::Local || referenced.contains(id));
                    lib.reindex();
                }
            }
        }
        // A sync parses thousands of songs' details: hand that memory back.
        crate::memory::trim();
    }

    // ---------------------------------------------------------------- spotify

    pub(super) fn spotify_login(&mut self) {
        let auth = self.spotify_auth.clone();
        let tx = self.cmd_tx.clone();
        let shared = self.shared.clone();
        self.set_account(
            |f| &mut f.spotify,
            AccountStatus::Working("Waiting for browser login…".into()),
        );
        tokio::spawn(async move {
            match auth.login().await {
                Ok(_) => {
                    let _ = tx.send(Command::SyncSpotify);
                }
                Err(e) => {
                    shared.feed.write().unwrap().spotify = AccountStatus::Error(format!("{e:#}"));
                    shared.repaint();
                }
            }
        });
    }

    pub(super) fn connect_spotify(&mut self) {
        if self.spotify.is_some() || self.spotify_connecting {
            return;
        }
        self.spotify_connecting = true;
        let auth = self.spotify_auth.clone();
        let dir = self.paths.spotify_dir();
        let cache = self.spotify_audio_cache();
        let cfg = self.cfg.spotify.clone();
        let vol = self.cfg.playback.volume;
        let ev = self.spotify_tx.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let r = SpotifyEngine::connect(&auth, &dir, cache, &cfg, vol, ev).await;
            let _ = tx.send(Internal::SpotifyReady(r));
        });
    }

    pub(super) fn start_spotify_sync(&mut self) {
        if !self.spotify_auth.has_login() || self.spotify_syncing {
            return;
        }
        // The library is imported through the playback session, so connect first.
        let Some(session) = self.spotify_session() else {
            self.spotify_sync_pending = true;
            self.set_account(|f| &mut f.spotify, AccountStatus::Working("Connecting…".into()));
            return;
        };
        self.spotify_syncing = true;
        self.set_account(|f| &mut f.spotify, AccountStatus::Working("Syncing playlists…".into()));
        let (known, known_albums): (HashSet<String>, HashSet<String>) = {
            let lib = self.shared.library.read().unwrap();
            (
                lib.tracks
                    .values()
                    .filter(|t| t.source == Source::Spotify)
                    .map(|t| t.id.clone())
                    .collect(),
                lib.playlists
                    .iter()
                    .filter(|p| p.kind == PlaylistKind::SpotifyAlbum)
                    .filter_map(|p| p.remote_id.clone())
                    .collect(),
            )
        };
        let web = self.user_web_auth().map(|a| (a, self.spotify_api.clone()));
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let progress_tx = tx.clone();
            let progress = move |text: String| {
                let _ = progress_tx.send(Internal::SyncProgress(text));
            };
            let r = match spotify_sync_internal(session, known, known_albums, progress).await {
                Ok(sync) => Ok(sync),
                Err(e) => {
                    tracing::warn!("Spotify library import via session failed: {e:#}; trying the Web API");
                    match web {
                        Some((auth, api)) => spotify_sync_web(&auth, &api, &HashMap::new())
                            .await
                            .map_err(|e2| anyhow!("{e:#} (Web API: {})", friendly_spotify_error(&e2))),
                        None => Err(e),
                    }
                }
            };
            let _ = tx.send(Internal::SpotifySynced(r));
        });
    }

    /// Fallback when no playback session can be opened (e.g. connection refused).
    pub(super) fn start_spotify_web_sync(&mut self) {
        let Some(auth) = self.user_web_auth() else { return };
        if self.spotify_syncing {
            return;
        }
        self.spotify_syncing = true;
        self.set_account(|f| &mut f.spotify, AccountStatus::Working("Syncing playlists…".into()));
        let api = self.spotify_api.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let r = spotify_sync_web(&auth, &api, &HashMap::new())
                .await
                .map_err(|e| anyhow!("{}", friendly_spotify_error(&e)));
            let _ = tx.send(Internal::SpotifySynced(r));
        });
    }

    /// With a Client secret, fetches an app token right away so Settings can say whether the
    /// ID and secret work.
    pub(super) fn check_app_secret(&self) {
        let Some(auth) = self
            .spotify_web_auth
            .clone()
            .filter(|a| a.uses_secret() && !a.has_user_login())
        else {
            return;
        };
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let status = match auth.token().await {
                Ok(_) => AccountStatus::Connected("your app (Client secret) for search and pages".into()),
                Err(e) => AccountStatus::Error(format!("{e:#}")),
            };
            shared.feed.write().unwrap().spotify_web_api = status;
            shared.repaint();
        });
    }

    pub(super) fn spotify_web_api_login(&mut self) {
        let Some(auth) = self.spotify_web_auth.clone() else {
            self.shared
                .error("Enter your Spotify app's client ID in Settings → Spotify → Advanced first");
            return;
        };
        // Show exactly what goes to Spotify, so a mismatch with the app's settings is visible.
        self.set_account(
            |f| &mut f.spotify_web_api,
            AccountStatus::Working(format!(
                "Waiting for browser login… (sent Client ID {} and Redirect URI {})",
                auth.client_id(),
                auth.redirect()
            )),
        );
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let status = match auth.login().await {
                Ok(_) => AccountStatus::Connected(String::new()),
                Err(e) => AccountStatus::Error(format!("{e:#}")),
            };
            shared.feed.write().unwrap().spotify_web_api = status;
            shared.repaint();
        });
    }

    pub(super) fn merge_spotify(&mut self, sync: SpotifySync) {
        let _ = self.db.set_kv("spotify_user", &sync.user);
        let existing: HashMap<String, Playlist> = {
            let lib = self.shared.library.read().unwrap();
            lib.playlists.iter().map(|p| (p.id.clone(), p.clone())).collect()
        };
        for (meta, list) in &sync.playlists {
            if list.is_some() {
                let _ = self
                    .db
                    .set_kv(&format!("spotify_snapshot:{}", meta.id), &meta.snapshot_id);
            }
        }
        // What couldn't be loaded this time stays as it was.
        if sync.liked.is_none() {
            self.shared
                .error("Spotify Liked Songs couldn't be loaded this time; they stay as they were");
        }
        let kept = sync.kept.len();
        if kept > 0 {
            self.shared.error(format!(
                "{kept} Spotify playlist{} couldn't be loaded this time and stay as they were",
                if kept == 1 { "" } else { "s" }
            ));
        }
        let count = sync.playlists.len();
        let tracks = sync.tracks;
        let mut playlists = spotify_playlists(sync.liked, sync.playlists, &sync.kept, &existing);
        let albums = sync.albums.len();
        playlists.extend(sync.albums);
        self.merge_imported(tracks, playlists, &[PlaylistKind::Spotify, PlaylistKind::SpotifyLiked]);
        self.set_account(|f| &mut f.spotify, AccountStatus::Connected(sync.user));
        self.shared.info(match albums {
            0 => format!("Spotify synced: {count} playlists"),
            1 => format!("Spotify synced: {count} playlists and 1 new album"),
            n => format!("Spotify synced: {count} playlists and {n} new albums"),
        });
        let _ = self.db.set_kv("spotify_synced_at", &now_unix().to_string());
        self.connect_spotify();
    }

    // ---------------------------------------------------------------- soundcloud

    pub(super) fn soundcloud_configured(&self) -> bool {
        !self.cfg.soundcloud.profile_url.trim().is_empty() || !self.cfg.soundcloud.oauth_token.trim().is_empty()
    }

    pub(super) fn start_soundcloud_sync(&mut self) {
        if !self.soundcloud_configured() {
            self.shared.error("Set your SoundCloud profile URL in Settings first");
            return;
        }
        self.set_account(|f| &mut f.soundcloud, AccountStatus::Working("Syncing…".into()));
        let sc = self.soundcloud.clone();
        let profile = self.cfg.soundcloud.profile_url.trim().to_string();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let r = async {
                let user = if profile.is_empty() {
                    sc.me().await?
                } else {
                    sc.resolve_user(&profile).await?
                };
                let likes = sc.likes(user.id).await?;
                let playlists = sc.playlists(user.id).await.map_err(|e| {
                    tracing::warn!("SoundCloud playlists: {e:#}");
                    format!("{e:#}")
                });
                Ok(SoundCloudSync {
                    user: user.username,
                    likes,
                    playlists,
                })
            }
            .await;
            let _ = tx.send(Internal::SoundCloudSynced(r));
        });
    }

    pub(super) fn merge_soundcloud(&mut self, sync: SoundCloudSync) {
        let mut tracks = Vec::new();
        let mut playlists = vec![Playlist {
            id: "soundcloud:likes".into(),
            name: "SoundCloud Likes".into(),
            kind: PlaylistKind::SoundCloudLikes,
            remote_id: None,
            description: format!("Tracks liked by {}", sync.user),
            art: sync.likes.iter().find_map(|t| t.art.clone()),
            track_ids: sync.likes.iter().map(|t| t.id.clone()).collect(),
        }];
        tracks.extend(sync.likes);
        // Playlists that couldn't be loaded stay as they were (not taken for deleted).
        let prune = soundcloud_prune(&sync.playlists);
        let lists = match sync.playlists {
            Ok(lists) => lists,
            Err(e) => {
                self.shared.error(format!(
                    "SoundCloud playlists couldn't be loaded ({e}); they stay as they were"
                ));
                Vec::new()
            }
        };
        let count = lists.len();
        for p in lists {
            playlists.push(Playlist {
                id: format!("soundcloud:pl:{}", p.remote_id),
                name: p.name,
                kind: PlaylistKind::SoundCloud,
                remote_id: Some(p.remote_id),
                description: p.description,
                art: p.art,
                track_ids: p.tracks.iter().map(|t| t.id.clone()).collect(),
            });
            tracks.extend(p.tracks);
        }
        self.merge_imported(tracks, playlists, prune);
        let _ = self.db.set_kv("soundcloud_user", &sync.user);
        let _ = self.db.set_kv("soundcloud_synced_at", &now_unix().to_string());
        self.set_account(|f| &mut f.soundcloud, AccountStatus::Connected(sync.user));
        self.shared
            .info(format!("SoundCloud synced: likes + {count} playlists"));
    }

    // ---------------------------------------------------------------- apple music

    pub(super) fn import_apple_api(&mut self) {
        let c = &self.cfg.apple_music;
        if c.user_token.trim().is_empty() {
            self.shared.error("Paste your Apple Music user token in Settings first");
            return;
        }
        self.shared.info("Importing from Apple Music…");
        let http = self.http.clone();
        let (dev, user, store) = (c.developer_token.clone(), c.user_token.clone(), c.storefront.clone());
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let r = async {
                let dev = if dev.trim().is_empty() {
                    apple_music::scrape_developer_token(&http).await?
                } else {
                    dev
                };
                let store = if store.trim().is_empty() {
                    "us".to_string()
                } else {
                    store
                };
                let api = AppleMusicApi::new(http, dev.trim(), user.trim(), &store);
                let songs = api.library_songs().await?;
                let playlists = api.library_playlists().await?;
                Ok((songs, playlists))
            }
            .await;
            let _ = tx.send(Internal::AppleImported(r));
        });
    }

    pub(super) fn merge_apple(&mut self, songs: Vec<Track>, imported: Vec<ImportedPlaylist>) {
        let mut tracks = songs.clone();
        let mut playlists = Vec::new();
        if !songs.is_empty() {
            playlists.push(Playlist {
                id: "applemusic:library".into(),
                name: "Apple Music Library".into(),
                kind: PlaylistKind::AppleMusic,
                remote_id: None,
                description: "Songs imported from Apple Music".into(),
                art: songs.iter().find_map(|t| t.art.clone()),
                track_ids: songs.iter().map(|t| t.id.clone()).collect(),
            });
        }
        let count = imported.len();
        for p in imported {
            playlists.push(Playlist {
                id: format!("applemusic:{}", p.remote_id),
                name: p.name,
                kind: PlaylistKind::AppleMusic,
                remote_id: Some(p.remote_id),
                description: p.description,
                art: p.art.or_else(|| p.tracks.iter().find_map(|t| t.art.clone())),
                track_ids: p.tracks.iter().map(|t| t.id.clone()).collect(),
            });
            tracks.extend(p.tracks);
        }
        self.merge_imported(tracks, playlists, &[]);
        self.shared.info(format!(
            "Apple Music imported: {} songs, {count} playlists",
            songs.len()
        ));
    }
}

/// The playlists a Spotify sync leaves: what it loaded, and in place of what it couldn't load
/// (Liked Songs, some playlists) the lists as they were.
pub(super) fn spotify_playlists(
    liked: Option<Vec<String>>,
    lists: Vec<(SpotifyPlaylistMeta, Option<Vec<String>>)>,
    kept: &[String],
    existing: &HashMap<String, Playlist>,
) -> Vec<Playlist> {
    let as_before = |id: &str| existing.get(id).map(|p| p.track_ids.clone()).unwrap_or_default();
    let mut playlists = vec![Playlist {
        id: "spotify:liked".into(),
        name: "Liked Songs".into(),
        kind: PlaylistKind::SpotifyLiked,
        remote_id: None,
        description: "Your liked songs on Spotify".into(),
        art: None,
        track_ids: liked.unwrap_or_else(|| as_before("spotify:liked")),
    }];
    for (meta, list) in lists {
        let id = format!("spotify:{}", meta.id);
        let track_ids = list.unwrap_or_else(|| as_before(&id));
        playlists.push(Playlist {
            id,
            name: meta.name,
            kind: PlaylistKind::Spotify,
            remote_id: Some(meta.id),
            description: meta.description,
            art: meta.art,
            track_ids,
        });
    }
    playlists.extend(
        kept.iter()
            .filter_map(|id| existing.get(&format!("spotify:{id}")).cloned()),
    );
    playlists
}

/// Playlists of the kinds a sync covers (`prune_kinds`) that it didn't bring: deleted on the
/// service.
pub(super) fn removed_playlists(have: &[Playlist], synced: &[Playlist], prune_kinds: &[PlaylistKind]) -> Vec<String> {
    let keep: HashSet<&str> = synced.iter().map(|p| p.id.as_str()).collect();
    have.iter()
        .filter(|p| prune_kinds.contains(&p.kind) && !keep.contains(p.id.as_str()))
        .map(|p| p.id.clone())
        .collect()
}

/// Which SoundCloud lists a sync may take for deleted: not the playlists when they couldn't
/// be loaded.
pub(super) fn soundcloud_prune<T, E>(playlists: &std::result::Result<T, E>) -> &'static [PlaylistKind] {
    match playlists {
        Ok(_) => &[PlaylistKind::SoundCloud, PlaylistKind::SoundCloudLikes],
        Err(_) => &[PlaylistKind::SoundCloudLikes],
    }
}

pub(super) struct SpotifySync {
    pub(super) user: String,
    /// Playlists in Spotify's order with their track ids. `None` = unchanged since last sync.
    pub(super) playlists: Vec<(SpotifyPlaylistMeta, Option<Vec<String>>)>,
    /// Liked Songs track ids, newest first. `None` = couldn't be loaded (kept as they are).
    pub(super) liked: Option<Vec<String>>,
    /// Playlists (Spotify ids) that couldn't be loaded this time: kept as they are rather
    /// than taken for deleted.
    pub(super) kept: Vec<String>,
    /// Albums saved on Spotify that are new to the library.
    pub(super) albums: Vec<Playlist>,
    /// New or updated track metadata.
    pub(super) tracks: Vec<Track>,
}

pub(super) struct SoundCloudSync {
    pub(super) user: String,
    pub(super) likes: Vec<Track>,
    /// Why the playlists couldn't be loaded, if they couldn't (they are kept as they are).
    pub(super) playlists: std::result::Result<Vec<ImportedPlaylist>, String>,
}

/// Imports playlists and Liked Songs through the playback session (not rate limited like the
/// public Web API). Only tracks we don't know yet get their metadata fetched.
pub(super) async fn spotify_sync_internal(
    session: librespot_core::session::Session,
    known: HashSet<String>,
    known_albums: HashSet<String>,
    progress: impl Fn(String),
) -> Result<SpotifySync> {
    use crate::providers::spotify_internal as si;
    use futures_util::StreamExt;

    /// Spotify requests that hang shouldn't stall the sync forever.
    async fn timed<T>(what: &str, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        tokio::time::timeout(Duration::from_secs(45), fut)
            .await
            .map_err(|_| anyhow!("timed out {what}"))?
    }

    progress("Loading your playlists…".into());
    let user = tokio::time::timeout(Duration::from_secs(15), si::display_name(&session))
        .await
        .unwrap_or_else(|_| session.username());
    let ids = timed("loading your playlists", si::rootlist(&session)).await?;
    let total = ids.len();
    let mut lists = Vec::with_capacity(total);
    let mut kept = Vec::new();
    {
        let mut results = futures_util::stream::iter(ids)
            .map(|id| {
                let session = session.clone();
                async move {
                    let result = timed("loading a playlist", si::playlist(&session, &id)).await;
                    (id, result)
                }
            })
            .buffered(4);
        let mut done = 0;
        while let Some((id, result)) = results.next().await {
            done += 1;
            progress(format!("Loading playlists… {done}/{total}"));
            match result {
                Ok(p) => lists.push(p),
                Err(e) => {
                    tracing::warn!("keeping Spotify playlist {id} as it was: {e:#}");
                    kept.push(id);
                }
            }
        }
    }
    progress("Loading Liked Songs and albums…".into());
    let collection = timed("loading Liked Songs", si::collection(&session))
        .await
        .map_err(|e| tracing::warn!("Spotify Liked Songs: {e:#}"))
        .ok();
    let liked_loaded = collection.is_some();
    let collection = collection.unwrap_or_default();
    let liked: Vec<(String, i64)> = collection
        .iter()
        .filter(|(uri, _)| uri.starts_with("spotify:track:"))
        .cloned()
        .collect();
    // Albums saved on Spotify that aren't in the library yet (albums already here are kept
    // as they are, and ones removed on Spotify stay until removed here).
    let new_albums: Vec<String> = collection
        .iter()
        .filter_map(|(uri, _)| uri.strip_prefix("spotify:album:"))
        .filter(|id| !known_albums.contains(*id))
        .map(str::to_string)
        .collect();
    let mut albums = Vec::with_capacity(new_albums.len());
    let mut album_tracks = Vec::new();
    {
        let total = new_albums.len();
        let mut results = futures_util::stream::iter(new_albums)
            .map(|id| {
                let session = session.clone();
                async move {
                    let result = timed("loading an album", si::album_page(&session, &id)).await;
                    (id, result)
                }
            })
            .buffered(3);
        let mut done = 0;
        while let Some((id, result)) = results.next().await {
            done += 1;
            progress(format!("Loading your albums… {done}/{total}"));
            match result {
                Ok(page) if !page.tracks.is_empty() => {
                    albums.push(Playlist {
                        id: saved_album_id(PlaylistKind::SpotifyAlbum, &id),
                        name: page.title,
                        kind: PlaylistKind::SpotifyAlbum,
                        remote_id: Some(id),
                        description: page.artist,
                        art: page.image,
                        track_ids: page.tracks.iter().map(|t| t.id.clone()).collect(),
                    });
                    album_tracks.extend(page.tracks);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("skipping Spotify album {id}: {e:#}"),
            }
        }
    }

    let mut need = Vec::new();
    let mut seen = HashSet::new();
    let all = lists
        .iter()
        .flat_map(|p| p.items.iter().map(|(u, _)| u))
        .chain(liked.iter().map(|(u, _)| u));
    for uri in all {
        if uri.starts_with("spotify:track:") && !known.contains(uri) && seen.insert(uri.as_str()) {
            need.push(uri.clone());
        }
    }
    let mut fetched: HashMap<String, Track> = HashMap::with_capacity(need.len());
    {
        let chunks: Vec<Vec<String>> = need.chunks(500).map(|c| c.to_vec()).collect();
        let mut batches = futures_util::stream::iter(chunks)
            .map(|chunk| {
                let session = session.clone();
                async move {
                    let result = timed("loading song details", si::tracks(&session, &chunk)).await;
                    (chunk.len(), result)
                }
            })
            .buffered(2);
        let mut done = 0;
        while let Some((n, result)) = batches.next().await {
            done += n;
            progress(format!("Loading song details… {done}/{}", need.len()));
            fetched.extend(result?);
        }
    }
    for (uri, at) in &liked {
        if let Some(t) = fetched.get_mut(uri) {
            t.added_at = *at;
        }
    }
    let have = |uri: &String| known.contains(uri) || fetched.contains_key(uri);
    let playlists = lists
        .into_iter()
        .map(|p| {
            let ids = p.items.iter().map(|(u, _)| u).filter(|u| have(u)).cloned().collect();
            (p.meta, Some(ids))
        })
        .collect();
    let liked_ids = liked.iter().map(|(u, _)| u).filter(|u| have(u)).cloned().collect();
    Ok(SpotifySync {
        user,
        playlists,
        liked: liked_loaded.then_some(liked_ids),
        kept,
        albums,
        tracks: fetched.into_values().chain(album_tracks).collect(),
    })
}

/// Fallback import through the public Web API.
pub(super) async fn spotify_sync_web(
    auth: &SpotifyAuth,
    api: &SpotifyApi,
    known: &HashMap<String, String>,
) -> Result<SpotifySync> {
    let mut token = auth.token().await?;
    let user = match api.me(&token).await {
        Ok(u) => u,
        Err(e) if spotify_api::is_unauthorized(&e) => {
            auth.invalidate();
            token = auth.token().await?;
            api.me(&token).await?
        }
        Err(e) => return Err(e),
    };
    let metas = api.playlists(&token).await?;
    let mut playlists = Vec::with_capacity(metas.len());
    let mut tracks = Vec::new();
    for meta in metas {
        if known.get(&meta.id) == Some(&meta.snapshot_id) && !meta.snapshot_id.is_empty() {
            playlists.push((meta, None));
            continue;
        }
        // Tokens last an hour; refresh between playlists for huge libraries.
        token = auth.token().await?;
        match api.playlist_tracks(&token, &meta.id).await {
            Ok(list) => {
                playlists.push((meta, Some(list.iter().map(|t| t.id.clone()).collect())));
                tracks.extend(list);
            }
            Err(e) => {
                tracing::warn!("skipping playlist {}: {e:#}", meta.name);
                playlists.push((meta, None));
            }
        }
    }
    let liked = api.liked_tracks(&auth.token().await?).await?;
    let liked_ids = liked.iter().map(|t| t.id.clone()).collect();
    tracks.extend(liked);
    Ok(SpotifySync {
        user: if user.display_name.is_empty() {
            user.id
        } else {
            user.display_name
        },
        playlists,
        liked: Some(liked_ids),
        kept: Vec::new(),
        // The Web API fallback doesn't bring in saved albums.
        albums: Vec::new(),
        tracks,
    })
}
