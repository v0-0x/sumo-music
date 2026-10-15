//! Spotify playback via librespot (Premium required), plus OAuth token handling
//! shared with the Web API client.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_core::SpotifyUri;
use librespot_playback::audio_backend;
use librespot_playback::config::{AudioFormat, Bitrate, PlayerConfig, VolumeCtrl};
use librespot_playback::mixer::{self, Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::SpotifyConfig;

pub const SCOPES: &[&str] = &[
    "streaming",
    "user-read-private",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-private",
    "playlist-modify-public",
    "user-library-read",
    "user-library-modify",
];

#[derive(Serialize, Deserialize)]
struct StoredToken {
    client_id: String,
    refresh_token: String,
}

struct AuthState {
    access: Option<(String, Instant)>,
    refresh: Option<String>,
}

/// OAuth (PKCE) login and token refresh. The refresh token is stored in the data dir.
pub struct SpotifyAuth {
    client_id: String,
    /// The app's Client secret: app tokens without a user login.
    secret: Option<String>,
    file: PathBuf,
    /// Redirect URI registered for `client_id`.
    redirect: String,
    /// The login in progress, aborted (freeing its port) when a new one starts.
    login_task: Mutex<Option<tokio::task::AbortHandle>>,
    state: Mutex<AuthState>,
    /// Serializes refreshes so parallel requests don't refresh twice.
    refresh_lock: tokio::sync::Mutex<()>,
}

impl SpotifyAuth {
    /// The main login (playback and library import).
    pub fn new(cfg: &SpotifyConfig, dir: &Path) -> SpotifyAuth {
        let redirect = super::oauth::redirect_uri(cfg.redirect_port);
        Self::with_client(&cfg.client_id, &redirect, dir, "oauth.json")
    }

    /// Login with the user's own developer app, used only for Web API calls.
    pub fn web_api(cfg: &SpotifyConfig, dir: &Path) -> Option<SpotifyAuth> {
        let id = cfg.web_api_client_id.trim();
        let secret = cfg.web_api_client_secret.trim();
        (!id.is_empty()).then(|| {
            let mut auth = Self::with_client(id, &cfg.web_api_redirect(), dir, "oauth-webapi.json");
            auth.secret = (!secret.is_empty()).then(|| secret.to_string());
            auth
        })
    }

    pub fn with_client(client_id: &str, redirect: &str, dir: &Path, file_name: &str) -> SpotifyAuth {
        let _ = std::fs::create_dir_all(dir);
        let file = dir.join(file_name);
        let refresh = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str::<StoredToken>(&s).ok())
            .filter(|t| t.client_id == client_id)
            .map(|t| t.refresh_token);
        SpotifyAuth {
            client_id: client_id.to_string(),
            secret: None,
            redirect: redirect.to_string(),
            file,
            login_task: Mutex::new(None),
            state: Mutex::new(AuthState { access: None, refresh }),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// A user login, or an app token from the Client secret.
    pub fn has_login(&self) -> bool {
        self.has_user_login() || self.secret.is_some()
    }

    /// Logged in as a user (needed for anything in their library).
    pub fn has_user_login(&self) -> bool {
        self.state.lock().unwrap().refresh.is_some()
    }

    pub fn uses_secret(&self) -> bool {
        self.secret.is_some()
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The Redirect URI sent to Spotify, exactly.
    pub fn redirect(&self) -> &str {
        &self.redirect
    }

    /// Opens the browser for the Spotify login page and waits for the redirect.
    /// Starting a new login cancels a previous one that is still waiting.
    pub async fn login(&self) -> Result<String> {
        let (client_id, redirect) = (self.client_id.clone(), self.redirect.clone());
        let task = tokio::spawn(async move { super::oauth::login(&client_id, &redirect, SCOPES).await });
        let previous = self.login_task.lock().unwrap().replace(task.abort_handle());
        if let Some(old) = previous {
            old.abort();
            // Give the aborted task a moment to drop its listener.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let token = match task.await {
            Ok(r) => r.map_err(|e| anyhow!("Spotify login failed: {e:#}"))?,
            Err(e) if e.is_cancelled() => return Err(anyhow!("Spotify login cancelled")),
            Err(e) => return Err(anyhow!("Spotify login failed: {e}")),
        };
        self.store(&token);
        Ok(token.access_token)
    }

    fn store(&self, token: &super::oauth::Token) {
        let mut st = self.state.lock().unwrap();
        st.access = Some((token.access_token.clone(), token.expires_at));
        if let Some(refresh) = token.refresh_token.clone() {
            st.refresh = Some(refresh.clone());
            let stored = StoredToken {
                client_id: self.client_id.clone(),
                refresh_token: refresh,
            };
            if let Ok(text) = serde_json::to_string(&stored) {
                let _ = crate::config::write_private(&self.file, text);
            }
        }
    }

    /// A valid access token, refreshing it when it is about to expire.
    pub async fn token(&self) -> Result<String> {
        if let Some(t) = self.cached_access() {
            return Ok(t);
        }
        let _guard = self.refresh_lock.lock().await;
        if let Some(t) = self.cached_access() {
            return Ok(t);
        }
        let refresh = self.state.lock().unwrap().refresh.clone();
        let Some(refresh) = refresh else {
            let secret = self
                .secret
                .as_ref()
                .ok_or_else(|| anyhow!("not logged in to Spotify"))?;
            let token = super::oauth::client_credentials(&self.client_id, secret).await?;
            self.state.lock().unwrap().access = Some((token.access_token.clone(), token.expires_at));
            return Ok(token.access_token);
        };
        let token = super::oauth::refresh(&self.client_id, &refresh)
            .await
            .map_err(|e| anyhow!("Spotify token refresh failed: {e:#}"))?;
        self.store(&token);
        Ok(token.access_token)
    }

    fn cached_access(&self) -> Option<String> {
        let st = self.state.lock().unwrap();
        st.access
            .as_ref()
            .filter(|(_, exp)| *exp > Instant::now() + Duration::from_secs(60))
            .map(|(t, _)| t.clone())
    }

    /// Forces the next `token()` call to refresh (e.g. after a 401).
    pub fn invalidate(&self) {
        self.state.lock().unwrap().access = None;
    }

    pub fn logout(&self) {
        let mut st = self.state.lock().unwrap();
        st.access = None;
        st.refresh = None;
        let _ = std::fs::remove_file(&self.file);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpotifyEvent {
    Loading,
    Playing {
        position_ms: u32,
    },
    Paused {
        position_ms: u32,
    },
    Seeked {
        position_ms: u32,
    },
    /// Good moment to preload the next track for gapless playback.
    TimeToPreload,
    /// `uri` is the track that ended ("spotify:track:…").
    EndOfTrack {
        uri: String,
    },
    /// The track can't be played. It may be the next one, failing while it was preloaded.
    Unavailable {
        uri: String,
    },
    Stopped,
}

/// Events from one Spotify player, tagged with its [`SpotifyDeck::id`].
pub type SpotifySender = mpsc::UnboundedSender<(u64, SpotifyEvent)>;

/// One librespot player with its own volume. Crossfades play two at once on the same session.
pub struct SpotifyDeck {
    pub id: u64,
    player: Arc<Player>,
    mixer: Arc<dyn Mixer>,
}

impl SpotifyDeck {
    pub fn load(&self, uri: &str, start_ms: u32) -> Result<()> {
        let uri = SpotifyUri::from_uri(uri).map_err(|e| anyhow!("bad Spotify URI {uri}: {e}"))?;
        self.player.load(uri, true, start_ms);
        Ok(())
    }

    pub fn stop(&self) {
        self.player.stop();
    }

    pub fn set_volume(&self, volume: f32) {
        self.mixer.set_volume(volume_to_u16(volume));
    }
}

pub struct SpotifyEngine {
    session: Session,
    player_config: PlayerConfig,
    backend: audio_backend::SinkBuilder,
    events: SpotifySender,
    next_deck: u64,
    /// The player for the current track.
    current: SpotifyDeck,
    /// An idle player kept for the next crossfade.
    spare: Option<SpotifyDeck>,
}

impl SpotifyEngine {
    pub async fn connect(
        auth: &SpotifyAuth,
        dir: &Path,
        audio_cache: Option<PathBuf>,
        cfg: &SpotifyConfig,
        volume: f32,
        events: SpotifySender,
    ) -> Result<SpotifyEngine> {
        let cache = Cache::new(Some(dir), Some(dir), audio_cache.as_deref(), Some(2 << 30))
            .context("creating Spotify cache")?;
        let session = new_session(auth, &cache).await?;

        let (backend_name, backend) = select_backend(&cfg.audio_output)?;
        tracing::info!("Spotify audio output: {backend_name}");

        let player_config = PlayerConfig {
            bitrate: match cfg.bitrate {
                0..=96 => Bitrate::Bitrate96,
                97..=160 => Bitrate::Bitrate160,
                _ => Bitrate::Bitrate320,
            },
            normalisation: cfg.normalisation,
            gapless: true,
            ..PlayerConfig::default()
        };
        let current = new_deck(1, &session, &player_config, backend, &events)?;
        current.set_volume(volume);
        Ok(SpotifyEngine {
            session,
            player_config,
            backend,
            events,
            next_deck: 2,
            current,
            spare: None,
        })
    }

    /// Id of the player for the current track (events from other players are stale).
    pub fn current_id(&self) -> u64 {
        self.current.id
    }

    /// Hands out the current player (to fade out) and switches to another one.
    pub fn detach_current(&mut self) -> Result<SpotifyDeck> {
        let next = match self.spare.take() {
            Some(deck) => deck,
            None => {
                let id = self.next_deck;
                self.next_deck += 1;
                new_deck(id, &self.session, &self.player_config, self.backend, &self.events)?
            }
        };
        Ok(std::mem::replace(&mut self.current, next))
    }

    /// Takes back a player that finished fading out.
    pub fn return_deck(&mut self, deck: SpotifyDeck) {
        deck.stop();
        if self.spare.is_none() {
            self.spare = Some(deck);
        }
    }

    pub fn load(&self, uri: &str, start_ms: u32) -> Result<()> {
        self.current.load(uri, start_ms)
    }

    pub fn preload(&self, uri: &str) {
        if let Ok(uri) = SpotifyUri::from_uri(uri) {
            self.current.player.preload(uri);
        }
    }

    pub fn play(&self) {
        self.current.player.play();
    }

    pub fn pause(&self) {
        self.current.player.pause();
    }

    pub fn stop(&self) {
        self.current.stop();
    }

    pub fn seek(&self, secs: f64) {
        self.current.player.seek((secs.max(0.0) * 1000.0) as u32);
    }

    pub fn set_volume(&self, volume: f32) {
        self.current.set_volume(volume);
    }

    pub fn shutdown(&self) {
        self.current.stop();
        if let Some(spare) = &self.spare {
            spare.stop();
        }
        self.session.shutdown();
    }

    /// The live session, if it's still connected (used for library import).
    pub fn session(&self) -> Option<Session> {
        (!self.session.is_invalid()).then(|| self.session.clone())
    }
}

/// A librespot player with its own soft volume, forwarding its events tagged with `id`.
fn new_deck(
    id: u64,
    session: &Session,
    config: &PlayerConfig,
    backend: audio_backend::SinkBuilder,
    events: &SpotifySender,
) -> Result<SpotifyDeck> {
    let mixer = new_mixer()?;
    let player = Player::new(config.clone(), session.clone(), mixer.get_soft_volume(), move || {
        backend(None, AudioFormat::default())
    });
    let mut rx = player.get_player_event_channel();
    let events = events.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let Some(mapped) = map_event(ev) else { continue };
            if events.send((id, mapped)).is_err() {
                break;
            }
        }
    });
    Ok(SpotifyDeck { id, player, mixer })
}

/// The player events Sumo acts on.
fn map_event(ev: PlayerEvent) -> Option<SpotifyEvent> {
    Some(match ev {
        PlayerEvent::Loading { .. } => SpotifyEvent::Loading,
        PlayerEvent::Playing { position_ms, .. } => SpotifyEvent::Playing { position_ms },
        PlayerEvent::Paused { position_ms, .. } => SpotifyEvent::Paused { position_ms },
        PlayerEvent::Seeked { position_ms, .. } | PlayerEvent::PositionCorrection { position_ms, .. } => {
            SpotifyEvent::Seeked { position_ms }
        }
        PlayerEvent::TimeToPreloadNextTrack { .. } => SpotifyEvent::TimeToPreload,
        PlayerEvent::EndOfTrack { track_id, .. } => SpotifyEvent::EndOfTrack {
            uri: track_id.to_uri().unwrap_or_default(),
        },
        PlayerEvent::Unavailable { track_id, .. } => SpotifyEvent::Unavailable {
            uri: track_id.to_uri().unwrap_or_default(),
        },
        PlayerEvent::Stopped { .. } => SpotifyEvent::Stopped,
        _ => return None,
    })
}

/// A software volume control for one player, silent until it is given a volume.
fn new_mixer() -> Result<Arc<dyn Mixer>> {
    // `volume_to_u16` already applies mpv's volume curve, so the mixer must not add its own
    // (librespot's default is a 60 dB log curve, much quieter than mpv at the same setting).
    let config = MixerConfig {
        volume_ctrl: VolumeCtrl::Linear,
        ..MixerConfig::default()
    };
    let mixer = mixer::find(None).ok_or_else(|| anyhow!("no mixer"))?(config).map_err(|e| anyhow!("mixer: {e}"))?;
    // librespot starts a mixer at half volume, which is loud for a player nobody set yet.
    mixer.set_volume(0);
    Ok(mixer)
}

/// Picks librespot's audio output. PipeWire desktops answer on the PulseAudio socket
/// (pipewire-pulse), which follows the system's default output device; plain ALSA may
/// point at another card (e.g. HDMI) when pipewire-alsa isn't installed.
fn select_backend(pref: &str) -> Result<(&'static str, audio_backend::SinkBuilder)> {
    let order: &[&'static str] = match pref {
        "alsa" => &["rodio", "pulseaudio"],
        "pulseaudio" => &["pulseaudio", "rodio"],
        _ if pulse_server_available() => &["pulseaudio", "rodio"],
        _ => &["rodio", "pulseaudio"],
    };
    order
        .iter()
        .find_map(|name| audio_backend::find(Some((*name).to_string())).map(|b| (*name, b)))
        .or_else(|| audio_backend::find(None).map(|b| ("default", b)))
        .ok_or_else(|| anyhow!("no audio output compiled in"))
}

/// True if a PulseAudio-compatible server (PulseAudio or pipewire-pulse) is listening.
pub fn pulse_server_available() -> bool {
    if std::env::var_os("PULSE_SERVER").is_some() {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| Path::new(&d).join("pulse").join("native").exists())
        .unwrap_or(false)
}

async fn new_session(auth: &SpotifyAuth, cache: &Cache) -> Result<Session> {
    // Reusable credentials from a previous login avoid an OAuth round trip.
    if let Some(creds) = cache.credentials() {
        let session = Session::new(SessionConfig::default(), Some(cache.clone()));
        match session.connect(creds, true).await {
            Ok(()) => return Ok(session),
            Err(e) => tracing::warn!("stored Spotify credentials rejected: {e}"),
        }
    }
    let token = auth.token().await?;
    let session = Session::new(SessionConfig::default(), Some(cache.clone()));
    session
        .connect(Credentials::with_access_token(token), true)
        .await
        .map_err(|e| anyhow!("Spotify connection failed: {e}"))?;
    Ok(session)
}

/// The volume slider (0-100) as a librespot volume, on the same cubic curve as mpv's volume
/// so a song is as loud on Spotify as on SoundCloud or from disk at the same setting.
fn volume_to_u16(volume: f32) -> u16 {
    let gain = (volume.clamp(0.0, 100.0) / 100.0).powi(3);
    (gain * u16::MAX as f32).round() as u16
}

/// Removes stored librespot credentials (used on logout).
pub fn clear_credentials(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("credentials.json"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_playback::convert::Converter;
    use librespot_playback::decoder::AudioPacket;

    /// End and failure events say which song they are about: a failing *next* song (being
    /// preloaded) must not stop the current one.
    #[test]
    fn events_name_their_song() {
        let id = || SpotifyUri::from_uri("spotify:track:4uLU6hMCjMI75M1A2tKUQC").unwrap();
        assert_eq!(
            map_event(PlayerEvent::Unavailable {
                play_request_id: 2,
                track_id: id(),
            }),
            Some(SpotifyEvent::Unavailable {
                uri: "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into()
            })
        );
        assert_eq!(
            map_event(PlayerEvent::EndOfTrack {
                play_request_id: 1,
                track_id: id(),
            }),
            Some(SpotifyEvent::EndOfTrack {
                uri: "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into()
            })
        );
        assert_eq!(
            map_event(PlayerEvent::SessionDisconnected {
                connection_id: String::new(),
                user_name: String::new()
            }),
            None
        );
    }

    #[test]
    fn volume_follows_mpvs_curve() {
        let mixer = new_mixer().unwrap();
        let gain = mixer.get_soft_volume();
        assert_eq!(gain.attenuation_factor(), 0.0);
        // mpv's volume is cubic: 50% is 1/8 of full scale, 100% is unchanged.
        for (volume, expected) in [(0.0, 0.0), (20.0, 0.008), (50.0, 0.125), (70.0, 0.343), (100.0, 1.0)] {
            mixer.set_volume(volume_to_u16(volume));
            let factor = gain.attenuation_factor();
            assert!(
                (factor - expected).abs() < 1e-4,
                "{volume}%: {factor} instead of {expected}"
            );
        }
        assert_eq!(volume_to_u16(150.0), u16::MAX);
        assert_eq!(volume_to_u16(-5.0), 0);
    }

    #[test]
    fn explicit_output_choice_wins() {
        assert_eq!(select_backend("alsa").unwrap().0, "rodio");
        #[cfg(feature = "pulseaudio")]
        assert_eq!(select_backend("pulseaudio").unwrap().0, "pulseaudio");
    }

    /// Plays half a second of a tone through the PulseAudio/PipeWire output if a server runs.
    /// Audible, so it only runs on request: `cargo test -- --ignored`.
    #[cfg(feature = "pulseaudio")]
    #[test]
    #[ignore = "plays a tone through the speakers"]
    fn pulse_output_plays_when_server_present() {
        if !pulse_server_available() {
            return;
        }
        let (name, builder) = select_backend("auto").unwrap();
        assert_eq!(name, "pulseaudio");
        let mut sink = builder(None, AudioFormat::default());
        let mut converter = Converter::new(None);
        sink.start().unwrap();
        let samples: Vec<f64> = (0..44_100)
            .map(|i| ((i / 2) as f64 * 440.0 * std::f64::consts::TAU / 44_100.0).sin() * 0.1)
            .collect();
        sink.write(AudioPacket::Samples(samples), &mut converter).unwrap();
        sink.stop().unwrap();
    }
}
