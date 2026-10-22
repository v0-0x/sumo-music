//! System media controls: MPRIS on Linux (media keys, playerctl, waybar/polybar modules,
//! desktop widgets), the media overlay and keys on Windows, Now Playing on macOS.

pub use os::Mpris;

/// Windows and macOS: souvlaki talks to the system.
#[cfg(any(windows, target_os = "macos"))]
mod os {
    use std::time::Duration;

    use souvlaki::{
        MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection,
    };
    use tokio::sync::mpsc::UnboundedSender;

    use super::cover_url;
    use crate::model::Track;
    use crate::service::Command;

    pub struct Mpris {
        controls: Option<MediaControls>,
        last_track: Option<String>,
    }

    impl Mpris {
        /// `window` is the main window's handle, which Windows needs (it has none before the
        /// window opens).
        pub fn new(commands: UnboundedSender<Command>, window: Option<isize>) -> Mpris {
            let config = PlatformConfig {
                display_name: "Sumo",
                dbus_name: "multimusic",
                hwnd: window.map(|h| h as *mut std::ffi::c_void),
            };
            let available = cfg!(not(windows)) || window.is_some();
            let controls = available
                .then(|| MediaControls::new(config).ok())
                .flatten()
                .and_then(|mut c| {
                    let result = c.attach(move |event: MediaControlEvent| {
                        let cmd = match event {
                            MediaControlEvent::Play => Command::Resume,
                            MediaControlEvent::Pause => Command::Pause,
                            MediaControlEvent::Toggle => Command::TogglePause,
                            MediaControlEvent::Next => Command::Next,
                            MediaControlEvent::Previous => Command::Previous,
                            MediaControlEvent::Stop => Command::Pause,
                            MediaControlEvent::Seek(dir) => Command::SeekRelative(match dir {
                                SeekDirection::Forward => 10.0,
                                SeekDirection::Backward => -10.0,
                            }),
                            MediaControlEvent::SeekBy(dir, by) => Command::SeekRelative(match dir {
                                SeekDirection::Forward => by.as_secs_f64(),
                                SeekDirection::Backward => -by.as_secs_f64(),
                            }),
                            MediaControlEvent::SetPosition(MediaPosition(pos)) => Command::Seek(pos.as_secs_f64()),
                            MediaControlEvent::SetVolume(v) => Command::SetVolume((v * 100.0) as f32),
                            MediaControlEvent::Raise => Command::Raise,
                            MediaControlEvent::Quit => Command::Quit,
                            MediaControlEvent::OpenUri(_) => return,
                        };
                        let _ = commands.send(cmd);
                    });
                    match result {
                        Ok(()) => Some(c),
                        Err(e) => {
                            tracing::warn!("media controls unavailable: {e:?}");
                            None
                        }
                    }
                });
            Mpris {
                controls,
                last_track: None,
            }
        }

        pub fn update(&mut self, track: Option<&Track>, playing: bool, position: f64) {
            let Some(c) = self.controls.as_mut() else { return };
            let id = track.map(|t| t.id.clone());
            if id != self.last_track {
                self.last_track = id;
                let cover = track.and_then(cover_url);
                let meta = match track {
                    Some(t) => MediaMetadata {
                        title: Some(&t.title),
                        album: Some(&t.album),
                        artist: Some(&t.artist),
                        cover_url: cover.as_deref(),
                        duration: (t.duration_ms > 0).then(|| Duration::from_millis(t.duration_ms)),
                    },
                    None => MediaMetadata::default(),
                };
                let _ = c.set_metadata(meta);
            }
            let progress = Some(MediaPosition(Duration::from_secs_f64(position.max(0.0))));
            let playback = match (track, playing) {
                (None, _) => MediaPlayback::Stopped,
                (Some(_), true) => MediaPlayback::Playing { progress },
                (Some(_), false) => MediaPlayback::Paused { progress },
            };
            let _ = c.set_playback(playback);
        }

        /// The overlays have no volume.
        pub fn set_volume(&mut self, _volume: f32) {}
    }
}

/// Linux: MPRIS over D-Bus, served on the app's own runtime.
#[cfg(not(any(windows, target_os = "macos")))]
mod os {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Instant;

    use tokio::sync::mpsc::UnboundedSender;
    use zbus::object_server::SignalEmitter;
    use zbus::zvariant::{ObjectPath, OwnedValue, Value};

    use super::cover_url;
    use crate::model::Track;
    use crate::service::Command;

    const PATH: &str = "/org/mpris/MediaPlayer2";

