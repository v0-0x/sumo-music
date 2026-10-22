//! User configuration (`~/.config/multimusic/config.toml`) and XDG paths.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::integrations::lastfm_stats::{ActivityRange, Period};

/// Spotify's own desktop client id. librespot uses it for streaming, and it is
/// allowed to request the library/playlist scopes too, so no developer app is needed.
pub const SPOTIFY_DEFAULT_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub library: LibraryConfig,
    pub playback: PlaybackConfig,
    pub spotify: SpotifyConfig,
    pub soundcloud: SoundCloudConfig,
    pub apple_music: AppleMusicConfig,
    pub lastfm: LastfmConfig,
    pub discord: DiscordConfig,
    pub lyrics: LyricsConfig,
    pub downloads: DownloadsConfig,
    pub ui: UiConfig,
    pub updates: UpdatesConfig,
}

impl Default for Config {
    fn default() -> Self {
        let music_dir = directories::UserDirs::new()
            .and_then(|u| u.audio_dir().map(Path::to_path_buf))
            .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join("Music")));
        Config {
            library: LibraryConfig {
                folders: music_dir.into_iter().collect(),
                scan_on_startup: true,
            },
            playback: PlaybackConfig::default(),
            spotify: SpotifyConfig::default(),
            soundcloud: SoundCloudConfig::default(),
            apple_music: AppleMusicConfig::default(),
            lastfm: LastfmConfig::default(),
            discord: DiscordConfig::default(),
            lyrics: LyricsConfig::default(),
            downloads: DownloadsConfig::default(),
            ui: UiConfig::default(),
            updates: UpdatesConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct LibraryConfig {
    pub folders: Vec<PathBuf>,
    pub scan_on_startup: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PlaybackConfig {
    /// 0..=100
    pub volume: f32,
    /// mpv binary used for local files and SoundCloud streams.
    pub mpv_path: String,
    /// Apply ReplayGain tags to local files.
    pub replaygain: bool,
    pub gapless: bool,
    /// Optional mpv `--audio-device`, e.g. `pipewire/alsa_output.usb-...`. Empty = default.
    pub audio_device: String,
    /// Bit-perfect output for lossless files: exclusive device access, no ReplayGain.
    pub bit_perfect: bool,
    /// Crossfade between songs, in seconds (0 = off).
    pub crossfade: f32,
    /// Also crossfade between songs of the same album (they play gapless otherwise).
    pub crossfade_albums: bool,
    /// Which loudness curve `volume` was set on (see [`VOLUME_CURVE`]). Missing in settings
    /// saved before there was a choice.
    #[serde(default)]
    pub volume_curve: u8,
}

/// The volume's loudness curve: 1 = mpv's cubic curve for every source. Before (0), Spotify
/// followed librespot's 60 dB curve, about 12 dB quieter than mpv at the same setting.
pub const VOLUME_CURVE: u8 = 1;

impl Default for PlaybackConfig {
    fn default() -> Self {
        PlaybackConfig {
            volume: 70.0,
            mpv_path: "mpv".into(),
            replaygain: true,
            gapless: true,
            audio_device: String::new(),
            bit_perfect: false,
            crossfade: 0.0,
            crossfade_albums: false,
            volume_curve: VOLUME_CURVE,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SpotifyConfig {
    pub enabled: bool,
    /// 96, 160 or 320 kbps.
    pub bitrate: u16,
    pub normalisation: bool,
    /// OAuth client id used to log in (playback + library). Defaults to Spotify's desktop client id.
    pub client_id: String,
    /// Loopback port registered as redirect URI for the client id.
    pub redirect_port: u16,
    /// Optional client id of your own Spotify developer app, used for Web API calls
    /// (search, likes). The shared default client id is often rate limited.
    pub web_api_client_id: String,
    /// Redirect port registered for `web_api_client_id` (http://127.0.0.1:<port>/login).
    /// Only used when `web_api_redirect_uri` is empty.
    pub web_api_redirect_port: u16,
    /// The exact Redirect URI registered in the user's Spotify app, e.g.
    /// `http://127.0.0.1:8899/callback`. Empty = `http://127.0.0.1:<web_api_redirect_port>/login`.
    pub web_api_redirect_uri: String,
    /// Optional Client secret of that app: search and pages then use an app token, with no
    /// browser login or Redirect URI needed.
    pub web_api_client_secret: String,
    /// Keep downloaded audio in ~/.cache/multimusic/spotify (uses disk, saves bandwidth).
    pub cache_audio: bool,
    /// Spotify audio output: "auto" (PipeWire/PulseAudio when available), "pulseaudio" or "alsa".
    pub audio_output: String,
}

impl SpotifyConfig {
    /// Redirect URI used when authorizing the user's own Spotify app.
    pub fn web_api_redirect(&self) -> String {
        match self.web_api_redirect_uri.trim() {
            "" => crate::player::oauth::redirect_uri(self.web_api_redirect_port),
            uri => uri.to_string(),
        }
    }
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        SpotifyConfig {
            enabled: true,
            bitrate: 320,
            normalisation: true,
            client_id: SPOTIFY_DEFAULT_CLIENT_ID.into(),
            redirect_port: 8898,
            web_api_client_id: String::new(),
            web_api_redirect_port: 8899,
            web_api_redirect_uri: String::new(),
            web_api_client_secret: String::new(),
            cache_audio: false,
            audio_output: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SoundCloudConfig {
    pub enabled: bool,
    /// Your profile, e.g. `https://soundcloud.com/yourname`. Used to import public likes and playlists.
    pub profile_url: String,
    /// Optional `oauth_token` cookie from soundcloud.com, needed for private playlists/likes.
    pub oauth_token: String,
    /// Optional API client id override (scraped from soundcloud.com automatically when empty).
    pub client_id: String,
    /// Moved to `[downloads] folder`; only read to carry an old setting over.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub download_folder: String,
}

impl Default for SoundCloudConfig {
    fn default() -> Self {
        SoundCloudConfig {
            enabled: true,
            profile_url: String::new(),
            oauth_token: String::new(),
            client_id: String::new(),
            download_folder: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DownloadsConfig {
    /// Where downloads go. Empty = "SoundCloud", "Spotify" and "Apple Music" folders in the
    /// first library folder.
    pub folder: String,
    /// Look for Spotify and Apple Music songs on YouTube (needs yt-dlp) before SoundCloud.
    pub youtube: bool,
    pub ytdlp_path: String,
    /// Extra yt-dlp options, e.g. `--cookies-from-browser firefox` for YouTube's bot check.
    pub ytdlp_args: String,
    /// Save YouTube downloads as MP3 instead of YouTube's own Opus / AAC.
    pub youtube_mp3: bool,
    /// Embed lyrics (from LRCLIB, time-synced when available).
    pub lyrics: bool,
}

impl Default for DownloadsConfig {
    fn default() -> Self {
        DownloadsConfig {
            folder: String::new(),
            youtube: true,
            ytdlp_path: "yt-dlp".into(),
            ytdlp_args: String::new(),
            youtube_mp3: false,
            lyrics: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct AppleMusicConfig {
    /// Developer token (JWT) from music.apple.com. Optional, only needed for API import.
    pub developer_token: String,
    /// `media-user-token` cookie from music.apple.com.
    pub user_token: String,
    /// Storefront country code, e.g. `us`.
    pub storefront: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LastfmConfig {
    pub enabled: bool,
    pub api_key: String,
    pub api_secret: String,
    /// Filled in after authorizing.
    pub session_key: String,
    pub username: String,
    /// Scrobble as soon as a song starts, instead of after half of it (or 4 minutes).
    pub scrobble_instantly: bool,
}

impl Default for LastfmConfig {
    fn default() -> Self {
        LastfmConfig {
            enabled: false,
            api_key: String::new(),
            api_secret: String::new(),
            session_key: String::new(),
            username: String::new(),
            scrobble_instantly: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DiscordConfig {
    pub enabled: bool,
    /// Application id from https://discord.com/developers/applications
    pub app_id: String,
    /// Show the song title as the activity name ("Listening to <song>").
    pub song_as_activity_name: bool,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        DiscordConfig {
            enabled: true,
            app_id: String::new(),
            song_as_activity_name: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LyricsConfig {
    pub enabled: bool,
    /// Fetch from lrclib.net when no local lyrics exist.
    pub online: bool,
}

impl Default for LyricsConfig {
    fn default() -> Self {
        LyricsConfig {
            enabled: true,
            online: true,
        }
    }
}

/// The logo's off-white.
pub const DEFAULT_ACCENT: [u8; 3] = [0xe9, 0xe6, 0xdf];
/// The accent older versions used by default; replaced by the new default on load.
const OLD_DEFAULT_ACCENT: [u8; 3] = [0x8b, 0x7c, 0xf6];

/// Where new versions come from.
pub const UPDATES_REPO: &str = "v0-0x/sumo-music";
/// The repository's earlier addresses (before its owner and then it were renamed). GitHub only
/// redirects from them until someone else takes the name, so saved settings move to the new one.
const OLD_UPDATES_REPOS: [&str; 2] = ["simo1337s/localmusicplayer", "v0-0x/localmusicplayer"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiConfig {
    /// Accent colour (buttons, the playing song).
    pub accent: [u8; 3],
    /// Colour page backgrounds with the dominant colour of the current cover.
    pub dynamic_accent: bool,
    /// UI zoom factor.
    pub scale: f32,
    pub show_right_panel: bool,
    /// Show the sidebar as a narrow strip of icons and covers.
    pub collapse_library: bool,
    /// Width of the expanded sidebar (drag its edge to change it).
    pub sidebar_width: f32,
    /// Width of the lyrics / queue panel.
    pub right_panel_width: f32,
    /// Max decoded cover images kept in memory.
    pub art_cache_size: usize,
    /// The periods picked on the Last.fm profile page.
    pub profile: ProfilePrefs,
    /// The window's size when last closed (not maximized), in screen points.
    pub window_size: Option<[f32; 2]>,
    pub maximized: bool,
    /// A desktop notification when a new song starts.
    pub notifications: bool,
}

/// What each part of the Last.fm profile page shows.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ProfilePrefs {
    pub summary: Period,
    pub activity: ActivityRange,
    pub artists: Period,
    pub albums: Period,
    pub tracks: Period,
    pub genres: Period,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            accent: DEFAULT_ACCENT,
            dynamic_accent: true,
            scale: 1.0,
            show_right_panel: true,
            collapse_library: false,
            sidebar_width: 248.0,
            right_panel_width: 352.0,
            art_cache_size: 200,
            profile: ProfilePrefs::default(),
            window_size: None,
            maximized: false,
            notifications: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpdatesConfig {
    /// Look for new versions on GitHub at startup (and every few hours).
    pub check: bool,
    /// `owner/name` of the GitHub repository releases come from.
    pub repo: String,
    /// A version the user chose to skip.
    pub skipped: String,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        UpdatesConfig {
            check: true,
            repo: UPDATES_REPO.into(),
            skipped: String::new(),
        }
    }
}

/// XDG directories used by the app.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn new() -> Self {
        let dirs = directories::ProjectDirs::from("", "", "multimusic");
        let (config_dir, data_dir, cache_dir) = match dirs {
            Some(d) => (
                d.config_dir().to_path_buf(),
                d.data_dir().to_path_buf(),
                d.cache_dir().to_path_buf(),
            ),
            None => {
                #[cfg(unix)]
                // SAFETY: geteuid has no preconditions.
                let base = std::env::temp_dir().join(format!("multimusic-{}", unsafe { libc::geteuid() }));
                #[cfg(not(unix))]
                let base = std::env::temp_dir().join("multimusic");
                (base.join("config"), base.join("data"), base.join("cache"))
            }
        };
        // The app used to be called Medley: carry its settings, library and logins over.
        if let Some(old) = directories::ProjectDirs::from("", "", "medley") {
            migrate_dir(old.config_dir(), &config_dir);
            migrate_dir(old.data_dir(), &data_dir);
            migrate_dir(old.cache_dir(), &cache_dir);
        }
        for d in [&config_dir, &data_dir, &cache_dir] {
            let _ = std::fs::create_dir_all(d);
            // Keys, logins (librespot writes its own readable by all), listening history and
            // update downloads: for this user only.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
            }
        }
        Paths {
            config_dir,
            data_dir,
            cache_dir,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn database(&self) -> PathBuf {
        self.data_dir.join("library.db")
    }

    pub fn spotify_dir(&self) -> PathBuf {
        self.data_dir.join("spotify")
    }

    pub fn art_cache(&self) -> PathBuf {
        self.cache_dir.join("art")
    }

    pub fn lyrics_cache(&self) -> PathBuf {
        self.cache_dir.join("lyrics")
    }
}

/// Moves `old` to `new` if `new` doesn't exist yet.
fn migrate_dir(old: &Path, new: &Path) {
    if !old.is_dir() || new.exists() {
        return;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(old, new) {
        Ok(()) => tracing::info!("moved {} to {}", old.display(), new.display()),
        Err(e) => tracing::warn!("could not move {} to {}: {e}", old.display(), new.display()),
    }
}

impl Default for Paths {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Config {
        let file = paths.config_file();
        match std::fs::read_to_string(&file) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(mut cfg) => {
                    cfg.upgrade();
                    cfg
                }
                Err(e) => {
                    tracing::error!("invalid config {}: {e}; using defaults", file.display());
                    Config::default()
                }
            },
            Err(_) => {
                let cfg = Config::default();
                let _ = cfg.save(paths);
                cfg
            }
        }
    }

    /// Folder songs from `source` are downloaded to.
    pub fn download_dir(&self, source: crate::model::Source) -> PathBuf {
        let custom = self.downloads.folder.trim();
        if !custom.is_empty() {
            return expand_home(custom);
        }
        self.library_root().join(source.label())
    }

    /// The first library folder, where downloads go by default (so they join the library).
    pub fn library_root(&self) -> PathBuf {
        self.library
            .folders
            .first()
            .cloned()
            .or_else(|| Config::default().library.folders.first().cloned())
            .unwrap_or_else(|| PathBuf::from("Music"))
    }

    /// Adjusts settings saved by older versions.
    fn upgrade(&mut self) {
        if self.ui.accent == OLD_DEFAULT_ACCENT {
            self.ui.accent = DEFAULT_ACCENT;
        }
        if OLD_UPDATES_REPOS
            .iter()
            .any(|old| self.updates.repo.trim().eq_ignore_ascii_case(old))
        {
            self.updates.repo = UPDATES_REPO.into();
        }
        let old_folder = std::mem::take(&mut self.soundcloud.download_folder);
        if self.downloads.folder.is_empty() {
            self.downloads.folder = old_folder;
        }
        if self.playback.volume_curve < VOLUME_CURVE {
            // Keep Spotify as loud as it was, so nothing gets louder: its old curve played
            // 1000^(v-1), the new one plays v³. Local files and SoundCloud get quieter.
            let v = self.playback.volume.clamp(0.0, 100.0) / 100.0;
            if v > 0.0 {
                self.playback.volume = (1000.0 * 10f32.powf(v - 1.0)).round() / 10.0;
            }
            self.playback.volume_curve = VOLUME_CURVE;
        }
    }

    pub fn save(&self, paths: &Paths) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        let file = paths.config_file();
        let tmp = file.with_extension("toml.tmp");
        // The file holds API secrets, keep it private.
        let _ = std::fs::remove_file(&tmp);
        write_private(&tmp, text)?;
        std::fs::rename(tmp, file)?;
        Ok(())
    }
}

/// Writes a file only this user can read, for keys and logins. On Unix it is private before
/// anything is written to it, so the secret is never readable by others, not even briefly.
pub fn write_private(path: &Path, data: impl AsRef<[u8]>) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    // A file that was already there keeps its old mode when opened.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(data.as_ref())?;
    file.flush()
}

/// `~/x` → `$HOME/x`.
pub fn expand_home(path: &str) -> PathBuf {
    let path = path.trim();
    match path.strip_prefix("~/").or(if path == "~" { Some("") } else { None }) {
        Some(rest) => directories::BaseDirs::new()
            .map(|b| b.home_dir().join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_old_directory_once() {
        let base = std::env::temp_dir().join(format!("multimusic-migrate-{}", std::process::id()));
        let old = base.join("medley");
        let new = base.join("multimusic");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("library.db"), b"x").unwrap();
        migrate_dir(&old, &new);
        assert!(new.join("library.db").exists());
        assert!(!old.exists());
        // An existing new directory is never overwritten.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("library.db"), b"old").unwrap();
        migrate_dir(&old, &new);
        assert_eq!(std::fs::read(new.join("library.db")).unwrap(), b"x");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn config_roundtrip() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn partial_config_uses_defaults() {
        let cfg: Config = toml::from_str("[playback]\nvolume = 30.0\n").unwrap();
        assert_eq!(cfg.playback.volume, 30.0);
        assert_eq!(cfg.playback.mpv_path, "mpv");
        assert_eq!(cfg.spotify.client_id, SPOTIFY_DEFAULT_CLIENT_ID);
    }

    #[test]
    fn web_api_redirect_defaults_to_the_port() {
        let mut cfg = SpotifyConfig::default();
        assert_eq!(cfg.web_api_redirect(), "http://127.0.0.1:8899/login");
        cfg.web_api_redirect_port = 9001;
        assert_eq!(cfg.web_api_redirect(), "http://127.0.0.1:9001/login");
        cfg.web_api_redirect_uri = " http://127.0.0.1:8888/callback ".into();
        assert_eq!(cfg.web_api_redirect(), "http://127.0.0.1:8888/callback");
    }

    #[test]
    fn download_dir_defaults_to_the_library() {
        use crate::model::Source;
        let mut cfg = Config::default();
        cfg.library.folders = vec![PathBuf::from("/music"), PathBuf::from("/more")];
        assert_eq!(cfg.download_dir(Source::SoundCloud), PathBuf::from("/music/SoundCloud"));
        assert_eq!(cfg.download_dir(Source::Spotify), PathBuf::from("/music/Spotify"));
        cfg.downloads.folder = " /data/dl ".into();
        assert_eq!(cfg.download_dir(Source::Spotify), PathBuf::from("/data/dl"));
        cfg.downloads.folder = "~/dl".into();
        assert!(cfg.download_dir(Source::SoundCloud).ends_with("dl"));
        assert!(cfg.download_dir(Source::SoundCloud).is_absolute());

        // The folder set in the previous version moves to [downloads].
        let mut cfg: Config = toml::from_str("[soundcloud]\ndownload_folder = \"/old\"\n").unwrap();
        cfg.upgrade();
        assert_eq!(cfg.downloads.folder, "/old");
        assert!(!toml::to_string(&cfg).unwrap().contains("download_folder"));
    }

    #[test]
    fn volume_moves_to_the_new_curve_once() {
        let upgraded = |text: &str| {
            let mut cfg: Config = toml::from_str(text).unwrap();
            cfg.upgrade();
            cfg.playback
        };
        // Saved before the curves were the same: Spotify's loudness is kept.
        for (old, new) in [(100.0, 100.0), (70.0, 50.1), (50.0, 31.6), (30.0, 20.0), (0.0, 0.0)] {
            let p = upgraded(&format!("[playback]\nvolume = {old:.1}\n"));
            assert_eq!((p.volume, p.volume_curve), (new, VOLUME_CURVE), "from {old}");
        }
        // Saved since: left alone, also when loaded again.
        let p = upgraded("[playback]\nvolume = 50.0\nvolume_curve = 1\n");
        assert_eq!(p.volume, 50.0);
        let mut cfg = Config::default();
        cfg.playback.volume = 31.6;
        let text = toml::to_string(&cfg).unwrap();
        assert_eq!(upgraded(&text).volume, 31.6);
        // A new install starts on the new curve.
        assert_eq!(Config::default().playback.volume_curve, VOLUME_CURVE);
    }

    #[test]
    fn old_default_accent_is_replaced() {
        let mut cfg: Config = toml::from_str("[ui]\naccent = [139, 124, 246]\n").unwrap();
        cfg.upgrade();
        assert_eq!(cfg.ui.accent, DEFAULT_ACCENT);
        // A colour the user picked is kept.
        let mut cfg: Config = toml::from_str("[ui]\naccent = [200, 10, 10]\n").unwrap();
        cfg.upgrade();
        assert_eq!(cfg.ui.accent, [200, 10, 10]);
    }

    #[test]
    fn updates_come_from_the_renamed_repository() {
        for old in OLD_UPDATES_REPOS {
            let mut cfg: Config = toml::from_str(&format!("[updates]\nrepo = \"{old}\"\n")).unwrap();
            cfg.upgrade();
            assert_eq!(cfg.updates.repo, UPDATES_REPO);
        }
        assert_eq!(Config::default().updates.repo, UPDATES_REPO);
        // Another repository someone chose is kept.
        let mut cfg: Config = toml::from_str("[updates]\nrepo = \"someone/fork\"\n").unwrap();
        cfg.upgrade();
        assert_eq!(cfg.updates.repo, "someone/fork");
    }
}
