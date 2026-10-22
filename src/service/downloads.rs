//! Downloading songs from SoundCloud, Spotify and Apple Music.

use super::*;

impl Service {
    // ---------------------------------------------------------------- downloads

    pub(super) fn load_downloads(&mut self) {
        // Files that are gone (or on a drive that isn't mounted) are left out, not forgotten.
        let downloaded = self
            .db
            .kv_with_prefix(DOWNLOAD_KEY)
            .into_iter()
            .map(|(id, path)| (id, PathBuf::from(path)))
            .filter(|(_, path)| path.exists())
            .collect();
        self.shared.feed.write().unwrap().downloaded = downloaded;
    }

    /// The file to play for `track` when it is one: a local file or a downloaded song.
    pub(super) fn file_of(&self, track: &Track) -> Option<String> {
        match track.source {
            Source::Local => Some(track.uri.clone()),
            _ => self.downloaded_file(track),
        }
    }

    pub(super) fn downloaded_file(&self, track: &Track) -> Option<String> {
        let feed = self.shared.feed.read().unwrap();
        let path = feed.downloaded.get(&track.id)?;
        path.exists().then(|| path.to_string_lossy().into_owned())
    }

    pub(super) fn download(&mut self, tracks: Vec<Track>) {
        let mut already = 0;
        let mut added: Vec<String> = Vec::new();
        {
            let mut feed = self.shared.feed.write().unwrap();
            let mut seen = HashSet::new();
            for track in tracks {
                // Local files are files already.
                if track.source == Source::Local || !seen.insert(track.id.clone()) {
                    continue;
                }
                if feed.downloaded.get(&track.id).is_some_and(|p| p.exists()) {
                    already += 1;
                    continue;
                }
                match feed.downloads.iter_mut().find(|d| d.track.id == track.id) {
                    Some(d) if d.state.active() => continue,
                    // Failed before, or the file was deleted since: try again.
                    Some(d) => d.state = DownloadState::Queued,
                    None => feed.downloads.push(DownloadItem {
                        track: track.clone(),
                        state: DownloadState::Queued,
                    }),
                }
                added.push(track.title);
            }
        }
        match added.as_slice() {
            [] if already == 1 => self.shared.info("Already downloaded"),
            [] if already > 1 => self.shared.info(format!("All {already} songs are already downloaded")),
            [] => {}
            [title] => self.shared.info(format!("Downloading “{title}”…")),
            many => self.shared.info(format!("Downloading {} songs…", many.len())),
        }
        self.pump_downloads();
    }

