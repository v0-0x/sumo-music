//! Playing songs: loading them into mpv or librespot, their events, crossfades, and what the UI and media controls are told.

use super::*;

impl Service {
    // ---------------------------------------------------------------- playback

    pub(super) fn current_position(&self) -> f64 {
        if self.status == PlayStatus::Playing {
            self.position + self.position_at.elapsed().as_secs_f64()
        } else {
            self.position
        }
    }

    pub(super) fn set_position(&mut self, secs: f64) {
        self.position = secs.max(0.0);
        self.position_at = Instant::now();
    }

    pub(super) fn set_status(&mut self, status: PlayStatus) {
        if status != self.status {
            // Freeze the extrapolated position when leaving "playing".
            let p = self.current_position();
            self.status = status;
            self.set_position(p);
        }
        self.publish_player();
        self.update_presence();
    }

    /// Starts playing the queue's current entry from `start` seconds.
    /// Starts the queue's current track, cutting a crossfade short (skips, new playlists).
    pub(super) async fn play_current(&mut self, start: f64) {
        self.cancel_fades().await;
        self.load_current(start).await;
    }

    /// Loads the queue's current track. During a crossfade it starts silent and fades in.
    pub(super) async fn load_current(&mut self, start: f64) {
        let Some(track) = self.queue.current().cloned() else {
            self.stop().await;
            return;
        };
        self.load_seq += 1;
        self.started_reported = false;
        self.pause_wanted = false;
        // `start` is where this song starts; a position saved for another one doesn't apply.
        self.resume_position = None;
        self.playing = None;
        self.duration = track.duration_secs();
        self.set_position(start);
        self.status = PlayStatus::Loading;
        self.publish_player();
        self.request_lyrics(&track);
        self.dispatch(track, start).await;
    }

