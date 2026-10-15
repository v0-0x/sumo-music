//! Discord Rich Presence ("Listening to ...") over Discord's local IPC socket.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use discord_rich_presence::activity::{Activity, ActivityType, Assets, Button, StatusDisplayType, Timestamps};
use discord_rich_presence::{DiscordIpc, DiscordIpcClient};

use crate::model::{Source, Track};

#[derive(Debug, Clone, PartialEq)]
pub struct Presence {
    pub track: Track,
    pub position_secs: f64,
    pub playing: bool,
}

enum Msg {
    Configure {
        enabled: bool,
        app_id: String,
        song_as_name: bool,
    },
    Set(Option<Presence>),
    Shutdown,
}

/// Handle to the presence thread. Cheap to use from any thread.
#[derive(Clone)]
pub struct Discord {
    tx: mpsc::Sender<Msg>,
}

impl Discord {
    pub fn spawn(enabled: bool, app_id: &str, song_as_name: bool) -> Discord {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Msg::Configure {
            enabled,
            app_id: app_id.to_string(),
            song_as_name,
        });
        std::thread::Builder::new()
            .name("discord-rpc".into())
            .spawn(move || run(rx))
            .expect("spawn discord thread");
        Discord { tx }
    }

    pub fn configure(&self, enabled: bool, app_id: &str, song_as_name: bool) {
        let _ = self.tx.send(Msg::Configure {
            enabled,
            app_id: app_id.to_string(),
            song_as_name,
        });
    }

    pub fn set(&self, presence: Option<Presence>) {
        let _ = self.tx.send(Msg::Set(presence));
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Msg::Shutdown);
    }
}

fn run(rx: mpsc::Receiver<Msg>) {
    let mut enabled = false;
    let mut app_id = String::new();
    let mut song_as_name = true;
    let mut client: Option<DiscordIpcClient> = None;
    let mut wanted: Option<Presence> = None;
    let mut dirty = true;
    let mut last_connect_try: Option<Instant> = None;
    let mut last_send = Instant::now() - Duration::from_secs(60);

    loop {
        // Discord rate limits presence updates (~5 per 20s); coalesce bursts.
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Msg::Configure {
                enabled: e,
                app_id: id,
                song_as_name: s,
            }) => {
                if id != app_id || !e {
                    if let Some(mut c) = client.take() {
                        let _ = c.clear_activity();
                        let _ = c.close();
                    }
                    last_connect_try = None;
                }
                enabled = e;
                app_id = id;
                song_as_name = s;
                dirty = true;
            }
            Ok(Msg::Set(p)) => {
                if p != wanted {
                    wanted = p;
                    dirty = true;
                }
            }
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        // Drain queued updates so only the newest state is sent.
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Set(p) => {
                    if p != wanted {
                        wanted = p;
                        dirty = true;
                    }
                }
                Msg::Configure {
                    enabled: e,
                    app_id: id,
                    song_as_name: s,
                } => {
                    // As above: turning it off (or another app) clears what Discord shows.
                    if id != app_id || !e {
                        if let Some(mut c) = client.take() {
                            let _ = c.clear_activity();
                            let _ = c.close();
                        }
                        last_connect_try = None;
                    }
                    enabled = e;
                    app_id = id;
                    song_as_name = s;
                    dirty = true;
                }
                Msg::Shutdown => {
                    if let Some(mut c) = client.take() {
                        let _ = c.clear_activity();
                        let _ = c.close();
                    }
                    return;
                }
            }
        }

        if !enabled || app_id.trim().is_empty() {
            continue;
        }
        if client.is_none() {
            // Discord might not be running yet; retry every 15 seconds.
            if last_connect_try.is_some_and(|t| t.elapsed() < Duration::from_secs(15)) {
                continue;
            }
            last_connect_try = Some(Instant::now());
            let mut c = DiscordIpcClient::new(app_id.trim());
            if c.connect().is_ok() {
                tracing::info!("connected to Discord");
                client = Some(c);
                dirty = true;
            } else {
                continue;
            }
        }
        if !dirty || last_send.elapsed() < Duration::from_secs(2) {
            continue;
        }
        let Some(c) = client.as_mut() else { continue };
        let result = match &wanted {
            Some(p) => c.set_activity(build_activity(p, song_as_name)),
            None => c.clear_activity(),
        };
        last_send = Instant::now();
        match result {
            Ok(()) => dirty = false,
            Err(e) => {
                tracing::debug!("Discord update failed: {e}; reconnecting later");
                let _ = c.close();
                client = None;
            }
        }
    }
    if let Some(mut c) = client {
        let _ = c.clear_activity();
        let _ = c.close();
    }
}

fn build_activity(p: &Presence, song_as_name: bool) -> Activity<'_> {
    let t = &p.track;
    let mut activity = Activity::new()
        .activity_type(ActivityType::Listening)
        .details(clamp(&t.title))
        .state(clamp(&format!("by {}", t.artist)))
        .status_display_type(StatusDisplayType::Details);
    if song_as_name {
        activity = activity.name(clamp(&t.title));
    }

    let mut assets = Assets::new().large_text(clamp(if t.album.is_empty() { &t.title } else { &t.album }));
    // Discord can show external images given as https URLs.
    if let Some(art) = t.art.as_deref().filter(|a| a.starts_with("https://")) {
        assets = assets.large_image(art.to_string());
    }
    assets = assets.small_text(format!("{} · Sumo", t.source.label()));
    activity = activity.assets(assets);

    if p.playing {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let start = now_ms - (p.position_secs * 1000.0) as i64;
        let mut ts = Timestamps::new().start(start);
        if t.duration_ms > 0 {
            ts = ts.end(start + t.duration_ms as i64);
        }
        activity = activity.timestamps(ts);
    }

    let link = match t.source {
        Source::Spotify => {
            t.id.strip_prefix("spotify:track:")
                .map(|id| format!("https://open.spotify.com/track/{id}"))
        }
        Source::SoundCloud if t.uri.starts_with("https://") => Some(t.uri.clone()),
        _ => None,
    };
    if let Some(url) = link {
        let label = format!("Open in {}", t.source.label());
        activity = activity.buttons(vec![Button::new(label, url)]);
    }
    activity
}

/// Discord rejects fields shorter than 2 or longer than 128 characters.
fn clamp(s: &str) -> String {
    let mut out: String = s.chars().take(128).collect();
    while out.chars().count() < 2 {
        out.push(' ');
    }
    out
}