    /// Starts queued downloads, a couple at a time.
    pub(super) fn pump_downloads(&mut self) {
        const PARALLEL: usize = 2;
        while self.download_jobs.len() < PARALLEL {
            let track = {
                let mut feed = self.shared.feed.write().unwrap();
                let Some(item) = feed.downloads.iter_mut().find(|d| d.state == DownloadState::Queued) else {
                    break;
                };
                item.state = DownloadState::Running(0.0);
                item.track.clone()
            };
            // Apple Music songs already matched to Spotify get Spotify's details.
            let spotify_twin = (track.source == Source::AppleMusic)
                .then(|| self.cached_resolution(&track))
                .flatten()
                .filter(|t| t.source == Source::Spotify);
            let wants_spotify = (track.source == Source::Spotify || spotify_twin.is_some())
                && self.cfg.spotify.enabled
                && self.spotify_auth.has_login();
            let spotify = if wants_spotify { self.spotify_session() } else { None };
            let d = &self.cfg.downloads;
            let downloader = Downloader {
                soundcloud: self.soundcloud.clone(),
                http: self.http.clone(),
                lyrics: d.lyrics.then(|| self.lyrics.clone()),
                spotify,
                spotify_twin,
                ytdlp: d
                    .youtube
                    .then(|| YtDlp::new(&d.ytdlp_path, &d.ytdlp_args).with_mp3(d.youtube_mp3)),
            };
            let dir = self.cfg.download_dir(track.source);
            self.download_seq += 1;
            let work = dir.join(downloader::work_dir_name(self.download_seq));
            let job_work = work.clone();
            let job_id = track.id.clone();
            let shared = self.shared.clone();
            let tx = self.internal_tx.clone();
            let task = tokio::spawn(async move {
                let id = track.id.clone();
                let last = std::sync::Mutex::new(Instant::now());
                let progress = |p: f32| {
                    {
                        let mut last = last.lock().unwrap();
                        if p < 1.0 && last.elapsed() < Duration::from_millis(150) {
                            return;
                        }
                        *last = Instant::now();
                    }
                    let mut feed = shared.feed.write().unwrap();
                    if let Some(d) = feed.downloads.iter_mut().find(|d| d.track.id == id) {
                        if let DownloadState::Running(old) = &mut d.state {
                            *old = p;
                        }
                    }
                    drop(feed);
                    shared.repaint();
                };
                // A bug in one download must still free its place and show as failed.
                let result = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                    downloader.download(&track, &dir, &job_work, &progress),
                ))
                .await
                .unwrap_or_else(|_| Err(anyhow!("the download stopped unexpectedly")));
                let _ = tx.send(Internal::DownloadDone { track, result });
            });
            self.download_jobs.insert(job_id, (task.abort_handle(), work));
        }
        self.shared.repaint();
    }

    pub(super) fn download_finished(&mut self, track: Track, result: Result<Saved>) {
        self.download_jobs.remove(&track.id);
        let state = match result {
            Ok(Saved { path, from }) => {
                let _ = self
                    .db
                    .set_kv(&format!("{DOWNLOAD_KEY}{}", track.id), &path.to_string_lossy());
                self.shared
                    .feed
                    .write()
                    .unwrap()
                    .downloaded
                    .insert(track.id.clone(), path.clone());
                self.add_downloaded_file(&path);
                self.download_batch.push((track.title.clone(), None));
                DownloadState::Done { path, from }
            }
            Err(e) => {
                let error = friendly_download_error(&e);
                tracing::warn!("download of {} failed: {e:#}", track.id);
                self.download_batch.push((track.title.clone(), Some(error.clone())));
                DownloadState::Failed(error)
            }
        };
        let idle = {
            let mut feed = self.shared.feed.write().unwrap();
            if let Some(d) = feed.downloads.iter_mut().find(|d| d.track.id == track.id) {
                d.state = state;
            }
            !feed.downloads.iter().any(|d| d.state.active())
        };
        if idle {
            let batch = std::mem::take(&mut self.download_batch);
            let failed: Vec<_> = batch.iter().filter(|(_, e)| e.is_some()).collect();
            let ok = batch.len() - failed.len();
            match (ok, failed.as_slice()) {
                (0, []) => {}
                (1, []) => self.shared.info(format!("Downloaded “{}”", batch[0].0)),
                (n, []) => self.shared.info(format!("Downloaded {n} songs")),
                (0, [(title, Some(e))]) => self.shared.error(format!("Couldn't download “{title}”: {e}")),
                (0, f) => self
                    .shared
                    .error(format!("{} downloads failed (see Downloads)", f.len())),
                (n, f) => self
                    .shared
                    .error(format!("Downloaded {n} songs, {} failed (see Downloads)", f.len())),
            }
        }
        self.pump_downloads();
    }

    /// Stops the download of `id`, or all of them.
    pub(super) fn cancel_downloads(&mut self, id: Option<&str>) {
        let cancelled: Vec<String> = {
            let mut feed = self.shared.feed.write().unwrap();
            let picked = |d: &DownloadItem| d.state.active() && id.is_none_or(|id| d.track.id == id);
            let ids = feed
                .downloads
                .iter()
                .filter(|d| picked(d))
                .map(|d| d.track.id.clone())
                .collect();
            feed.downloads.retain(|d| !picked(d));
            ids
        };
        for track_id in &cancelled {
            if let Some((task, work)) = self.download_jobs.remove(track_id) {
                task.abort();
                // yt-dlp / ffmpeg are killed with the task; give them a moment to let go of
                // the folder.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let _ = tokio::fs::remove_dir_all(&work).await;
                });
            }
        }
        if id.is_none() && !cancelled.is_empty() {
            self.shared.info(match cancelled.len() {
                1 => "Download cancelled".to_string(),
                n => format!("{n} downloads cancelled"),
            });
        }
        if !self
            .shared
            .feed
            .read()
            .unwrap()
            .downloads
            .iter()
            .any(|d| d.state.active())
        {
            self.download_batch.clear();
        }
        self.pump_downloads();
    }

    /// Clears work folders a crash or power cut left in the download folders.
    pub(super) fn clear_download_leftovers(&self) {
        let mut dirs: Vec<PathBuf> = [Source::SoundCloud, Source::Spotify, Source::AppleMusic]
            .into_iter()
            .map(|s| self.cfg.download_dir(s))
            .collect();
        dirs.dedup();
        tokio::task::spawn_blocking(move || {
            for dir in dirs {
                let Ok(entries) = std::fs::read_dir(&dir) else { continue };
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with(downloader::WORK_PREFIX) {
                        let path = entry.path();
                        let _ = if path.is_dir() {
                            std::fs::remove_dir_all(&path)
                        } else {
                            std::fs::remove_file(&path)
                        };
                    }
                }
            }
        });
    }

    /// Puts a finished download straight into the library when it was saved inside a library
    /// folder (instead of waiting for the next scan).
    pub(super) fn add_downloaded_file(&mut self, path: &Path) {
        let Some(folder) = self.cfg.library.folders.iter().find(|f| path.starts_with(f)) else {
            return;
        };
        let track = library::scanner::read_track_in(path, Some(folder), None, now_unix());
        let mtimes = HashMap::from([(track.id.clone(), library::scanner::mtime_of(path))]);
        if let Err(e) = self.db.upsert_tracks(std::slice::from_ref(&track), &mtimes) {
            tracing::warn!("couldn't add {} to the library: {e:#}", path.display());
            return;
        }
        let mut lib = self.shared.library.write().unwrap();
        lib.tracks.insert(track.id.clone(), track);
        lib.reindex();
    }

    /// What it takes to look an artist up on Spotify by name (search, then their page through
    /// the session); `None` when Spotify isn't logged in or connected yet.
    pub(super) fn spotify_lookup(&mut self) -> Option<SpotifyLookup> {
        if !self.cfg.spotify.enabled || !self.spotify_auth.has_login() {
            return None;
        }
        let auth = self.web_auth()?;
        let session = self.spotify_session()?;
        Some(SpotifyLookup {
            auth,
            api: self.spotify_api.clone(),
            session,
        })
    }

    /// The Spotify session, or `None` while (re)connecting.
    pub(super) fn spotify_session(&mut self) -> Option<librespot_core::session::Session> {
        if let Some(session) = self.spotify.as_ref().and_then(|e| e.session()) {
            return Some(session);
        }
        if let Some(sp) = self.spotify.take() {
            // Session dropped: reconnect from scratch.
            sp.shutdown();
        }
        self.connect_spotify();
        None
    }

    pub(super) fn lastfm_login(&mut self) {
        let Some(lfm) = self.lastfm.clone() else {
            self.shared.error("Enter your Last.fm API key and secret first");
            return;
        };
        self.set_account(
            |f| &mut f.lastfm,
            AccountStatus::Working("Approve Sumo in your browser…".into()),
        );
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let r = async {
                let token = lfm.get_token().await?;
                let _ = open::that_detached(lfm.auth_url(&token));
                // Poll until the user approved access (up to 3 minutes).
                for _ in 0..60 {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    match lfm.get_session(&token).await {
                        Ok(s) => return Ok(s),
                        Err(e) if crate::integrations::lastfm::is_pending_auth(&e) => continue,
                        Err(e) => return Err(e),
                    }
                }
                Err(anyhow!("timed out waiting for Last.fm approval"))
            }
            .await;
            let _ = tx.send(Internal::LastfmSession(r));
        });
    }

    pub(super) async fn update_config(&mut self, mut cfg: Config) {
        // Secrets obtained by the service are not editable in the UI copy.
        cfg.lastfm.session_key = self.cfg.lastfm.session_key.clone();
        cfg.lastfm.username = self.cfg.lastfm.username.clone();
        // The volume is set with SetVolume; the UI's copy is the one it started with, and
        // taking it would jump back to that volume on the next song.
        cfg.playback.volume = self.cfg.playback.volume;
        let old = std::mem::replace(&mut self.cfg, cfg);

        if old.playback != self.cfg.playback {
            for mpv in self.mpv.iter().chain(self.mpv_spare.iter()) {
                let p = &self.cfg.playback;
                let _ = mpv
                    .command(serde_json::json!([
                        "set_property",
                        "audio-exclusive",
                        if p.bit_perfect { "yes" } else { "no" }
                    ]))
                    .await;
                let _ = mpv.set_replaygain(p.replaygain && !p.bit_perfect).await;
                let _ = mpv
                    .command(serde_json::json!([
                        "set_property",
                        "gapless-audio",
                        if p.gapless { "weak" } else { "no" }
                    ]))
                    .await;
                let dev = if p.audio_device.is_empty() {
                    "auto"
                } else {
                    p.audio_device.as_str()
                };
                let _ = mpv
                    .command(serde_json::json!(["set_property", "audio-device", dev]))
                    .await;
            }
            if old.playback.mpv_path != self.cfg.playback.mpv_path {
                if self.engine != Engine::Mpv {
                    if let Some(mpv) = self.mpv.take() {
                        mpv.quit().await;
                    }
                }
                if let Some(mpv) = self.mpv_spare.take() {
                    mpv.quit().await;
                }
            }
        }
        if old.discord != self.cfg.discord {
            let d = &self.cfg.discord;
            self.discord.configure(d.enabled, &d.app_id, d.song_as_activity_name);
            self.update_presence();
        }
        if old.lastfm != self.cfg.lastfm {
            self.lastfm = make_lastfm(&self.cfg, &self.http, &self.paths);
            self.scrobble.set_instant(self.cfg.lastfm.scrobble_instantly);
        }
        if old.soundcloud != self.cfg.soundcloud {
            self.soundcloud = Arc::new(SoundCloud::new(
                self.http.clone(),
                &self.cfg.soundcloud.client_id,
                &self.cfg.soundcloud.oauth_token,
            ));
        }
        if old.lyrics != self.cfg.lyrics {
            self.lyrics = Arc::new(
                LyricsFetcher::new(self.http.clone(), self.paths.lyrics_cache(), self.cfg.lyrics.online)
                    .with_genius(genius::shared()),
            );
            if let Some(t) = self.queue.current().cloned() {
                self.request_lyrics(&t);
            }
        }
        if old.spotify.web_api_client_id != self.cfg.spotify.web_api_client_id
            || old.spotify.web_api_redirect() != self.cfg.spotify.web_api_redirect()
            || old.spotify.web_api_client_secret != self.cfg.spotify.web_api_client_secret
        {
            self.spotify_web_auth = SpotifyAuth::web_api(&self.cfg.spotify, &self.paths.spotify_dir()).map(Arc::new);
            self.publish_accounts();
            self.check_app_secret();
        }
        if old.spotify != self.cfg.spotify {
            if old.spotify.client_id != self.cfg.spotify.client_id
                || old.spotify.redirect_port != self.cfg.spotify.redirect_port
            {
                self.spotify_auth = Arc::new(SpotifyAuth::new(&self.cfg.spotify, &self.paths.spotify_dir()));
                self.publish_accounts();
            }
            // Bitrate/normalisation apply on the next connection.
            if self.engine != Engine::Spotify {
                if let Some(sp) = self.spotify.take() {
                    sp.shutdown();
                }
            }
        }
        if old.library.folders != self.cfg.library.folders {
            self.start_scan();
        }
        self.save_config();
    }

    pub(super) fn save_config(&self) {
        if let Err(e) = self.cfg.save(&self.paths) {
            self.shared.error(format!("Couldn't save settings: {e:#}"));
        }
    }
}

/// Short reason for the Downloads list (the song's title is shown next to it).
pub(super) fn friendly_download_error(e: &anyhow::Error) -> String {
    let offline = e.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|r| r.is_connect() || r.is_timeout())
    });
    if offline {
        return "Couldn't reach SoundCloud (offline?)".into();
    }
    // Drop wrappers that only repeat which song it was.
    let parts: Vec<String> = e
        .chain()
        .map(|c| c.to_string())
        .filter(|m| !(m.starts_with("couldn't load \"") && m.ends_with("from SoundCloud")))
        .collect();
    let text = if parts.is_empty() {
        format!("{e}")
    } else {
        parts.join(": ")
    };
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => "Download failed".into(),
    }
}