    /// Plays a track on the engine matching its source.
    pub(super) async fn dispatch(&mut self, track: Track, start: f64) {
        let seq = self.load_seq;
        // Local files, and downloaded songs from any service, play from the file.
        if let Some(file) = self.file_of(&track) {
            self.start_mpv(track, &file, start).await;
            return;
        }
        match track.source {
            Source::Local => {
                let path = track.uri.clone();
                self.start_mpv(track, &path, start).await;
            }
            Source::SoundCloud => {
                self.silence_engine().await;
                let sc = self.soundcloud.clone();
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let msg = match sc.stream_url(&track).await {
                        Ok(url) => Internal::StreamReady { seq, track, url },
                        Err(e) => Internal::LoadFailed {
                            seq,
                            error: format!("SoundCloud: {e:#}"),
                        },
                    };
                    let _ = tx.send(msg);
                });
            }
            Source::Spotify => self.start_spotify(track, start).await,
            Source::AppleMusic => {
                if let Some(t) = self.cached_resolution(&track) {
                    Box::pin(self.dispatch_resolved(track, Some(t), start)).await;
                    return;
                }
                let local = self
                    .shared
                    .library
                    .read()
                    .unwrap()
                    .local_match(&track.artist, &track.title)
                    .cloned();
                if local.is_some() {
                    Box::pin(self.dispatch_resolved(track, local, start)).await;
                    return;
                }
                self.silence_engine().await;
                let tx = self.internal_tx.clone();
                let resolver = self.resolver();
                tokio::spawn(async move {
                    let resolved = resolver.resolve(&track).await;
                    let _ = tx.send(Internal::Resolved {
                        seq,
                        original: track,
                        resolved,
                    });
                });
            }
        }
    }

    /// Stops the previous song when the next one has to be looked up online first: it would
    /// otherwise play on meanwhile, and its end would skip the new one. During a crossfade the
    /// previous song is on its own player and nothing is playing here.
    pub(super) async fn silence_engine(&mut self) {
        match self.engine {
            Engine::Mpv => {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.stop().await;
                }
            }
            Engine::Spotify => {
                if let Some(sp) = &self.spotify {
                    sp.stop();
                }
            }
            Engine::None => {}
        }
        self.engine = Engine::None;
        self.mpv_preloaded = None;
    }

    pub(super) async fn dispatch_resolved(&mut self, original: Track, resolved: Option<Track>, start: f64) {
        match resolved {
            Some(t) if t.source != Source::AppleMusic => {
                let _ = self.db.set_kv(
                    &format!("resolve:{}", original.id),
                    &serde_json::to_string(&t).unwrap_or_default(),
                );
                {
                    let mut pv = self.shared.player.write().unwrap();
                    pv.via = Some(t.clone());
                }
                self.dispatch(t, start).await;
            }
            _ => {
                let msg = format!("No playable match for “{} – {}”", original.artist, original.title);
                self.load_failed(msg).await;
            }
        }
    }

    pub(super) fn cached_resolution(&self, track: &Track) -> Option<Track> {
        self.db
            .get_kv(&format!("resolve:{}", track.id))
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Login used for Web API calls: the user's own app if authorized, else the main login.
    pub(super) fn web_auth(&self) -> Option<Arc<SpotifyAuth>> {
        if !self.cfg.spotify.enabled {
            return None;
        }
        match &self.spotify_web_auth {
            Some(a) if a.has_login() => Some(a.clone()),
            _ => self.spotify_auth.has_login().then(|| self.spotify_auth.clone()),
        }
    }

    /// Login for Web API calls about the user's own library (likes, playlists): an app token
    /// from a Client secret can't do those.
    pub(super) fn user_web_auth(&self) -> Option<Arc<SpotifyAuth>> {
        if !self.cfg.spotify.enabled {
            return None;
        }
        match &self.spotify_web_auth {
            Some(a) if a.has_user_login() => Some(a.clone()),
            _ => self.spotify_auth.has_login().then(|| self.spotify_auth.clone()),
        }
    }

    /// Spotify search, for finding a SoundCloud upload's release there.
    pub(super) fn spotify_search(&self) -> Option<(Arc<SpotifyAuth>, Arc<SpotifyApi>)> {
        self.web_auth().map(|auth| (auth, self.spotify_api.clone()))
    }

    pub(super) fn resolver(&self) -> Resolver {
        Resolver {
            spotify: self.web_auth().map(|auth| (auth, self.spotify_api.clone())),
            soundcloud: self.cfg.soundcloud.enabled.then(|| self.soundcloud.clone()),
        }
    }

    pub(super) async fn ensure_mpv(&mut self) -> Result<()> {
        if self.mpv.is_some() {
            return Ok(());
        }
        if let Some(spare) = self.mpv_spare.take() {
            self.mpv = Some(spare);
            return Ok(());
        }
        let opts = MpvOptions {
            binary: crate::tools::resolve(&self.cfg.playback.mpv_path, "mpv"),
            volume: self.cfg.playback.volume,
            replaygain: self.cfg.playback.replaygain && !self.cfg.playback.bit_perfect,
            gapless: self.cfg.playback.gapless,
            audio_device: self.cfg.playback.audio_device.clone(),
            exclusive: self.cfg.playback.bit_perfect,
        };
        let id = self.mpv_next_id;
        self.mpv_next_id += 1;
        self.mpv = Some(Mpv::spawn(&opts, id, self.mpv_tx.clone()).await?);
        Ok(())
    }

    pub(super) async fn start_mpv(&mut self, track: Track, url: &str, start: f64) {
        if self.engine == Engine::Spotify {
            if let Some(sp) = &self.spotify {
                sp.stop();
            }
        }
        if let Err(e) = self.ensure_mpv().await {
            self.load_failed(format!("{e:#}")).await;
            return;
        }
        self.engine = Engine::Mpv;
        self.mpv_preloaded = None;
        self.mpv_entry = None;
        self.playing = Some(track);
        let volume = self.start_volume();
        if let Some(mpv) = &self.mpv {
            let _ = mpv.set_volume(volume).await;
            if let Err(e) = mpv.load(url, start).await {
                self.load_failed(format!("mpv: {e:#}")).await;
            }
        }
    }

    pub(super) async fn start_spotify(&mut self, track: Track, start: f64) {
        if !self.cfg.spotify.enabled || !self.spotify_auth.has_login() {
            self.load_failed("Log in to Spotify in Settings to play Spotify tracks".into())
                .await;
            return;
        }
        // A session that dropped (network change, sleep) is replaced by a new connection.
        if self.spotify.as_ref().is_some_and(|sp| sp.session().is_none()) {
            tracing::info!("Spotify session lost, connecting again");
            if let Some(sp) = self.spotify.take() {
                sp.shutdown();
            }
        }
        if self.spotify.is_none() {
            // Connecting takes a few seconds: in the background, so everything else (other
            // buttons, media keys, a crossfade) carries on. The song starts once connected.
            self.silence_engine().await;
            self.set_account(|f| &mut f.spotify, AccountStatus::Working("Connecting…".into()));
            self.spotify_pending_play = Some((self.load_seq, track, start));
            self.connect_spotify();
            return;
        }
        if self.engine == Engine::Mpv {
            if let Some(mpv) = &self.mpv {
                let _ = mpv.stop().await;
            }
        }
        let volume = self.start_volume();
        let Some(sp) = self.spotify.as_mut() else { return };
        self.engine = Engine::Spotify;
        self.playing = Some(track.clone());
        sp.set_volume(volume);
        if let Err(e) = sp.load(&track.uri, (start * 1000.0) as u32) {
            self.load_failed(format!("{e:#}")).await;
        }
    }

    pub(super) fn spotify_audio_cache(&self) -> Option<PathBuf> {
        self.cfg
            .spotify
            .cache_audio
            .then(|| self.paths.cache_dir.join("spotify"))
    }

    pub(super) async fn load_failed(&mut self, error: String) {
        self.shared.error(error);
        self.consecutive_failures += 1;
        if self.consecutive_failures >= 5 {
            self.shared.error("Too many tracks failed in a row, stopping");
            self.consecutive_failures = 0;
            self.stop().await;
            return;
        }
        if self.queue.advance(true).is_some() {
            // A crossfade in progress carries on into the next track.
            Box::pin(self.load_current(0.0)).await;
            self.publish_queue();
        } else {
            self.stop().await;
        }
    }

    /// Called once the engine actually started producing audio for a new track.
    pub(super) fn on_track_started(&mut self) {
        if self.started_reported {
            return;
        }
        self.started_reported = true;
        self.songs_started += 1;
        self.consecutive_failures = 0;
        if self.fade_in == Some(FadeIn::Waiting) {
            self.start_fade_ramps();
        }
        let Some(track) = self.queue.current().cloned() else {
            return;
        };
        self.detect_quality();
        let now = now_unix();
        // The song from last time, picked up where it stopped: already counted and scrobbled.
        let resumed = self.resumed_scrobbled.take().is_some_and(|id| id == track.id);
        if self.cfg.ui.notifications && !resumed {
            crate::integrations::notify::song(&track.title, &track.artist, &track.album);
        }
        let most_played = if resumed {
            None
        } else {
            let _ = self.db.record_play(&track.id, now);
            self.db.top_plays(library::MOST_PLAYED).ok()
        };
        {
            let mut lib = self.shared.library.write().unwrap();
            if let Some(top) = most_played {
                lib.most_played = top;
            }
            lib.recent.retain(|id| id != &track.id);
            lib.recent.insert(0, track.id.clone());
            lib.recent.truncate(50);
            if !lib.tracks.contains_key(&track.id) {
                lib.tracks.insert(track.id.clone(), track.clone());
                let _ = self.db.upsert_tracks(std::slice::from_ref(&track), &HashMap::new());
            }
            lib.version += 1;
        }
        self.scrobble.start(&track.id, track.duration_ms, now);
        if resumed {
            self.scrobble.mark_scrobbled();
        }
        // In instant mode this scrobbles right away.
        self.scrobble_if_due();
        if let Some(lfm) = self.lastfm.clone() {
            let t = track.clone();
            let spotify = self.spotify_search();
            tokio::spawn(async move {
                let t = lastfm_track(spotify, t).await;
                if let Err(e) = lfm.now_playing(&t).await {
                    tracing::debug!("last.fm now playing: {e:#}");
                }
            });
        }
        self.save_session();
    }

    /// Works out the format of the track that just started.
    pub(super) fn detect_quality(&mut self) {
        self.quality = None;
        let Some(t) = self.playing.clone() else { return };
        // Local files and downloaded songs: the file's own format.
        if let Some(file) = self.file_of(&t) {
            let tx = self.internal_tx.clone();
            tokio::task::spawn_blocking(move || {
                let quality = library::quality::read(std::path::Path::new(&file));
                let _ = tx.send(Internal::Quality {
                    track_id: t.id,
                    quality,
                });
            });
            return;
        }
        match t.source {
            Source::Local => {}
            Source::Spotify => {
                self.quality = Some(AudioQuality {
                    codec: "Ogg Vorbis".into(),
                    bitrate_kbps: Some(self.cfg.spotify.bitrate as u32),
                    ..Default::default()
                });
            }
            // SoundCloud streams are inspected through mpv once loaded.
            Source::SoundCloud | Source::AppleMusic => {}
        }
    }

    pub(super) async fn mpv_stream_quality(&self) -> Option<AudioQuality> {
        let mpv = self.mpv.as_ref()?;
        let codec = mpv
            .command(serde_json::json!(["get_property", "audio-codec-name"]))
            .await
            .ok()?;
        let bitrate = mpv
            .command(serde_json::json!(["get_property", "audio-bitrate"]))
            .await
            .ok()
            .and_then(|v| v.as_f64());
        let rate = mpv
            .command(serde_json::json!(["get_property", "audio-params/samplerate"]))
            .await
            .ok()
            .and_then(|v| v.as_u64())
            .map(|r| r as u32);
        Some(AudioQuality::from_mpv(codec.as_str()?, bitrate, rate))
    }

    pub(super) async fn pause(&mut self) {
        if self.status == PlayStatus::Loading {
            // Loading would start it playing: it starts paused instead.
            self.pause_wanted = true;
            self.publish_player();
            return;
        }
        self.cancel_fades().await;
        match self.engine {
            Engine::Mpv => {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.set_pause(true).await;
                }
            }
            Engine::Spotify => {
                if let Some(sp) = &self.spotify {
                    sp.pause();
                }
            }
            Engine::None => {}
        }
        if self.status == PlayStatus::Playing {
            self.set_status(PlayStatus::Paused);
        }
    }

    pub(super) async fn resume(&mut self) {
        if self.status == PlayStatus::Loading {
            // Already on its way; just don't start it paused.
            self.pause_wanted = false;
            self.publish_player();
            return;
        }
        if self.playing.is_none() || self.engine == Engine::None {
            // Nothing loaded yet (fresh start with a restored session).
            if self.queue.current().is_some() {
                let start = self.resume_position.take().unwrap_or(0.0);
                self.play_current(start).await;
            }
            return;
        }
        match self.engine {
            Engine::Mpv => {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.set_pause(false).await;
                }
            }
            Engine::Spotify => {
                if let Some(sp) = &self.spotify {
                    sp.play();
                }
            }
            Engine::None => {}
        }
        if self.status == PlayStatus::Paused {
            self.set_status(PlayStatus::Playing);
        }
    }

    pub(super) async fn seek(&mut self, secs: f64) {
        self.cancel_fades().await;
        let secs = if self.duration > 0.0 {
            // Songs under half a second exist; clamp would panic with max < min.
            secs.clamp(0.0, (self.duration - 0.5).max(0.0))
        } else {
            secs.max(0.0)
        };
        match self.engine {
            Engine::Mpv => {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.seek(secs).await;
                }
            }
            Engine::Spotify => {
                if let Some(sp) = &self.spotify {
                    sp.seek(secs);
                }
            }
            Engine::None => {
                self.resume_position = Some(secs);
            }
        }
        self.set_position(secs);
        self.publish_player();
        self.update_presence();
    }

    pub(super) async fn stop(&mut self) {
        self.cancel_fades().await;
        match self.engine {
            Engine::Mpv => {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.stop().await;
                }
            }
            Engine::Spotify => {
                if let Some(sp) = &self.spotify {
                    sp.stop();
                }
            }
            Engine::None => {}
        }
        self.engine = Engine::None;
        self.playing = None;
        self.set_position(0.0);
        self.set_status(PlayStatus::Stopped);
    }

    pub(super) async fn after_queue_change(&mut self) {
        self.publish_queue();
        self.publish_player();
        self.refresh_mpv_preload().await;
    }

    /// Queues the next local file in mpv so the transition is gapless.
    pub(super) async fn refresh_mpv_preload(&mut self) {
        // A crossfade starts the next track on another mpv instead.
        if self.engine != Engine::Mpv || !self.cfg.playback.gapless || self.will_crossfade() {
            return;
        }
        // Local files and downloaded songs; streams are resolved when they start.
        let next = self
            .queue
            .peek_next()
            .and_then(|t| Some((t.id.clone(), self.file_of(t)?)));
        let Some(mpv) = &self.mpv else { return };
        let want = next.as_ref().map(|(id, _)| id.clone());
        if want == self.mpv_preloaded {
            return;
        }
        let _ = mpv.clear_upcoming().await;
        self.mpv_preloaded = None;
        if let Some((id, file)) = next {
            if mpv.append(&file).await.is_ok() {
                self.mpv_preloaded = Some(id);
            }
        }
    }

    pub(super) async fn on_mpv(&mut self, id: u64, ev: MpvEvent) {
        if self.mpv.as_ref().map(Mpv::id) != Some(id) {
            // The instance fading out or the spare one: only its exit matters.
            if ev == MpvEvent::Died {
                if self.mpv_spare.as_ref().map(Mpv::id) == Some(id) {
                    self.mpv_spare = None;
                }
                if matches!(&self.fading, Some(Fading { deck: Deck::Mpv(m), .. }) if m.id() == id) {
                    self.fading = None;
                }
            }
            return;
        }
        if ev == MpvEvent::Died {
            self.mpv = None;
            self.mpv_preloaded = None;
            if self.engine == Engine::Mpv {
                self.engine = Engine::None;
                self.playing = None;
                self.set_status(PlayStatus::Stopped);
                self.shared.error("mpv stopped unexpectedly");
            }
            return;
        }
        if self.engine != Engine::Mpv {
            return;
        }
        match ev {
            MpvEvent::FileLoaded => {
                let start = self.resume_position.take().unwrap_or(self.position);
                self.set_position(start);
                self.status = PlayStatus::Playing;
                if std::mem::take(&mut self.pause_wanted) {
                    if let Some(mpv) = &self.mpv {
                        let _ = mpv.set_pause(true).await;
                    }
                    self.status = PlayStatus::Paused;
                }
                self.on_track_started();
                if self.playing.as_ref().is_some_and(|t| t.source == Source::SoundCloud) {
                    self.quality = self.mpv_stream_quality().await;
                }
                self.publish_player();
                self.update_presence();
                self.refresh_mpv_preload().await;
            }
            MpvEvent::Pause(p) => {
                if self.status == PlayStatus::Playing || self.status == PlayStatus::Paused {
                    self.set_status(if p { PlayStatus::Paused } else { PlayStatus::Playing });
                }
            }
            MpvEvent::Duration(d) => {
                if d > 0.0 {
                    self.duration = d;
                    self.scrobble.set_duration_if_unknown((d * 1000.0) as u64);
                    self.publish_player();
                }
            }
            MpvEvent::EndFile { playlist_entry_id, .. } if self.mpv_entry != Some(playlist_entry_id) => {
                // A file that ended as another one was asked for: not the current song.
                tracing::debug!("mpv: ignoring the end of entry {playlist_entry_id}");
            }
            MpvEvent::EndFile { reason, error, .. } => match reason.as_str() {
                "eof" | "error" if self.stream_broke_off() => {
                    let at = self.current_position();
                    tracing::warn!("mpv: the stream stopped at {at:.0}s ({reason}); loading it again");
                    self.reload_at(at).await;
                }
                "eof" => {
                    if let Some(pre) = self.mpv_preloaded.take() {
                        // mpv continues into the appended file by itself.
                        let next = self.queue.advance(false);
                        if next.as_ref().map(|t| &t.id) == Some(&pre) {
                            self.load_seq += 1;
                            self.started_reported = false;
                            self.playing = next.clone();
                            self.duration = next.map(|t| t.duration_secs()).unwrap_or(0.0);
                            self.set_position(0.0);
                            if let Some(t) = self.queue.current().cloned() {
                                self.request_lyrics(&t);
                            }
                            self.publish_queue();
                            self.publish_player();
                            return;
                        }
                        self.play_current(0.0).await;
                        self.publish_queue();
                    } else if self.queue.advance(false).is_some() {
                        self.play_current(0.0).await;
                        self.publish_queue();
                    } else {
                        self.stop().await;
                        self.publish_queue();
                    }
                }
                "error" => {
                    let name = self.playing.as_ref().map(|t| t.title.clone()).unwrap_or_default();
                    self.load_failed(format!(
                        "Can't play “{name}”: {}",
                        error.unwrap_or_else(|| "unknown error".into())
                    ))
                    .await;
                }
                _ => {}
            },
            MpvEvent::StartFile { playlist_entry_id } => self.mpv_entry = Some(playlist_entry_id),
            MpvEvent::Died => {}
        }
    }

    pub(super) async fn on_spotify(&mut self, id: u64, ev: SpotifyEvent) {
        // Events of a player that is fading out (or idle) are stale.
        if self.engine != Engine::Spotify || self.spotify.as_ref().map(SpotifyEngine::current_id) != Some(id) {
            return;
        }
        match ev {
            SpotifyEvent::Loading => {}
            SpotifyEvent::Playing { position_ms } => {
                self.set_position(position_ms as f64 / 1000.0);
                self.status = PlayStatus::Playing;
                if std::mem::take(&mut self.pause_wanted) {
                    if let Some(sp) = &self.spotify {
                        sp.pause();
                    }
                    self.status = PlayStatus::Paused;
                }
                self.on_track_started();
                self.publish_player();
                self.update_presence();
            }
            SpotifyEvent::Paused { position_ms } => {
                self.set_position(position_ms as f64 / 1000.0);
                self.set_status(PlayStatus::Paused);
            }
            SpotifyEvent::Seeked { position_ms } => {
                self.set_position(position_ms as f64 / 1000.0);
                self.publish_player();
            }
            SpotifyEvent::TimeToPreload => {
                if self.will_crossfade() {
                    // The next track goes to another player; preloading here wouldn't help.
                    return;
                }
                if let (Some(next), Some(sp)) = (self.queue.peek_next(), &self.spotify) {
                    if next.source == Source::Spotify {
                        sp.preload(&next.uri);
                    }
                }
            }
            SpotifyEvent::EndOfTrack { uri } if !self.is_playing_uri(&uri) => {
                tracing::debug!("Spotify: ignoring the end of {uri}");
            }
            SpotifyEvent::EndOfTrack { .. } if self.stream_broke_off() => {
                let at = self.current_position();
                tracing::warn!("Spotify: the song stopped at {at:.0}s; connecting again");
                // Usually the connection dropped: start over with a new one if so.
                if self.spotify.as_ref().is_some_and(|sp| sp.session().is_none()) {
                    self.engine = Engine::None;
                }
                self.reload_at(at).await;
            }
            SpotifyEvent::EndOfTrack { .. } => {
                if self.queue.advance(false).is_some() {
                    self.play_current(0.0).await;
                } else {
                    self.stop().await;
                }
                self.publish_queue();
            }
            SpotifyEvent::Unavailable { uri } if !self.is_playing_uri(&uri) => {
                // The next song couldn't be preloaded. The current one plays on; the next one
                // fails with its own name if it still can't be played when its turn comes.
                tracing::warn!("Spotify: {uri} can't be preloaded");
            }
            SpotifyEvent::Unavailable { .. } => {
                let name = self.playing.as_ref().map(|t| t.title.clone()).unwrap_or_default();
                self.load_failed(format!("“{name}” is not available on Spotify in your region"))
                    .await;
            }
            SpotifyEvent::Stopped => {}
        }
    }

    /// Whether the streamed song that just ended stopped well before its end (the connection
    /// dropped, or a stream link expired during a long pause), and hasn't been retried yet.
    pub(super) fn stream_broke_off(&self) -> bool {
        let streamed = self
            .playing
            .as_ref()
            .is_some_and(|t| t.source != Source::Local && self.file_of(t).is_none());
        let position = self.current_position();
        streamed
            && self.retried_load != self.load_seq
            && self.started_reported
            && self.duration > 30.0
            && position > 1.0
            && position < self.duration - 5.0
    }

    /// Plays the current song again from `at` after its stream broke off. It is the same
    /// play: no second play count or scrobble.
    pub(super) async fn reload_at(&mut self, at: f64) {
        let Some(track) = self.queue.current().cloned() else {
            return;
        };
        self.load_seq += 1;
        self.retried_load = self.load_seq;
        self.playing = None;
        self.set_position(at);
        self.status = PlayStatus::Loading;
        self.publish_player();
        self.dispatch(track, at).await;
    }

    /// Whether a Spotify event about `uri` is about the song playing now (an empty `uri` is
    /// taken as the current one).
    pub(super) fn is_playing_uri(&self, uri: &str) -> bool {
        uri.is_empty() || self.playing.as_ref().is_some_and(|t| t.uri == uri)
    }

    pub(super) fn play_named(&mut self, songs: Vec<(String, String)>, context: String, queue: bool) {
        use futures_util::StreamExt;
        if songs.is_empty() {
            return;
        }
        // Files in the library first; the rest is looked up, a few at a time.
        let wanted: Vec<(Track, Option<Track>)> = {
            let lib = self.shared.library.read().unwrap();
            songs
                .into_iter()
                .map(|(artist, title)| {
                    let local = lib.local_match(&artist, &title).cloned();
                    (named_track(&artist, &title), local)
                })
                .collect()
        };
        let lookups = wanted.iter().filter(|(_, local)| local.is_none()).count();
        if lookups > 0 {
            self.shared.info(format!("Finding {}…", songs_text(lookups)));
        }
        let resolver = Arc::new(self.resolver());
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let found: Vec<Option<Track>> = futures_util::stream::iter(wanted)
                .map(|(want, local)| {
                    let resolver = resolver.clone();
                    async move {
                        match local {
                            Some(t) => Some(t),
                            None => resolver.resolve(&want).await,
                        }
                    }
                })
                .buffered(4)
                .collect()
                .await;
            let missing = found.iter().filter(|f| f.is_none()).count();
            let _ = tx.send(Internal::NamedFound {
                tracks: found.into_iter().flatten().collect(),
                missing,
                context,
                queue,
            });
        });
    }

    /// Pauses once the sleep timer runs out: its time is up, or the song it waited for ended.
    pub(super) async fn check_sleep(&mut self) {
        let due = match &self.sleep {
            None => return,
            Some(Sleep::At(at)) => Instant::now() >= *at,
            Some(Sleep::AfterSong(at)) => self.songs_started >= *at || self.status == PlayStatus::Stopped,
        };
        if !due {
            return;
        }
        self.sleep = None;
        if matches!(self.status, PlayStatus::Playing | PlayStatus::Loading) {
            self.pause().await;
            self.shared.info("Sleep timer: paused".to_string());
        }
        self.publish_player();
    }

    pub(super) async fn on_tick(&mut self) {
        let dt = self.last_tick.elapsed();
        self.last_tick = Instant::now();
        let playing = self.status == PlayStatus::Playing;

        if playing && self.engine == Engine::Mpv {
            if let Some(mpv) = &self.mpv {
                if let Some(pos) = mpv.time_pos().await {
                    self.set_position(pos);
                }
            }
        }

        // The queue when it changed; where in the song we are every 15 s, so a crash or power
        // cut picks up close to where it stopped.
        if std::mem::take(&mut self.session_dirty) {
            self.save_session();
        } else if playing && self.position_saved_at.elapsed() >= Duration::from_secs(15) {
            self.save_position();
        }
        // Save the volume once it stops changing (a drag sends dozens of changes).
        if self.volume_changed_at.is_some_and(|t| t.elapsed() >= VOLUME_SAVE_DELAY) {
            self.volume_changed_at = None;
            self.save_config();
        }
        self.scrobble.tick(playing, dt);
        self.scrobble_if_due();
        if playing {
            self.mpris.update(self.queue.current(), true, self.current_position());
        }
        // Expire toasts.
        let expired = {
            let mut feed = self.shared.feed.write().unwrap();
            let before = feed.toasts.len();
            feed.toasts.retain(|t| t.at.elapsed() < Duration::from_secs(6));
            before != feed.toasts.len()
        };
        if expired {
            self.shared.repaint();
        }
    }

    /// Sends the current track to Last.fm once it qualifies (see [`ScrobbleTracker`]).
    pub(super) fn scrobble_if_due(&mut self) {
        if !self.scrobble.should_scrobble() {
            return;
        }
        self.scrobble.mark_scrobbled();
        if let (Some(lfm), Some((_, started))) = (self.lastfm.clone(), self.scrobble.current()) {
            if let Some(t) = self.queue.current().cloned() {
                let shared = self.shared.clone();
                let spotify = self.spotify_search();
                tokio::spawn(async move {
                    let t = lastfm_track(spotify, t).await;
                    match lfm.scrobble(&t, started).await {
                        Ok(None) => {}
                        // Usually bad tags on a local file; silently missing scrobbles are worse.
                        Ok(Some(why)) => shared.error(format!("Not scrobbled: {why}")),
                        // The login was revoked (or the key changed): scrobbles wait in the queue
                        // (for 14 days) until Last.fm is connected again.
                        Err(e) if crate::integrations::lastfm::is_auth_error(&e) => {
                            tracing::warn!("scrobble refused: {e:#}");
                            let mut feed = shared.feed.write().unwrap();
                            if !matches!(feed.lastfm, AccountStatus::Error(_)) {
                                feed.lastfm = AccountStatus::Error(
                                    "Last.fm logged Sumo out: connect again in Settings (scrobbles are kept meanwhile)"
                                        .into(),
                                );
                                drop(feed);
                                shared.error("Last.fm logged Sumo out: connect again in Settings to keep scrobbling");
                            }
                        }
                        Err(e) => tracing::warn!("scrobble failed: {e:#}"),
                    }
                });
            }
        }
    }

    // ---------------------------------------------------------------- crossfade

    /// Crossfade length in seconds; 0 when off or not possible (exclusive output devices
    /// can't be opened twice).
    pub(super) fn crossfade_len(&self) -> f64 {
        let p = &self.cfg.playback;
        if p.bit_perfect || p.audio_device.starts_with("alsa/hw:") {
            0.0
        } else {
            f64::from(p.crossfade.clamp(0.0, MAX_CROSSFADE))
        }
    }

    /// True if the current track will crossfade into the next one (rather than gapless).
    pub(super) fn will_crossfade(&self) -> bool {
        crossfades_into(
            self.crossfade_len(),
            self.cfg.playback.crossfade_albums,
            self.queue.repeat,
            self.queue.current(),
            self.queue.peek_next(),
        )
    }

    /// Whether the fade timer has anything to do.
    pub(super) fn fade_watch_needed(&self) -> bool {
        self.fading.is_some()
            || self.fade_in.is_some()
            || (self.status == PlayStatus::Playing && self.crossfade_len() > 0.0)
    }

    pub(super) async fn on_fade_tick(&mut self) {
        self.maybe_begin_crossfade().await;
        self.step_fades().await;
    }

    /// Starts the next track when the current one is about to end.
    pub(super) async fn maybe_begin_crossfade(&mut self) {
        if self.fading.is_some()
            || self.fade_in.is_some()
            || self.status != PlayStatus::Playing
            || !self.started_reported
            || self.engine == Engine::None
            || self.duration <= 0.0
            || !self.will_crossfade()
        {
            return;
        }
        let Some(next) = self.queue.peek_next() else { return };
        let len = self.crossfade_len().min(self.duration / 3.0);
        let remaining = self.duration - self.current_position();
        // Start a bit early so the next track has time to load; too close to the end,
        // just let it finish.
        if remaining > len + load_lead(next.source) || remaining < 1.0 {
            return;
        }
        self.begin_crossfade(remaining).await;
    }

    /// Moves the playing track to its own player to fade out and starts the next one silently.
    pub(super) async fn begin_crossfade(&mut self, remaining: f64) {
        let deck = match self.engine {
            Engine::Mpv => match self.mpv.take() {
                Some(mpv) => {
                    // It must not continue into a preloaded file.
                    let _ = mpv.clear_upcoming().await;
                    Deck::Mpv(Box::new(mpv))
                }
                None => return,
            },
            Engine::Spotify => match self.spotify.as_mut().map(SpotifyEngine::detach_current) {
                Some(Ok(deck)) => Deck::Spotify(deck),
                Some(Err(e)) => {
                    tracing::warn!("crossfade: no second Spotify player: {e:#}");
                    return;
                }
                None => return,
            },
            Engine::None => return,
        };
        tracing::debug!("crossfade: {remaining:.1}s left, starting the next track");
        self.mpv_preloaded = None;
        self.engine = Engine::None;
        self.fading = Some(Fading {
            deck,
            ends_at: Instant::now() + Duration::from_secs_f64(remaining),
            ramp: None,
        });
        self.fade_in = Some(FadeIn::Waiting);
        if self.queue.advance(false).is_some() {
            self.load_current(0.0).await;
        } else {
            self.cancel_fades().await;
        }
        self.publish_queue();
    }

    /// The next track is audible: fade the previous one out and this one in.
    pub(super) fn start_fade_ramps(&mut self) {
        let now = Instant::now();
        let max = self.crossfade_len();
        let len = match &mut self.fading {
            Some(f) => {
                let left = f.ends_at.saturating_duration_since(now).as_secs_f64();
                let len = max.min(left).max(MIN_FADE);
                f.ramp = Some((now, len));
                len
            }
            // The previous track already ended: just a short fade-in.
            None => MIN_FADE,
        };
        tracing::debug!("crossfade: fading over {len:.1}s");
        self.fade_in = Some(FadeIn::Ramping { start: now, len });
    }

    pub(super) async fn step_fades(&mut self) {
        let volume = self.cfg.playback.volume;
        let now = Instant::now();
        let mut done = false;
        if let Some(f) = &self.fading {
            match f.ramp {
                Some((start, len)) => {
                    let t = fade_progress(start, len, now);
                    f.deck.set_volume(volume * (1.0 - ease(t))).await;
                    done = t >= 1.0;
                }
                // Nothing took over before the track ran out.
                None => done = now >= f.ends_at + Duration::from_secs(1),
            }
        }
        if done {
            self.finish_fading().await;
        }
        if let Some(FadeIn::Ramping { start, len }) = self.fade_in {
            let t = fade_progress(start, len, now);
            self.set_engine_volume(volume * ease(t)).await;
            if t >= 1.0 {
                self.fade_in = None;
            }
        }
    }

    /// Stops the player that faded out and keeps it for the next crossfade.
    pub(super) async fn finish_fading(&mut self) {
        let Some(f) = self.fading.take() else { return };
        tracing::debug!("crossfade: done");
        match f.deck {
            Deck::Mpv(mpv) => {
                let _ = mpv.stop().await;
                if self.mpv_spare.is_none() {
                    self.mpv_spare = Some(*mpv);
                } else {
                    mpv.quit().await;
                }
            }
            Deck::Spotify(deck) => match self.spotify.as_mut() {
                Some(sp) => sp.return_deck(deck),
                None => deck.stop(),
            },
        }
    }

    /// Ends a crossfade right away: the previous track stops, the current one goes to full volume.
    pub(super) async fn cancel_fades(&mut self) {
        self.finish_fading().await;
        if self.fade_in.take().is_some() {
            self.set_engine_volume(self.cfg.playback.volume).await;
        }
    }

    pub(super) async fn set_engine_volume(&self, volume: f32) {
        match self.engine {
            Engine::Mpv => {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.set_volume(volume).await;
                }
            }
            Engine::Spotify => {
                if let Some(sp) = &self.spotify {
                    sp.set_volume(volume);
                }
            }
            Engine::None => {}
        }
    }

    /// Volume a newly loaded track starts at: silent while it is going to fade in.
    pub(super) fn start_volume(&self) -> f32 {
        if self.fade_in.is_some() {
            0.0
        } else {
            self.cfg.playback.volume
        }
    }

    // ---------------------------------------------------------------- publishing

    pub(super) fn publish_player(&mut self) {
        {
            let mut pv = self.shared.player.write().unwrap();
            let current = self.queue.current().cloned();
            if pv.current.as_ref().map(|t| &t.id) != current.as_ref().map(|t| &t.id) {
                pv.via = None;
            }
            if let (Some(c), Some(p)) = (&current, &self.playing) {
                if c.id != p.id {
                    pv.via = Some(p.clone());
                }
            }
            pv.current = current;
            // A song that will start paused shows as paused while it loads.
            pv.status = if self.status == PlayStatus::Loading && self.pause_wanted {
                PlayStatus::Paused
            } else {
                self.status
            };
            pv.position = self.position;
            pv.position_at = self.position_at;
            pv.duration = self.duration;
            pv.volume = self.cfg.playback.volume;
            pv.shuffle = self.queue.shuffle;
            pv.repeat = self.queue.repeat;
            pv.context = self.queue.context_name.clone();
            pv.context_first = self.queue.context_first().map(|t| t.id.clone());
            pv.quality = self.quality.clone();
            pv.sleep = self.sleep.clone();
        }
        self.mpris.update(
            self.queue.current(),
            self.status == PlayStatus::Playing,
            self.current_position(),
        );
        self.shared.repaint();
    }

    pub(super) fn publish_queue(&mut self) {
        self.session_dirty = true;
        {
            let mut pv = self.shared.player.write().unwrap();
            pv.upcoming = self.queue.upcoming(200);
            pv.up_next_len = self.queue.up_next_len();
        }
        self.shared.repaint();
    }

    pub(super) fn update_presence(&self) {
        let presence = self
            .queue
            .current()
            .filter(|_| self.status != PlayStatus::Stopped)
            .map(|t| {
                let mut track = t.clone();
                // Resolved imports have no art of their own; borrow the playing version's cover.
                if track.art.is_none() {
                    track.art = self.playing.as_ref().and_then(|p| p.art.clone());
                }
                Presence {
                    track,
                    position_secs: self.current_position(),
                    playing: self.status == PlayStatus::Playing,
                }
            });
        self.discord.set(presence);
    }

    pub(super) fn set_account(&self, field: impl FnOnce(&mut Feed) -> &mut AccountStatus, status: AccountStatus) {
        {
            let mut feed = self.shared.feed.write().unwrap();
            *field(&mut feed) = status;
        }
        self.shared.repaint();
    }

    pub(super) fn publish_accounts(&self) {
        let spotify = if self.spotify_auth.has_login() {
            AccountStatus::Connected(self.db.get_kv("spotify_user").unwrap_or_else(|| "Logged in".into()))
        } else {
            AccountStatus::Off
        };
        let lastfm = if !self.cfg.lastfm.session_key.is_empty() {
            AccountStatus::Connected(self.cfg.lastfm.username.clone())
        } else {
            AccountStatus::Off
        };
        let web_api = match &self.spotify_web_auth {
            Some(a) if a.has_user_login() => AccountStatus::Connected(String::new()),
            Some(a) if a.uses_secret() => AccountStatus::Working("Checking your app's Client ID and secret…".into()),
            _ => AccountStatus::Off,
        };
        let mut feed = self.shared.feed.write().unwrap();
        feed.spotify_logged_in = self.spotify_auth.has_login();
        if !matches!(feed.spotify_web_api, AccountStatus::Working(_)) {
            feed.spotify_web_api = web_api;
        }
        if !matches!(feed.spotify, AccountStatus::Working(_)) {
            feed.spotify = spotify;
        }
        if !matches!(feed.lastfm, AccountStatus::Working(_)) {
            feed.lastfm = lastfm;
        }
        drop(feed);
        self.shared.repaint();
    }

    pub(super) fn request_lyrics(&self, track: &Track) {
        {
            let mut feed = self.shared.feed.write().unwrap();
            // Again for the same song only when lyrics were just turned on or off.
            if feed.lyrics.track_id == track.id && feed.lyrics.off != self.cfg.lyrics.enabled {
                return;
            }
            feed.lyrics = LyricsState {
                track_id: track.id.clone(),
                loading: self.cfg.lyrics.enabled,
                lyrics: None,
                off: !self.cfg.lyrics.enabled,
            };
        }
        if !self.cfg.lyrics.enabled {
            return;
        }
        let fetcher = self.lyrics.clone();
        let tx = self.internal_tx.clone();
        let t = track.clone();
        // Imported Apple Music songs: look lyrics up by metadata, which is what LRCLIB needs anyway.
        tokio::spawn(async move {
            let lyrics = fetcher.fetch(&t).await;
            let _ = tx.send(Internal::Lyrics { track_id: t.id, lyrics });
        });
    }
}