    /// What the desktop asks about.
    struct State {
        track: Option<Track>,
        playing: bool,
        /// Seconds into the song at `at`.
        position: f64,
        at: Instant,
        /// 0.0..=1.0
        volume: f64,
    }

    impl State {
        fn position_now(&self) -> f64 {
            if self.playing {
                self.position + self.at.elapsed().as_secs_f64()
            } else {
                self.position
            }
        }
    }

    type Shared = Arc<Mutex<State>>;

    fn lock(state: &Shared) -> std::sync::MutexGuard<'_, State> {
        state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub struct Mpris {
        state: Shared,
        conn: Arc<OnceLock<zbus::Connection>>,
        last_track: Option<String>,
        last_playing: bool,
    }

    impl Mpris {
        /// Must be called on the runtime, which serves the requests. `_window` is for Windows.
        pub fn new(commands: UnboundedSender<Command>, _window: Option<isize>) -> Mpris {
            let state = Arc::new(Mutex::new(State {
                track: None,
                playing: false,
                position: 0.0,
                at: Instant::now(),
                volume: 1.0,
            }));
            let conn = Arc::new(OnceLock::new());
            let (slot, shared) = (conn.clone(), state.clone());
            tokio::spawn(async move {
                let built = async {
                    zbus::connection::Builder::session()?
                        .name("org.mpris.MediaPlayer2.multimusic")?
                        .serve_at(
                            PATH,
                            Root {
                                commands: commands.clone(),
                            },
                        )?
                        .serve_at(
                            PATH,
                            Player {
                                commands,
                                state: shared,
                            },
                        )?
                        .build()
                        .await
                };
                match built.await {
                    Ok(c) => {
                        let _ = slot.set(c);
                    }
                    Err(e) => tracing::warn!("MPRIS unavailable: {e}"),
                }
            });
            Mpris {
                state,
                conn,
                last_track: None,
                last_playing: false,
            }
        }

        pub fn update(&mut self, track: Option<&Track>, playing: bool, position: f64) {
            let id = track.map(|t| t.id.clone());
            let new_track = id != self.last_track;
            let new_status = new_track || playing != self.last_playing;
            // A jump within the song (a seek) is announced; players that show a progress bar
            // only ask for the position again then.
            let seeked = {
                let mut s = lock(&self.state);
                let jumped = !new_track && (s.position_now() - position).abs() > 1.5;
                s.track = track.cloned();
                s.playing = playing;
                s.position = position.max(0.0);
                s.at = Instant::now();
                jumped
            };
            self.last_track = id;
            self.last_playing = playing;
            if new_track || new_status || seeked {
                let micros = (position.max(0.0) * 1e6) as i64;
                self.emit(move |player, emitter| {
                    Box::pin(async move {
                        if new_track {
                            if let Err(e) = player.metadata_changed(emitter).await {
                                tracing::debug!("MPRIS metadata: {e}");
                            }
                        }
                        if new_status {
                            let _ = player.playback_status_changed(emitter).await;
                        }
                        if seeked {
                            let _ = Player::seeked(emitter, micros).await;
                        }
                    })
                });
            }
        }

        pub fn set_volume(&mut self, volume: f32) {
            let v = f64::from(volume / 100.0).clamp(0.0, 1.0);
            {
                let mut s = lock(&self.state);
                if (s.volume - v).abs() < 1e-6 {
                    return;
                }
                s.volume = v;
            }
            self.emit(|player, emitter| {
                Box::pin(async move {
                    let _ = player.volume_changed(emitter).await;
                })
            });
        }

        /// Tells listeners what changed (once connected; before that nobody listens).
        fn emit<F>(&self, f: F)
        where
            F: for<'a> FnOnce(
                    &'a Player,
                    &'a SignalEmitter<'static>,
                ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>
                + Send
                + 'static,
        {
            let Some(conn) = self.conn.get().cloned() else {
                tracing::debug!("MPRIS: not connected yet");
                return;
            };
            tokio::spawn(async move {
                let Ok(iface) = conn.object_server().interface::<_, Player>(PATH).await else {
                    return;
                };
                let player = iface.get().await;
                f(&player, iface.signal_emitter()).await;
            });
        }
    }