/// Fraction (0..=1) of a fade of `len` seconds that started at `start`.
pub(super) fn fade_progress(start: Instant, len: f64, now: Instant) -> f32 {
    if len <= 0.0 {
        return 1.0;
    }
    (now.saturating_duration_since(start).as_secs_f64() / len).clamp(0.0, 1.0) as f32
}

/// Smooth start and end of a fade (smoothstep).
pub(super) fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// How early to start a crossfade so the next track has time to load.
pub(super) fn load_lead(source: Source) -> f64 {
    match source {
        Source::Local => 0.3,
        Source::Spotify => 1.5,
        Source::SoundCloud => 2.0,
        Source::AppleMusic => 2.5,
    }
}

/// Whether `current` should crossfade into `next`. Songs of the same album stay gapless
/// unless `same_album_too`; repeat-one never fades a song into itself.
pub(super) fn crossfades_into(
    len: f64,
    same_album_too: bool,
    repeat: RepeatMode,
    current: Option<&Track>,
    next: Option<&Track>,
) -> bool {
    let (Some(current), Some(next)) = (current, next) else {
        return false;
    };
    len > 0.0 && repeat != RepeatMode::One && (same_album_too || !same_album(current, next))
}

pub(super) fn same_album(a: &Track, b: &Track) -> bool {
    let album = a.album.trim();
    !album.is_empty()
        && album.eq_ignore_ascii_case(b.album.trim())
        && normalize_artist(&a.artist) == normalize_artist(&b.artist)
}