    struct Root {
        commands: UnboundedSender<Command>,
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl Root {
        fn raise(&self) {
            let _ = self.commands.send(Command::Raise);
        }

        fn quit(&self) {
            let _ = self.commands.send(Command::Quit);
        }

        #[zbus(property)]
        fn can_quit(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_raise(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn has_track_list(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn identity(&self) -> String {
            "Sumo".into()
        }

        #[zbus(property)]
        fn desktop_entry(&self) -> String {
            "multimusic".into()
        }

        #[zbus(property)]
        fn supported_uri_schemes(&self) -> Vec<String> {
            Vec::new()
        }

        #[zbus(property)]
        fn supported_mime_types(&self) -> Vec<String> {
            Vec::new()
        }
    }

    struct Player {
        commands: UnboundedSender<Command>,
        state: Shared,
    }

    impl Player {
        fn send(&self, cmd: Command) {
            let _ = self.commands.send(cmd);
        }
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl Player {
        fn next(&self) {
            self.send(Command::Next);
        }

        fn previous(&self) {
            self.send(Command::Previous);
        }

        fn pause(&self) {
            self.send(Command::Pause);
        }

        fn play_pause(&self) {
            self.send(Command::TogglePause);
        }

        fn stop(&self) {
            self.send(Command::Pause);
        }

        fn play(&self) {
            self.send(Command::Resume);
        }

        /// `offset` in microseconds.
        fn seek(&self, offset: i64) {
            self.send(Command::SeekRelative(offset as f64 / 1e6));
        }

        fn set_position(&self, _track_id: ObjectPath<'_>, position: i64) {
            self.send(Command::Seek(position.max(0) as f64 / 1e6));
        }

        fn open_uri(&self, _uri: String) {}

        #[zbus(signal)]
        async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

        #[zbus(property)]
        fn playback_status(&self) -> String {
            let s = lock(&self.state);
            match (&s.track, s.playing) {
                (None, _) => "Stopped",
                (Some(_), true) => "Playing",
                (Some(_), false) => "Paused",
            }
            .into()
        }

        #[zbus(property)]
        fn metadata(&self) -> HashMap<String, OwnedValue> {
            let s = lock(&self.state);
            s.track.as_ref().map(metadata).unwrap_or_default()
        }

        #[zbus(property)]
        fn volume(&self) -> f64 {
            lock(&self.state).volume
        }

        #[zbus(property)]
        fn set_volume(&mut self, volume: f64) {
            self.send(Command::SetVolume((volume.clamp(0.0, 1.0) * 100.0) as f32));
        }

        /// Microseconds into the song; asked for, never announced.
        #[zbus(property(emits_changed_signal = "false"))]
        fn position(&self) -> i64 {
            (lock(&self.state).position_now() * 1e6) as i64
        }

        #[zbus(property)]
        fn rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn minimum_rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn maximum_rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn can_go_next(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_go_previous(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_play(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_pause(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_seek(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_control(&self) -> bool {
            true
        }
    }

    /// The song as MPRIS describes one.
    fn metadata(t: &Track) -> HashMap<String, OwnedValue> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        t.id.hash(&mut h);
        let mut m: HashMap<String, OwnedValue> = HashMap::new();
        let mut put = |key: &str, value: Value<'_>| {
            if let Ok(v) = value.try_to_owned() {
                m.insert(key.into(), v);
            }
        };
        let id = format!("{PATH}/Track/{:016x}", h.finish());
        if let Ok(path) = ObjectPath::try_from(id.as_str()) {
            put("mpris:trackid", Value::from(path));
        }
        put("xesam:title", Value::from(t.title.as_str()));
        put("xesam:artist", Value::from(vec![t.artist.as_str()]));
        put("xesam:album", Value::from(t.album.as_str()));
        if !t.album_artist.is_empty() {
            put("xesam:albumArtist", Value::from(vec![t.album_artist.as_str()]));
        }
        if t.duration_ms > 0 {
            put("mpris:length", Value::from(t.duration_ms as i64 * 1000));
        }
        if let Some(n) = t.track_no {
            put("xesam:trackNumber", Value::from(n as i32));
        }
        if let Some(art) = cover_url(t) {
            put("mpris:artUrl", Value::from(art));
        }
        m
    }
}

/// The cover as a link media controls can show: a web link, or a local image as `file://`.
fn cover_url(t: &crate::model::Track) -> Option<String> {
    let a = t.art.as_deref()?;
    if a.starts_with("http") {
        Some(a.to_string())
    } else if is_image(a) {
        Some(file_url(a))
    } else {
        None
    }
}

/// A local cover as a `file://` URL (Windows paths need a slash before the drive).
fn file_url(path: &str) -> String {
    let path = path.replace('\\', "/");
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

fn is_image(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".webp"].iter().any(|e| lower.ends_with(e))
}
