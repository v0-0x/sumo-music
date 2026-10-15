//! The background service: owns playback, the database and every network integration.
//! The UI talks to it with [`Command`]s and reads state from [`Shared`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::config::{Config, Paths};
use crate::downloader::{self, Downloader, Saved};
use crate::integrations::discord::{Discord, Presence};
use crate::integrations::genius;
use crate::integrations::lastfm::{Lastfm, ScrobbleTracker};
use crate::integrations::lastfm_stats::{ArtLookup, ProfileFeed, ProfileStats, StatsData, StatsRequest};
use crate::integrations::lyrics::LyricsFetcher;
use crate::integrations::mpris::Mpris;
use crate::library::{self, liked_playlist, Db, Library, LIKED_ID};
use crate::links::{self, LinkKind, Target};
use crate::model::{
    normalize_artist, normalize_title, now_unix, ArtistHit, AudioQuality, CollectionHit, ImportedPlaylist, Lyrics,
    Playlist, PlaylistKind, RepeatMode, Source, Track,
};
use crate::player::mpv::{Mpv, MpvEvent, MpvOptions, MpvSender};
use crate::player::queue::{Queue, QueueState};
use crate::player::spotify::{self, SpotifyAuth, SpotifyDeck, SpotifyEngine, SpotifyEvent, SpotifySender};
use crate::providers::apple_music::{self, AppleMusicApi};
use crate::providers::soundcloud::{self, ScResolved, ScUser, SoundCloud};
use crate::providers::spotify_api::{self, SpotifyApi, SpotifyPlaylistMeta};
use crate::providers::youtube::YtDlp;

/// Requests from the UI (and MPRIS).
#[derive(Debug, Clone)]
pub enum Command {
    Play {
        tracks: Vec<Track>,
        start: usize,
        context: String,
    },
    TogglePause,
    Pause,
    Resume,
    Next,
    Previous,
    Seek(f64),
    SeekRelative(f64),
    SetVolume(f32),
    SetShuffle(bool),
    CycleRepeat,
    Enqueue(Vec<Track>),
    PlayNext(Vec<Track>),
    /// Entry of the upcoming list and the song shown there (in case the queue moved on).
    JumpTo(usize, String),
    RemoveUpcoming(usize, String),
    ClearUpcoming,
    ToggleLike(Track),
    CreatePlaylist {
        name: String,
        tracks: Vec<Track>,
    },
    AddToPlaylist {
        playlist_id: String,
        tracks: Vec<Track>,
    },
    /// Removes rows of a playlist: each one's position and song (the song wins when the
    /// position no longer holds it).
    RemoveFromPlaylist {
        playlist_id: String,
        rows: Vec<(usize, String)>,
    },
    /// Adds the songs behind pasted links (Spotify or SoundCloud songs, albums and playlists,
    /// or music files), in order.
    AddLinksToPlaylist {
        playlist_id: String,
        links: Vec<String>,
    },
    RenamePlaylist {
        playlist_id: String,
        name: String,
    },
    DeletePlaylist(String),
    /// Saves the album on the open page (by its page key) to the library's albums.
    SaveAlbum(String),
    /// Shows a message (for things the UI does on its own, like copying songs).
    Notify(String),
    /// The main window's native handle (an HWND on Windows), for the system media controls.
    #[cfg_attr(not(windows), allow(dead_code))]
    AttachWindow(isize),
    /// Writes changed details into local files: each field gets the text given (empty removes
    /// it), and the cover is replaced or removed.
    EditTags {
        paths: Vec<PathBuf>,
        changes: Vec<(library::tags::Field, String)>,
        cover: Option<CoverChange>,
    },
    /// Looks a song up on Spotify for the tag editor; the answer lands in `Feed::tag_lookup`
    /// under `request`.
    LookUpTags {
        request: u64,
        track: Track,
    },
    ImportM3u(PathBuf),
    ExportM3u {
        playlist_id: String,
        path: PathBuf,
    },
    ImportAppleXml(PathBuf),
    ImportAppleApi,
    /// Saves the settings (and optionally keys, logins and your playlists) to a file.
    ExportSettings {
        path: PathBuf,
        include: crate::backup::Include,
    },
    /// Takes over the settings in a file, then restarts the app.
    ImportSettings(PathBuf),
    /// Looks for a new version (`manual`: the user asked, so say when there is none).
    CheckUpdates {
        manual: bool,
    },
    /// Downloads and installs the new version found, then quits (it starts again by itself).
    /// On Linux it then waits for the password (`Feed::update.status` asks for it).
    InstallUpdate,
    /// The password to install the downloaded update with (Linux).
    AuthorizeUpdate(crate::updater::Secret),
    /// Don't install the downloaded update after all.
    CancelUpdate,
    Rescan,
    /// Ask mpv which output devices exist (for the device picker).
    ListAudioDevices,
    SpotifyLogin,
    /// Authorize the user's own developer app for Web API calls.
    SpotifyWebApiLogin,
    SpotifyLogout,
    SyncSpotify,
    SyncSoundCloud,
    LastfmLogin,
    LastfmLogout,
    /// Loads (or reloads, once it's old) something the Last.fm profile page shows.
    LastfmStats(StatsRequest),
    /// Looks up pictures for artists (title empty) and songs Last.fm lists without one.
    LastfmPictures(Vec<(String, String)>),
    /// Loads everything on the profile page again.
    RefreshLastfmProfile,
    Search(String),
    /// Load an artist / album / playlist / song page: a page key or a pasted link.
    OpenPage(String),
    /// Save songs as files: SoundCloud songs directly, Spotify and Apple Music songs from a
    /// matching YouTube or SoundCloud upload.
    Download(Vec<Track>),
    /// Remove finished and failed entries from the Downloads list.
    ClearDownloads,
    /// Stop a waiting or running download (by track id).
    CancelDownload(String),
    /// Stop every waiting and running download.
    CancelDownloads,
    /// Find out whether this yt-dlp program runs (answer in `Feed::ytdlp`).
    CheckYtDlp(String),
    UpdateConfig(Box<Config>),
    Raise,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayStatus {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
}

/// Snapshot of playback for the UI.
#[derive(Debug, Clone)]
pub struct PlayerView {
    pub current: Option<Track>,
    /// The track actually playing when `current` had to be resolved (Apple Music imports).
    pub via: Option<Track>,
    pub status: PlayStatus,
    pub position: f64,
    pub position_at: Instant,
    pub duration: f64,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub upcoming: Vec<Track>,
    pub up_next_len: usize,
    pub context: String,
    /// First song of the list playback started from (tells lists with the same name apart).
    pub context_first: Option<String>,
    /// Format of what's playing (codec, bit depth, sample rate).
    pub quality: Option<AudioQuality>,
}

impl Default for PlayerView {
    fn default() -> Self {
        PlayerView {
            current: None,
            via: None,
            status: PlayStatus::Stopped,
            position: 0.0,
            position_at: Instant::now(),
            duration: 0.0,
            volume: 70.0,
            shuffle: false,
            repeat: RepeatMode::Off,
            upcoming: Vec::new(),
            up_next_len: 0,
            context: String::new(),
            context_first: None,
            quality: None,
        }
    }
}

impl PlayerView {
    /// Position extrapolated to now.
    pub fn position_now(&self) -> f64 {
        let p = if self.status == PlayStatus::Playing {
            self.position + self.position_at.elapsed().as_secs_f64()
        } else {
            self.position
        };
        if self.duration > 0.0 {
            p.min(self.duration)
        } else {
            p
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum AccountStatus {
    #[default]
    Off,
    Working(String),
    Connected(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub at: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    pub spotify: Vec<Track>,
    pub soundcloud: Vec<Track>,
    /// Spotify artists and SoundCloud profiles, best matches first.
    pub artists: Vec<ArtistHit>,
    /// Albums and public playlists from Spotify and SoundCloud.
    pub albums: Vec<CollectionHit>,
    pub playlists: Vec<CollectionHit>,
    pub spotify_pending: bool,
    pub soundcloud_pending: bool,
    pub errors: Vec<String>,
}

impl SearchState {
    pub fn pending(&self) -> bool {
        self.spotify_pending || self.soundcloud_pending
    }
}

/// An artist / album / playlist / song page from one of the services.
#[derive(Debug, Clone, Default)]
pub struct PageState {
    /// The page key or link it was opened with.
    pub key: String,
    pub loading: bool,
    pub error: Option<String>,
    /// "Artist", "Album", "Playlist" or "Song".
    pub kind: String,
    pub source: Option<Source>,
    pub title: String,
    pub subtitle: String,
    /// An album's artist.
    pub artist: String,
    pub image: Option<String>,
    /// Artist pictures are drawn round.
    pub round: bool,
    pub tracks: Vec<Track>,
    /// The page on the service's website.
    pub external_url: Option<String>,
    /// Still adding the artist's songs from this other service.
    pub merging: Option<Source>,
}

/// One song in the Downloads list.
#[derive(Debug, Clone)]
pub struct DownloadItem {
    pub track: Track,
    pub state: DownloadState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DownloadState {
    Queued,
    /// Progress 0..=1.
    Running(f32),
    /// `from`: where the audio came from ("original file", "YouTube", …).
    Done {
        path: PathBuf,
        from: String,
    },
    Failed(String),
}

impl DownloadState {
    pub fn active(&self) -> bool {
        matches!(self, DownloadState::Queued | DownloadState::Running(_))
    }
}

#[derive(Debug, Clone, Default)]
pub struct LyricsState {
    pub track_id: String,
    pub loading: bool,
    pub lyrics: Option<Lyrics>,
    /// Lyrics are turned off in Settings (none were looked for).
    pub off: bool,
}

/// Everything else the UI shows that isn't library or playback.
#[derive(Debug, Clone, Default)]
pub struct Feed {
    pub toasts: Vec<Toast>,
    pub search: SearchState,
    pub page: PageState,
    pub lyrics: LyricsState,
    /// Spotify's details of a song, for the tag editor's request with this number.
    pub tag_lookup: Option<(u64, Result<library::tags::Metadata, String>)>,
    /// Covers that changed: the UI drops these images (it compares the number to the last
    /// one it handled).
    pub art_changed: (u64, Vec<String>),
    /// Downloads started this session, oldest first.
    pub downloads: Vec<DownloadItem>,
    /// Downloaded songs: track id → file.
    pub downloaded: HashMap<String, PathBuf>,
    /// The yt-dlp program last checked, and its version or why it doesn't run.
    pub ytdlp: Option<(String, Result<String, String>)>,
    pub spotify: AccountStatus,
    /// True while a Spotify login is stored, whatever the current status message says.
    pub spotify_logged_in: bool,
    /// Status of the optional own-app Web API login.
    pub spotify_web_api: AccountStatus,
    pub soundcloud: AccountStatus,
    pub lastfm: AccountStatus,
    /// The Last.fm profile page: stats loaded so far.
    pub profile: ProfileFeed,
    pub scan: Option<(usize, usize)>,
    /// mpv output devices as (id, description), filled on request.
    pub audio_devices: Vec<(String, String)>,
    pub raise: bool,
    pub quit: bool,
    /// Settings were imported: the UI takes these over and the app restarts.
    pub imported_settings: Option<Box<Config>>,
    /// Start Sumo again once it has closed (asked for after the player stopped working).
    pub restart: bool,
    /// The settings file last exported.
    pub exported_settings: Option<PathBuf>,
    pub update: UpdateState,
}

/// New versions of Sumo.
#[derive(Debug, Clone, Default)]
pub struct UpdateState {
    /// A newer release than this one.
    pub available: Option<crate::updater::Release>,
    pub status: UpdateStatus,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    /// Checked on request: this is the newest version.
    UpToDate,
    /// Downloading, 0.0..=1.0.
    Downloading(f32),
    /// Downloaded and checked; installing needs the password (Linux). Each request has its
    /// own number, so the UI asks once per request; `wrong` after a mistyped one.
    NeedsPassword {
        attempt: u32,
        wrong: bool,
    },
    /// The new version is being installed; Sumo quits and starts again.
    Installing,
    /// Installed, but Sumo couldn't start itself again.
    Installed,
    Failed(String),
}

/// State shared between the service and the UI.
#[derive(Default)]
pub struct Shared {
    pub library: RwLock<Library>,
    pub player: RwLock<PlayerView>,
    pub feed: RwLock<Feed>,
    pub ctx: OnceLock<egui::Context>,
}

impl Shared {
    pub fn repaint(&self) {
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }

    fn toast(&self, kind: ToastKind, text: impl Into<String>) {
        let text = text.into();
        match kind {
            ToastKind::Error => tracing::warn!("{text}"),
            ToastKind::Info => tracing::info!("{text}"),
        }
        let mut feed = self.feed.write().unwrap();
        feed.toasts.push(Toast {
            text,
            kind,
            at: Instant::now(),
        });
        if feed.toasts.len() > 5 {
            feed.toasts.remove(0);
        }
        drop(feed);
        self.repaint();
    }

    fn info(&self, text: impl Into<String>) {
        self.toast(ToastKind::Info, text);
    }

    fn error(&self, text: impl Into<String>) {
        self.toast(ToastKind::Error, text);
    }
}

/// Results of background jobs, sent back into the service loop.
enum Internal {
    StreamReady {
        seq: u64,
        track: Track,
        url: String,
    },
    Resolved {
        seq: u64,
        original: Track,
        resolved: Option<Track>,
    },
    LoadFailed {
        seq: u64,
        error: String,
    },
    SpotifyReady(Result<SpotifyEngine>),
    SpotifySynced(Result<SpotifySync>),
    SoundCloudSynced(Result<SoundCloudSync>),
    AppleImported(Result<(Vec<Track>, Vec<ImportedPlaylist>)>),
    M3uImported(Result<ImportedPlaylist>),
    LinksResolved {
        playlist_id: String,
        tracks: Vec<Track>,
        failed: usize,
    },
    UpdateChecked {
        manual: bool,
        result: Result<crate::updater::Release, String>,
    },
    UpdateFailed(String),
    /// The update was downloaded and checked; installing it needs the password.
    UpdateDownloaded(PathBuf),
    /// sudo and pacman are done: `Err(None)` means a wrong password.
    UpdateInstalled(std::result::Result<(), Option<String>>),
    TagsEdited {
        /// The files read again, with their new modification times.
        tracks: Vec<(Track, i64)>,
        errors: Vec<String>,
        cover_changed: bool,
    },
    TagLookup {
        request: u64,
        result: Result<library::tags::Metadata, String>,
    },
    ScanDone(library::scanner::ScanResult),
    Lyrics {
        track_id: String,
        lyrics: Option<Lyrics>,
    },
    SearchResults {
        query: String,
        source: Source,
        tracks: Result<Vec<Track>>,
        artists: Vec<ArtistHit>,
        albums: Vec<CollectionHit>,
        playlists: Vec<CollectionHit>,
    },
    /// The client_id SoundCloud accepted, saved so the next start needn't scrape one.
    SoundCloudClientId(String),
    PageLoaded {
        key: String,
        result: Result<PageState>,
    },
    /// A short link was expanded to `url`.
    ShortLink {
        key: String,
        result: Result<String>,
    },
    LastfmSession(Result<(String, String)>),
    LastfmStats {
        user: String,
        request: StatsRequest,
        result: Result<StatsData, String>,
    },
    LastfmPicture {
        user: String,
        artist: String,
        title: String,
        found: Option<String>,
    },
    Quality {
        track_id: String,
        quality: Option<AudioQuality>,
    },
    AudioDevices(Vec<(String, String)>),
    DownloadDone {
        track: Track,
        result: Result<Saved>,
    },
    YtDlpChecked {
        program: String,
        result: Result<String, String>,
    },
    /// Status text while the Spotify library syncs.
    SyncProgress(String),
}

struct SpotifySync {
    user: String,
    /// Playlists in Spotify's order with their track ids. `None` = unchanged since last sync.
    playlists: Vec<(SpotifyPlaylistMeta, Option<Vec<String>>)>,
    /// Liked Songs track ids, newest first. `None` = couldn't be loaded (kept as they are).
    liked: Option<Vec<String>>,
    /// Playlists (Spotify ids) that couldn't be loaded this time: kept as they are rather
    /// than taken for deleted.
    kept: Vec<String>,
    /// Albums saved on Spotify that are new to the library.
    albums: Vec<Playlist>,
    /// New or updated track metadata.
    tracks: Vec<Track>,
}

struct SoundCloudSync {
    user: String,
    likes: Vec<Track>,
    /// Why the playlists couldn't be loaded, if they couldn't (they are kept as they are).
    playlists: std::result::Result<Vec<ImportedPlaylist>, String>,
}

/// GitHub's API, where releases (updates) come from.
const GITHUB_API: &str = "https://api.github.com";

/// Bumped when the scanner learns to read more from files, so the library is read once again.
const SCAN_VERSION: &str = "2";

/// How often crossfade timing is checked and volumes are stepped.
const FADE_STEP: Duration = Duration::from_millis(50);
/// Longest crossfade offered in the settings.
const MAX_CROSSFADE: f32 = 12.0;
/// Shortest fade, used when the previous track is almost over.
const MIN_FADE: f64 = 0.3;
/// How long the volume has to stay put before it is saved.
const VOLUME_SAVE_DELAY: Duration = Duration::from_secs(2);

/// The previous track during a crossfade, fading out on its own player.
struct Fading {
    deck: Deck,
    /// When that track runs out.
    ends_at: Instant,
    /// Start and length of the fade-out, set once the next track is audible.
    ramp: Option<(Instant, f64)>,
}

/// A player taken out of service to fade out.
enum Deck {
    Mpv(Box<Mpv>),
    Spotify(SpotifyDeck),
}

impl Deck {
    async fn set_volume(&self, volume: f32) {
        match self {
            Deck::Mpv(mpv) => {
                let _ = mpv.set_volume(volume).await;
            }
            Deck::Spotify(deck) => deck.set_volume(volume),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FadeIn {
    /// The next track is loading; it starts silent.
    Waiting,
    Ramping {
        start: Instant,
        len: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    None,
    Mpv,
    Spotify,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedSession {
    /// Before 0.5.3: the current song and what was coming up, as a plain list.
    #[serde(default)]
    tracks: Vec<String>,
    #[serde(default)]
    index: usize,
    position: f64,
    #[serde(default)]
    context: String,
    #[serde(default)]
    shuffle: bool,
    #[serde(default)]
    repeat: RepeatMode,
    /// The whole queue (play order, songs you queued, removed songs).
    #[serde(default)]
    queue: Option<QueueState>,
    /// Songs of the queue the library doesn't have (from search, artist or album pages).
    #[serde(default)]
    extra: Vec<Track>,
}

/// Where in the current song playback was; saved every few seconds (the queue only when it
/// changes).
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedPosition {
    track: String,
    position: f64,
    /// Already scrobbled: picking the song up again doesn't scrobble it twice.
    scrobbled: bool,
}

pub struct Service {
    shared: Arc<Shared>,
    paths: Paths,
    cfg: Config,
    db: Db,
    http: reqwest::Client,
    cmd_tx: UnboundedSender<Command>,
    internal_tx: UnboundedSender<Internal>,

    queue: Queue,
    engine: Engine,
    status: PlayStatus,
    position: f64,
    position_at: Instant,
    duration: f64,
    /// Playable track for the current queue entry (differs for resolved imports).
    playing: Option<Track>,
    quality: Option<AudioQuality>,
    load_seq: u64,
    started_reported: bool,
    resume_position: Option<f64>,
    consecutive_failures: u32,
    /// Pause was pressed while the song was loading: it starts paused.
    pause_wanted: bool,
    /// The queue changed and the saved session is behind.
    session_dirty: bool,
    /// When the playback position was last saved.
    position_saved_at: Instant,
    /// The song picked up from the last run, if it had been scrobbled then.
    resumed_scrobbled: Option<String>,

    mpv: Option<Mpv>,
    /// An idle mpv kept for the next crossfade.
    mpv_spare: Option<Mpv>,
    mpv_next_id: u64,
    mpv_tx: MpvSender,
    mpv_preloaded: Option<String>,
    /// mpv's playlist entry for the current song; `None` until the file we asked for starts.
    /// The end of any other entry (one that finished just as another song was asked for) is
    /// ignored.
    mpv_entry: Option<i64>,
    /// The previous track fading out on its own player during a crossfade.
    fading: Option<Fading>,
    /// The current track's fade-in during a crossfade.
    fade_in: Option<FadeIn>,

    spotify_auth: Arc<SpotifyAuth>,
    /// Optional login with the user's own developer app, for Web API calls.
    spotify_web_auth: Option<Arc<SpotifyAuth>>,
    spotify: Option<SpotifyEngine>,
    spotify_connecting: bool,
    /// A sync was requested before the session was connected.
    spotify_sync_pending: bool,
    spotify_syncing: bool,
    spotify_tx: SpotifySender,
    spotify_api: Arc<SpotifyApi>,
    /// A Spotify page waiting for the session to connect.
    spotify_pending_page: Option<String>,
    /// A Spotify song waiting for the connection (load number, song, start position).
    spotify_pending_play: Option<(u64, Track, f64)>,
    /// The load that was a retry after a stream broke off (only one retry per song).
    retried_load: u64,
    /// Recently loaded pages, so going back doesn't reload them.
    page_cache: Vec<PageState>,
    /// Apple Music developer token (from the settings, or scraped once).
    apple_dev_token: Arc<tokio::sync::Mutex<Option<String>>>,

    soundcloud: Arc<SoundCloud>,
    lyrics: Arc<LyricsFetcher>,
    lastfm: Option<Arc<Lastfm>>,
    /// Loads the profile page's stats (made when first needed).
    profile_stats: Option<Arc<ProfileStats>>,
    /// A downloaded update waiting for the password (Linux), and how often it was asked for.
    update_package: Option<PathBuf>,
    update_attempt: u32,
    scrobble: ScrobbleTracker,
    discord: Discord,
    mpris: Mpris,
    last_tick: Instant,
    /// When the volume last changed and isn't saved yet (saved once it settles).
    volume_changed_at: Option<Instant>,

    /// Running downloads by track id: their task and work folder.
    download_jobs: HashMap<String, (tokio::task::AbortHandle, PathBuf)>,
    download_seq: u64,
    /// Title and error (if any) of each download finished since the queue was last empty.
    download_batch: Vec<(String, Option<String>)>,
}

/// Starts the service on its own thread, driving futures on `rt`. Returns the command sender.
pub fn start(
    rt: tokio::runtime::Handle,
    shared: Arc<Shared>,
    paths: Paths,
    cfg: Config,
) -> (UnboundedSender<Command>, std::thread::JoinHandle<()>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let tx = cmd_tx.clone();
    let handle = std::thread::Builder::new()
        .name("multimusic-service".into())
        .spawn(move || {
            rt.block_on(async move {
                match Service::new(shared.clone(), paths, cfg, tx) {
                    Ok((svc, mpv_rx, sp_rx, int_rx)) => svc.run(cmd_rx, mpv_rx, sp_rx, int_rx).await,
                    Err(e) => {
                        shared.error(format!("Failed to start: {e:#}"));
                    }
                }
            });
        })
        .expect("spawn service thread");
    (cmd_tx, handle)
}

type MpvReceiver = UnboundedReceiver<(u64, MpvEvent)>;
type SpotifyReceiver = UnboundedReceiver<(u64, SpotifyEvent)>;
type Receivers = (MpvReceiver, SpotifyReceiver, UnboundedReceiver<Internal>);
type Started = (Service, MpvReceiver, SpotifyReceiver, UnboundedReceiver<Internal>);

impl Service {
    fn new(shared: Arc<Shared>, paths: Paths, cfg: Config, cmd_tx: UnboundedSender<Command>) -> Result<Started> {
        let db = Db::open(&paths.database())?;
        let lib = Library::load(&db)?;
        *shared.library.write().unwrap() = lib;

        let http = crate::http::client();
        let (mpv_tx, mpv_rx) = mpsc::unbounded_channel();
        let (spotify_tx, sp_rx) = mpsc::unbounded_channel();
        let (internal_tx, int_rx): (UnboundedSender<Internal>, UnboundedReceiver<Internal>) = mpsc::unbounded_channel();
        let receivers: Receivers = (mpv_rx, sp_rx, int_rx);

        let spotify_auth = Arc::new(SpotifyAuth::new(&cfg.spotify, &paths.spotify_dir()));
        let spotify_web_auth = SpotifyAuth::web_api(&cfg.spotify, &paths.spotify_dir()).map(Arc::new);
        let soundcloud = Arc::new(SoundCloud::new(
            http.clone(),
            &cfg.soundcloud.client_id,
            &cfg.soundcloud.oauth_token,
        ));
        let lyrics = Arc::new(
            LyricsFetcher::new(http.clone(), paths.lyrics_cache(), cfg.lyrics.online).with_genius(genius::shared()),
        );
        let lastfm = make_lastfm(&cfg, &http, &paths);
        let discord = Discord::spawn(
            cfg.discord.enabled,
            &cfg.discord.app_id,
            cfg.discord.song_as_activity_name,
        );
        crate::player::mpv::stop_orphans();
        let mpris = Mpris::new(cmd_tx.clone(), None);
        // Look for a new version a little after starting, then every few hours.
        let updates = cmd_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(8)).await;
            loop {
                if updates.send(Command::CheckUpdates { manual: false }).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
            }
        });

        let svc = Service {
            shared,
            paths,
            db,
            http: http.clone(),
            cmd_tx,
            internal_tx,
            queue: Queue::default(),
            engine: Engine::None,
            status: PlayStatus::Stopped,
            position: 0.0,
            position_at: Instant::now(),
            duration: 0.0,
            playing: None,
            quality: None,
            load_seq: 0,
            started_reported: false,
            resume_position: None,
            pause_wanted: false,
            session_dirty: false,
            position_saved_at: Instant::now(),
            resumed_scrobbled: None,
            consecutive_failures: 0,
            mpv: None,
            mpv_spare: None,
            mpv_next_id: 1,
            mpv_tx,
            mpv_preloaded: None,
            mpv_entry: None,
            fading: None,
            fade_in: None,
            spotify_auth,
            spotify_web_auth,
            spotify: None,
            spotify_connecting: false,
            spotify_sync_pending: false,
            spotify_syncing: false,
            spotify_tx,
            spotify_api: Arc::new(SpotifyApi::new(http)),
            spotify_pending_page: None,
            spotify_pending_play: None,
            retried_load: 0,
            page_cache: Vec::new(),
            apple_dev_token: Arc::new(tokio::sync::Mutex::new(None)),
            soundcloud,
            lyrics,
            lastfm,
            profile_stats: None,
            update_package: None,
            update_attempt: 0,
            scrobble: {
                let mut tracker = ScrobbleTracker::new();
                tracker.set_instant(cfg.lastfm.scrobble_instantly);
                tracker
            },
            discord,
            mpris,
            last_tick: Instant::now(),
            volume_changed_at: None,
            download_jobs: HashMap::new(),
            download_seq: 0,
            download_batch: Vec::new(),
            cfg,
        };
        let (a, b, c) = receivers;
        Ok((svc, a, b, c))
    }

    async fn run(
        mut self,
        mut cmd_rx: UnboundedReceiver<Command>,
        mut mpv_rx: MpvReceiver,
        mut sp_rx: SpotifyReceiver,
        mut int_rx: UnboundedReceiver<Internal>,
    ) {
        self.startup();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Crossfade timing and volume ramps; only runs while one could happen.
        let mut fade_tick = tokio::time::interval(FADE_STEP);
        fade_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let fade_active = self.fade_watch_needed();
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    if matches!(cmd, Command::Quit) {
                        break;
                    }
                    self.handle(cmd).await;
                }
                Some((id, ev)) = mpv_rx.recv() => self.on_mpv(id, ev).await,
                Some((id, ev)) = sp_rx.recv() => self.on_spotify(id, ev).await,
                Some(msg) = int_rx.recv() => self.on_internal(msg).await,
                _ = tick.tick() => self.on_tick().await,
                _ = fade_tick.tick(), if fade_active => self.on_fade_tick().await,
            }
        }
        self.shutdown().await;
    }

    fn startup(&mut self) {
        self.publish_accounts();
        self.check_app_secret();
        self.load_downloads();
        self.clear_download_leftovers();
        self.restore_session();
        if self.cfg.library.scan_on_startup && !self.cfg.library.folders.is_empty() {
            self.start_scan();
        }
        if self.cfg.spotify.enabled && self.spotify_auth.has_login() {
            self.connect_spotify();
            self.start_spotify_sync();
        }
        if self.cfg.soundcloud.enabled {
            self.warm_up_soundcloud();
        }
        if self.cfg.soundcloud.enabled && self.soundcloud_configured() {
            let last: i64 = self
                .db
                .get_kv("soundcloud_synced_at")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if now_unix() - last > 6 * 3600 {
                self.start_soundcloud_sync();
            } else {
                self.set_account(
                    |f| &mut f.soundcloud,
                    AccountStatus::Connected(self.db.get_kv("soundcloud_user").unwrap_or_default()),
                );
            }
        }
        if let Some(lfm) = self.lastfm.clone() {
            tokio::spawn(async move {
                let _ = lfm.flush_queue().await;
            });
        }
        // Media controls report 100% until told otherwise, and desktop volume controls
        // step from what they report.
        self.mpris.set_volume(self.cfg.playback.volume);
        self.publish_player();
        self.publish_queue();
    }

    async fn shutdown(&mut self) {
        self.save_session();
        if self.volume_changed_at.take().is_some() {
            self.save_config();
        }
        self.discord.shutdown();
        self.cancel_fades().await;
        if let Some(sp) = self.spotify.take() {
            sp.shutdown();
        }
        if let Some(mpv) = self.mpv.take() {
            mpv.quit().await;
        }
        if let Some(mpv) = self.mpv_spare.take() {
            mpv.quit().await;
        }
        let mut feed = self.shared.feed.write().unwrap();
        feed.quit = true;
        drop(feed);
        self.shared.repaint();
    }

    // ---------------------------------------------------------------- commands

    async fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Play { tracks, start, context } => {
                self.resume_position = None;
                if self.queue.set_context(tracks, start, context).is_some() {
                    self.consecutive_failures = 0;
                    self.play_current(0.0).await;
                }
                self.publish_queue();
            }
            Command::TogglePause => {
                let loading = self.status == PlayStatus::Loading;
                if self.status == PlayStatus::Playing || (loading && !self.pause_wanted) {
                    self.pause().await;
                } else {
                    self.resume().await;
                }
            }
            Command::Pause => self.pause().await,
            Command::Resume => self.resume().await,
            Command::Next => {
                self.consecutive_failures = 0;
                if self.queue.advance(true).is_some() {
                    self.play_current(0.0).await;
                } else {
                    self.stop().await;
                }
                self.publish_queue();
            }
            Command::Previous => {
                if self.current_position() > 3.0 {
                    self.seek(0.0).await;
                } else if self.queue.previous().is_some() {
                    self.play_current(0.0).await;
                    self.publish_queue();
                }
            }
            Command::Seek(secs) => self.seek(secs).await,
            Command::SeekRelative(d) => {
                let p = self.current_position() + d;
                self.seek(p).await;
            }
            Command::SetVolume(v) => {
                let v = v.clamp(0.0, 100.0);
                self.cfg.playback.volume = v;
                // During a crossfade the ramps apply the new volume on their next step.
                if self.fade_in.is_none() {
                    if let Some(mpv) = &self.mpv {
                        let _ = mpv.set_volume(v).await;
                    }
                    if let Some(sp) = &self.spotify {
                        sp.set_volume(v);
                    }
                }
                self.mpris.set_volume(v);
                self.publish_player();
                self.volume_changed_at = Some(Instant::now());
            }
            Command::SetShuffle(on) => {
                self.queue.set_shuffle(on);
                self.after_queue_change().await;
            }
            Command::CycleRepeat => {
                self.queue.repeat = self.queue.repeat.cycle();
                self.after_queue_change().await;
            }
            Command::Enqueue(tracks) => {
                let n = tracks.len();
                if self.queue.is_empty() {
                    self.queue.set_context(tracks, 0, "Queue".into());
                    self.play_current(0.0).await;
                } else {
                    self.queue.enqueue(tracks);
                }
                self.shared
                    .info(format!("Added {n} track{} to the queue", if n == 1 { "" } else { "s" }));
                self.after_queue_change().await;
            }
            Command::PlayNext(tracks) => {
                let n = tracks.len();
                if self.queue.is_empty() {
                    self.queue.set_context(tracks, 0, "Queue".into());
                    self.play_current(0.0).await;
                } else {
                    self.queue.play_next(tracks);
                    self.shared.info(if n == 1 {
                        "Plays next".to_string()
                    } else {
                        format!("{n} tracks play next")
                    });
                }
                self.after_queue_change().await;
            }
            Command::JumpTo(i, id) => {
                if self.queue.jump_to(i, &id).is_some() {
                    self.play_current(0.0).await;
                }
                self.publish_queue();
            }
            Command::RemoveUpcoming(i, id) => {
                self.queue.remove(i, &id);
                self.after_queue_change().await;
            }
            Command::ClearUpcoming => {
                self.queue.clear_upcoming();
                self.after_queue_change().await;
            }
            Command::ToggleLike(track) => self.toggle_like(track),
            Command::CreatePlaylist { name, tracks } => {
                let id = format!("custom:{:x}{:04x}", now_unix(), rand::random::<u16>());
                let p = Playlist {
                    id,
                    name: name.clone(),
                    kind: PlaylistKind::Custom,
                    remote_id: None,
                    description: String::new(),
                    art: tracks.iter().find_map(|t| t.art.clone()),
                    track_ids: tracks.iter().map(|t| t.id.clone()).collect(),
                };
                self.store_tracks(&tracks);
                self.save_playlists(vec![p], &[]);
                self.shared.info(format!("Created playlist “{name}”"));
            }
            Command::AddToPlaylist { playlist_id, tracks } => self.add_to_playlist(&playlist_id, &tracks),
            Command::AddLinksToPlaylist { playlist_id, links } => self.add_links_to_playlist(playlist_id, links),
            Command::RemoveFromPlaylist { playlist_id, mut rows } => {
                let updated = {
                    let mut lib = self.shared.library.write().unwrap();
                    lib.playlist_mut(&playlist_id).and_then(|p| {
                        let before = p.track_ids.len();
                        // Last rows first, so the earlier positions still hold.
                        rows.sort_by_key(|row| std::cmp::Reverse(row.0));
                        for (index, id) in rows {
                            let at = if p.track_ids.get(index) == Some(&id) {
                                Some(index)
                            } else {
                                p.track_ids.iter().position(|t| *t == id)
                            };
                            if let Some(at) = at {
                                p.track_ids.remove(at);
                            }
                        }
                        (p.track_ids.len() != before).then(|| p.clone())
                    })
                };
                if let Some(p) = updated {
                    self.save_playlists(vec![p], &[]);
                }
            }
            Command::RenamePlaylist { playlist_id, name } => {
                let updated = {
                    let mut lib = self.shared.library.write().unwrap();
                    lib.playlist_mut(&playlist_id).map(|p| {
                        p.name = name;
                        p.clone()
                    })
                };
                if let Some(p) = updated {
                    self.save_playlists(vec![p], &[]);
                }
            }
            Command::Notify(text) => self.shared.info(text),
            Command::EditTags { paths, changes, cover } => self.edit_tags(paths, changes, cover),
            Command::LookUpTags { request, track } => self.look_up_tags(request, track),
            Command::DeletePlaylist(id) => {
                if id != LIKED_ID {
                    let album = {
                        let lib = self.shared.library.read().unwrap();
                        lib.playlist(&id)
                            .filter(|p| p.kind == PlaylistKind::SpotifyAlbum)
                            .and_then(|p| p.remote_id.clone())
                    };
                    self.save_playlists(vec![], &[id]);
                    if let Some(album) = album {
                        self.spotify_album_saved(album, false);
                    }
                }
            }
            Command::SaveAlbum(key) => self.save_album(&key),
            Command::ImportM3u(path) => {
                let shared = self.shared.clone();
                let tx = self.internal_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let result = library::m3u::import(&path, &|id| shared.library.read().unwrap().get(id).cloned());
                    let _ = tx.send(Internal::M3uImported(result));
                });
            }
            Command::ExportM3u { playlist_id, path } => {
                let tracks = {
                    let lib = self.shared.library.read().unwrap();
                    lib.playlist(&playlist_id)
                        .map(|p| lib.tracks_for(&p.track_ids))
                        .unwrap_or_default()
                };
                match library::m3u::export(&path, &tracks) {
                    Ok(n) => self
                        .shared
                        .info(format!("Exported {n} local tracks to {}", path.display())),
                    Err(e) => self.shared.error(format!("Export failed: {e:#}")),
                }
            }
            Command::ImportAppleXml(path) => {
                self.shared.info("Importing Apple Music library…");
                let tx = self.internal_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let r = apple_music::import_library_xml(&path).map(|l| (l.tracks, l.playlists));
                    let _ = tx.send(Internal::AppleImported(r));
                });
            }
            Command::ImportAppleApi => self.import_apple_api(),
            Command::ExportSettings { path, include } => self.export_settings(&path, include),
            Command::ImportSettings(path) => self.import_settings(&path),
            Command::CheckUpdates { manual } => self.check_updates(manual),
            Command::InstallUpdate => self.install_update(),
            Command::AuthorizeUpdate(password) => self.authorize_update(password),
            Command::CancelUpdate => {
                if let Some(file) = self.update_package.take() {
                    let _ = std::fs::remove_file(file);
                }
                self.shared.feed.write().unwrap().update.status = UpdateStatus::Idle;
                self.shared.repaint();
            }
            Command::Rescan => self.start_scan(),
            Command::ListAudioDevices => {
                let binary = crate::tools::resolve(&self.cfg.playback.mpv_path, "mpv");
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let devices = crate::player::mpv::list_audio_devices(&binary).await;
                    let _ = tx.send(Internal::AudioDevices(devices));
                });
            }
            Command::SpotifyLogin => self.spotify_login(),
            Command::SpotifyLogout => {
                self.cancel_fades().await;
                if let Some(sp) = self.spotify.take() {
                    sp.shutdown();
                }
                if self.engine == Engine::Spotify {
                    self.engine = Engine::None;
                    self.set_status(PlayStatus::Stopped);
                }
                self.spotify_auth.logout();
                if let Some(a) = &self.spotify_web_auth {
                    a.logout();
                }
                spotify::clear_credentials(&self.paths.spotify_dir());
                self.set_account(|f| &mut f.spotify, AccountStatus::Off);
            }
            Command::SyncSpotify => self.start_spotify_sync(),
            Command::SpotifyWebApiLogin => self.spotify_web_api_login(),
            Command::SyncSoundCloud => self.start_soundcloud_sync(),
            Command::LastfmLogin => self.lastfm_login(),
            Command::LastfmLogout => {
                self.cfg.lastfm.session_key.clear();
                self.cfg.lastfm.username.clear();
                self.save_config();
                self.lastfm = make_lastfm(&self.cfg, &self.http, &self.paths);
                self.profile_stats = None;
                self.shared.feed.write().unwrap().profile = ProfileFeed::default();
                self.publish_accounts();
            }
            Command::LastfmStats(request) => self.lastfm_stats(request),
            Command::LastfmPictures(wanted) => self.lastfm_pictures(wanted),
            Command::RefreshLastfmProfile => {
                let mut feed = self.shared.feed.write().unwrap();
                for fetch in feed.profile.stats.values_mut() {
                    fetch.at = None;
                    fetch.error = None;
                }
                drop(feed);
                self.shared.repaint();
            }
            Command::Search(q) => self.search(q),
            Command::OpenPage(key) => self.open_page(key),
            Command::Download(tracks) => self.download(tracks),
            Command::ClearDownloads => {
                self.shared.feed.write().unwrap().downloads.retain(|d| d.state.active());
                self.shared.repaint();
            }
            Command::CancelDownload(id) => self.cancel_downloads(Some(&id)),
            Command::CancelDownloads => self.cancel_downloads(None),
            Command::CheckYtDlp(program) => {
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let result = YtDlp::new(&program, "").version().await.map_err(|e| {
                        if e.is::<crate::providers::youtube::NotInstalled>() {
                            "not installed".to_string()
                        } else {
                            format!("{e:#}")
                        }
                    });
                    let _ = tx.send(Internal::YtDlpChecked { program, result });
                });
            }
            Command::UpdateConfig(cfg) => self.update_config(*cfg).await,
            Command::AttachWindow(handle) => {
                // Windows media controls belong to a window.
                if cfg!(windows) {
                    self.mpris = Mpris::new(self.cmd_tx.clone(), Some(handle));
                    self.publish_player();
                }
            }
            Command::Raise => {
                self.shared.feed.write().unwrap().raise = true;
                self.shared.repaint();
            }
            Command::Quit => {}
        }
    }

    // ---------------------------------------------------------------- playback

    fn current_position(&self) -> f64 {
        if self.status == PlayStatus::Playing {
            self.position + self.position_at.elapsed().as_secs_f64()
        } else {
            self.position
        }
    }

    fn set_position(&mut self, secs: f64) {
        self.position = secs.max(0.0);
        self.position_at = Instant::now();
    }

    fn set_status(&mut self, status: PlayStatus) {
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
    async fn play_current(&mut self, start: f64) {
        self.cancel_fades().await;
        self.load_current(start).await;
    }

    /// Loads the queue's current track. During a crossfade it starts silent and fades in.
    async fn load_current(&mut self, start: f64) {
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
    async fn dispatch(&mut self, track: Track, start: f64) {
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
    async fn silence_engine(&mut self) {
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

    async fn dispatch_resolved(&mut self, original: Track, resolved: Option<Track>, start: f64) {
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

    fn cached_resolution(&self, track: &Track) -> Option<Track> {
        self.db
            .get_kv(&format!("resolve:{}", track.id))
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Login used for Web API calls: the user's own app if authorized, else the main login.
    fn web_auth(&self) -> Option<Arc<SpotifyAuth>> {
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
    fn user_web_auth(&self) -> Option<Arc<SpotifyAuth>> {
        if !self.cfg.spotify.enabled {
            return None;
        }
        match &self.spotify_web_auth {
            Some(a) if a.has_user_login() => Some(a.clone()),
            _ => self.spotify_auth.has_login().then(|| self.spotify_auth.clone()),
        }
    }

    /// Spotify search, for finding a SoundCloud upload's release there.
    fn spotify_search(&self) -> Option<(Arc<SpotifyAuth>, Arc<SpotifyApi>)> {
        self.web_auth().map(|auth| (auth, self.spotify_api.clone()))
    }

    fn resolver(&self) -> Resolver {
        Resolver {
            spotify: self.web_auth().map(|auth| (auth, self.spotify_api.clone())),
            soundcloud: self.cfg.soundcloud.enabled.then(|| self.soundcloud.clone()),
        }
    }

    async fn ensure_mpv(&mut self) -> Result<()> {
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

    async fn start_mpv(&mut self, track: Track, url: &str, start: f64) {
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

    async fn start_spotify(&mut self, track: Track, start: f64) {
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

    fn spotify_audio_cache(&self) -> Option<PathBuf> {
        self.cfg
            .spotify
            .cache_audio
            .then(|| self.paths.cache_dir.join("spotify"))
    }

    async fn load_failed(&mut self, error: String) {
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
    fn on_track_started(&mut self) {
        if self.started_reported {
            return;
        }
        self.started_reported = true;
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
        if !resumed {
            let _ = self.db.record_play(&track.id, now);
        }
        {
            let mut lib = self.shared.library.write().unwrap();
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
    fn detect_quality(&mut self) {
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

    async fn mpv_stream_quality(&self) -> Option<AudioQuality> {
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

    async fn pause(&mut self) {
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

    async fn resume(&mut self) {
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

    async fn seek(&mut self, secs: f64) {
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

    async fn stop(&mut self) {
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

    async fn after_queue_change(&mut self) {
        self.publish_queue();
        self.publish_player();
        self.refresh_mpv_preload().await;
    }

    /// Queues the next local file in mpv so the transition is gapless.
    async fn refresh_mpv_preload(&mut self) {
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

    async fn on_mpv(&mut self, id: u64, ev: MpvEvent) {
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

    async fn on_spotify(&mut self, id: u64, ev: SpotifyEvent) {
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
    fn stream_broke_off(&self) -> bool {
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
    async fn reload_at(&mut self, at: f64) {
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
    fn is_playing_uri(&self, uri: &str) -> bool {
        uri.is_empty() || self.playing.as_ref().is_some_and(|t| t.uri == uri)
    }

    async fn on_tick(&mut self) {
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
    fn scrobble_if_due(&mut self) {
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
    fn crossfade_len(&self) -> f64 {
        let p = &self.cfg.playback;
        if p.bit_perfect || p.audio_device.starts_with("alsa/hw:") {
            0.0
        } else {
            f64::from(p.crossfade.clamp(0.0, MAX_CROSSFADE))
        }
    }

    /// True if the current track will crossfade into the next one (rather than gapless).
    fn will_crossfade(&self) -> bool {
        crossfades_into(
            self.crossfade_len(),
            self.cfg.playback.crossfade_albums,
            self.queue.repeat,
            self.queue.current(),
            self.queue.peek_next(),
        )
    }

    /// Whether the fade timer has anything to do.
    fn fade_watch_needed(&self) -> bool {
        self.fading.is_some()
            || self.fade_in.is_some()
            || (self.status == PlayStatus::Playing && self.crossfade_len() > 0.0)
    }

    async fn on_fade_tick(&mut self) {
        self.maybe_begin_crossfade().await;
        self.step_fades().await;
    }

    /// Starts the next track when the current one is about to end.
    async fn maybe_begin_crossfade(&mut self) {
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
    async fn begin_crossfade(&mut self, remaining: f64) {
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
    fn start_fade_ramps(&mut self) {
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

    async fn step_fades(&mut self) {
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
    async fn finish_fading(&mut self) {
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
    async fn cancel_fades(&mut self) {
        self.finish_fading().await;
        if self.fade_in.take().is_some() {
            self.set_engine_volume(self.cfg.playback.volume).await;
        }
    }

    async fn set_engine_volume(&self, volume: f32) {
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
    fn start_volume(&self) -> f32 {
        if self.fade_in.is_some() {
            0.0
        } else {
            self.cfg.playback.volume
        }
    }

    // ---------------------------------------------------------------- publishing

    fn publish_player(&mut self) {
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
        }
        self.mpris.update(
            self.queue.current(),
            self.status == PlayStatus::Playing,
            self.current_position(),
        );
        self.shared.repaint();
    }

    fn publish_queue(&mut self) {
        self.session_dirty = true;
        {
            let mut pv = self.shared.player.write().unwrap();
            pv.upcoming = self.queue.upcoming(200);
            pv.up_next_len = self.queue.up_next_len();
        }
        self.shared.repaint();
    }

    fn update_presence(&self) {
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

    fn set_account(&self, field: impl FnOnce(&mut Feed) -> &mut AccountStatus, status: AccountStatus) {
        {
            let mut feed = self.shared.feed.write().unwrap();
            *field(&mut feed) = status;
        }
        self.shared.repaint();
    }

    fn publish_accounts(&self) {
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

    fn request_lyrics(&self, track: &Track) {
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

    // ---------------------------------------------------------------- library

    fn store_tracks(&mut self, tracks: &[Track]) {
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
    fn save_playlists(&mut self, playlists: Vec<Playlist>, remove: &[String]) {
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
    fn save_album(&mut self, key: &str) {
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
    fn spotify_album_saved(&self, album_id: String, saved: bool) {
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

    fn toggle_like(&mut self, track: Track) {
        self.store_tracks(std::slice::from_ref(&track));
        let (liked_now, playlist) = {
            let mut lib = self.shared.library.write().unwrap();
            if lib.playlist(LIKED_ID).is_none() {
                lib.playlists.insert(0, liked_playlist());
            }
            let p = lib.playlist_mut(LIKED_ID).expect("liked playlist");
            let liked_now = if let Some(i) = p.track_ids.iter().position(|id| id == &track.id) {
                p.track_ids.remove(i);
                false
            } else {
                p.track_ids.insert(0, track.id.clone());
                true
            };
            (liked_now, p.clone())
        };
        self.save_playlists(vec![playlist], &[]);

        if let (Source::Spotify, Some(auth)) = (track.source, self.user_web_auth()) {
            let api = self.spotify_api.clone();
            let t = track.clone();
            let shared = self.shared.clone();
            tokio::spawn(async move {
                let result = match auth.token().await {
                    Ok(token) => api.set_liked(&token, &t.uri, liked_now).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = result {
                    tracing::warn!("Spotify like sync failed: {e:#}");
                    shared.error(format!(
                        "Couldn't {} “{}” on Spotify: {e:#}",
                        if liked_now { "like" } else { "unlike" },
                        t.title
                    ));
                }
            });
        }
        // Loved on Last.fm too when logged in there (songs without an artist can't be).
        if let Some(lfm) = self
            .lastfm
            .clone()
            .filter(|l| l.is_authenticated() && !track.artist.trim().is_empty())
        {
            let shared = self.shared.clone();
            tokio::spawn(async move {
                if let Err(e) = lfm.love(&track, liked_now).await {
                    tracing::warn!("Last.fm love failed: {e:#}");
                    shared.error(format!(
                        "Couldn't {} “{}” on Last.fm",
                        if liked_now { "love" } else { "unlove" },
                        track.title
                    ));
                }
            });
        }
    }

    fn start_scan(&mut self) {
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

    fn merge_scan(&mut self, result: library::scanner::ScanResult) {
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
    fn merge_imported(&mut self, tracks: Vec<Track>, playlists: Vec<Playlist>, prune_kinds: &[PlaylistKind]) {
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
        let keep: HashSet<String> = playlists.iter().map(|p| p.id.clone()).collect();
        let remove: Vec<String> = {
            let lib = self.shared.library.read().unwrap();
            lib.playlists
                .iter()
                .filter(|p| prune_kinds.contains(&p.kind) && !keep.contains(&p.id))
                .map(|p| p.id.clone())
                .collect()
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

    fn spotify_login(&mut self) {
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

    fn connect_spotify(&mut self) {
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

    fn start_spotify_sync(&mut self) {
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
    fn start_spotify_web_sync(&mut self) {
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
    fn check_app_secret(&self) {
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

    fn spotify_web_api_login(&mut self) {
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

    fn merge_spotify(&mut self, sync: SpotifySync) {
        let _ = self.db.set_kv("spotify_user", &sync.user);
        let mut tracks = Vec::new();
        let mut playlists = Vec::new();
        let existing: HashMap<String, Playlist> = {
            let lib = self.shared.library.read().unwrap();
            lib.playlists.iter().map(|p| (p.id.clone(), p.clone())).collect()
        };
        tracks.extend(sync.tracks);
        // What couldn't be loaded this time stays as it was.
        let liked_ids = sync.liked.unwrap_or_else(|| {
            self.shared
                .error("Spotify Liked Songs couldn't be loaded this time; they stay as they were");
            existing
                .get("spotify:liked")
                .map(|p| p.track_ids.clone())
                .unwrap_or_default()
        });
        playlists.push(Playlist {
            id: "spotify:liked".into(),
            name: "Liked Songs".into(),
            kind: PlaylistKind::SpotifyLiked,
            remote_id: None,
            description: "Your liked songs on Spotify".into(),
            art: None,
            track_ids: liked_ids,
        });
        let count = sync.playlists.len();
        for (meta, list) in sync.playlists {
            let id = format!("spotify:{}", meta.id);
            let track_ids = match list {
                Some(ids) => {
                    let _ = self
                        .db
                        .set_kv(&format!("spotify_snapshot:{}", meta.id), &meta.snapshot_id);
                    ids
                }
                None => existing.get(&id).map(|p| p.track_ids.clone()).unwrap_or_default(),
            };
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
        let kept = sync.kept.len();
        playlists.extend(
            sync.kept
                .iter()
                .filter_map(|id| existing.get(&format!("spotify:{id}")).cloned()),
        );
        if kept > 0 {
            self.shared.error(format!(
                "{kept} Spotify playlist{} couldn't be loaded this time and stay as they were",
                if kept == 1 { "" } else { "s" }
            ));
        }
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

    fn soundcloud_configured(&self) -> bool {
        !self.cfg.soundcloud.profile_url.trim().is_empty() || !self.cfg.soundcloud.oauth_token.trim().is_empty()
    }

    fn start_soundcloud_sync(&mut self) {
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

    fn merge_soundcloud(&mut self, sync: SoundCloudSync) {
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
        let (lists, prune) = match sync.playlists {
            Ok(lists) => (lists, &[PlaylistKind::SoundCloud, PlaylistKind::SoundCloudLikes][..]),
            Err(e) => {
                self.shared.error(format!(
                    "SoundCloud playlists couldn't be loaded ({e}); they stay as they were"
                ));
                (Vec::new(), &[PlaylistKind::SoundCloudLikes][..])
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

    fn import_apple_api(&mut self) {
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

    fn merge_apple(&mut self, songs: Vec<Track>, imported: Vec<ImportedPlaylist>) {
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

    // ---------------------------------------------------------------- search, last.fm

    /// Searches Spotify and SoundCloud at the same time; each shows up as soon as it answers.
    fn search(&mut self, query: String) {
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
    fn warm_up_soundcloud(&mut self) {
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

    // ---------------------------------------------------------------- updates

    fn check_updates(&mut self, manual: bool) {
        if !manual && !self.cfg.updates.check {
            return;
        }
        {
            let mut feed = self.shared.feed.write().unwrap();
            if matches!(
                feed.update.status,
                UpdateStatus::Downloading(_) | UpdateStatus::Installing
            ) {
                return;
            }
            if manual {
                feed.update.status = UpdateStatus::Checking;
            }
        }
        self.shared.repaint();
        let repo = self.cfg.updates.repo.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let http = crate::updater::client();
            // Tests point this at a stand-in for GitHub.
            let api = std::env::var("MULTIMUSIC_UPDATE_API").unwrap_or_else(|_| GITHUB_API.to_string());
            let result = crate::updater::latest(&http, &api, &repo)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Internal::UpdateChecked { manual, result });
        });
    }

    fn install_update(&mut self) {
        let release = self.shared.feed.read().unwrap().update.available.clone();
        let Some(release) = release else { return };
        let Some(asset) = release.asset.clone().filter(|_| crate::updater::can_install()) else {
            let why = if crate::updater::can_install() {
                "This release has no download for your system yet"
            } else {
                "Update with: cd ~/localmusicplayer && git pull && makepkg -sif"
            };
            self.shared.feed.write().unwrap().update.status = UpdateStatus::Failed(why.into());
            return;
        };
        self.shared.feed.write().unwrap().update.status = UpdateStatus::Downloading(0.0);
        self.shared.repaint();
        let shared = self.shared.clone();
        let tx = self.internal_tx.clone();
        let cache = self.paths.cache_dir.clone();
        tokio::spawn(async move {
            let result = async {
                let http = crate::updater::client();
                let dir = tokio::task::spawn_blocking(move || crate::updater::fresh_download_dir(&cache)).await??;
                let file = dir.join(&asset.name);
                let last = std::sync::atomic::AtomicU32::new(0);
                crate::updater::download(&http, &asset, &file, |done| {
                    let percent = (done * 100.0) as u32;
                    if last.swap(percent, std::sync::atomic::Ordering::Relaxed) != percent {
                        shared.feed.write().unwrap().update.status = UpdateStatus::Downloading(done);
                        shared.repaint();
                    }
                })
                .await?;
                let sums = release
                    .sums
                    .as_ref()
                    .ok_or_else(|| anyhow!("the release has no checksums, so it can't be checked"))?;
                let sums_file = dir.join(crate::updater::SUMS_FILE);
                crate::updater::download(&http, sums, &sums_file, |_| {}).await?;
                let sums_text = tokio::fs::read_to_string(&sums_file).await?;
                let name = asset.name.clone();
                let installed = tokio::task::spawn_blocking(move || {
                    crate::updater::verify(&file, &name, &sums_text)?;
                    if crate::updater::NEEDS_PASSWORD {
                        // Installed once the password is in (`authorize_update`).
                        return Ok(Some(file));
                    }
                    crate::updater::install(&file).map(|()| None)
                })
                .await?;
                installed
            }
            .await;
            match result {
                Ok(Some(file)) => {
                    let _ = tx.send(Internal::UpdateDownloaded(file));
                }
                Ok(None) => {
                    let mut feed = shared.feed.write().unwrap();
                    feed.update.status = UpdateStatus::Installing;
                    // Quit so the new version can take over (it starts by itself).
                    feed.quit = true;
                    drop(feed);
                    shared.repaint();
                }
                Err(e) => {
                    let _ = tx.send(Internal::UpdateFailed(format!("{e:#}")));
                }
            }
        });
    }

    /// Asks the UI for the password to install the downloaded update.
    fn ask_update_password(&mut self, wrong: bool) {
        self.update_attempt += 1;
        self.shared.feed.write().unwrap().update.status = UpdateStatus::NeedsPassword {
            attempt: self.update_attempt,
            wrong,
        };
        self.shared.repaint();
    }

    /// Installs the downloaded update with the password (Linux: sudo pacman -U).
    fn authorize_update(&mut self, password: crate::updater::Secret) {
        let Some(file) = self.update_package.clone() else {
            return;
        };
        self.shared.feed.write().unwrap().update.status = UpdateStatus::Installing;
        self.shared.repaint();
        let tx = self.internal_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = crate::updater::install_package(&file, &password).map_err(|e| match e {
                crate::updater::InstallError::WrongPassword => None,
                crate::updater::InstallError::Failed(why) => Some(why),
            });
            drop(password);
            let _ = tx.send(Internal::UpdateInstalled(result));
        });
    }

    fn update_installed(&mut self, result: std::result::Result<(), Option<String>>) {
        match result {
            Ok(()) => {
                if let Some(file) = self.update_package.take() {
                    let _ = std::fs::remove_file(file);
                }
                #[cfg(unix)]
                let restarted = crate::updater::relaunch();
                #[cfg(not(unix))]
                let restarted: Result<()> = Err(anyhow!("can't restart here"));
                let mut feed = self.shared.feed.write().unwrap();
                match restarted {
                    Ok(()) => {
                        tracing::info!("update installed; starting again");
                        feed.update.status = UpdateStatus::Installing;
                        feed.quit = true;
                    }
                    Err(e) => {
                        tracing::warn!("update installed, but couldn't start again: {e:#}");
                        feed.update.status = UpdateStatus::Installed;
                    }
                }
                drop(feed);
                self.shared.repaint();
            }
            Err(None) => self.ask_update_password(true),
            Err(Some(why)) => {
                self.shared.feed.write().unwrap().update.status = UpdateStatus::Failed(why);
                self.shared.repaint();
            }
        }
    }

    // ---------------------------------------------------------------- settings files

    fn export_settings(&mut self, path: &Path, include: crate::backup::Include) {
        let file = {
            let lib = self.shared.library.read().unwrap();
            crate::backup::build(&self.cfg, &self.paths.spotify_dir(), &lib, include)
        };
        match crate::backup::write(path, &file) {
            Ok(()) => {
                self.shared.feed.write().unwrap().exported_settings = Some(path.to_path_buf());
                let keys = if include.keys {
                    ". It holds your keys and logins: keep it private"
                } else {
                    ""
                };
                self.shared
                    .info(format!("Saved your settings to {}{keys}", path.display()));
            }
            Err(e) => self.shared.error(format!("Couldn't save your settings: {e:#}")),
        }
    }

    fn import_settings(&mut self, path: &Path) {
        let file = match crate::backup::read(path) {
            Ok(file) => file,
            Err(e) => {
                self.shared.error(format!("Couldn't import settings: {e:#}"));
                return;
            }
        };
        let cfg = crate::backup::merge_config(&self.cfg, &file);
        if file.has_keys {
            let dir = self.paths.spotify_dir();
            let _ = std::fs::create_dir_all(&dir);
            for (name, text) in &file.logins {
                if !crate::backup::LOGIN_FILES.contains(&name.as_str()) {
                    continue;
                }
                let target = dir.join(name);
                if let Err(e) = crate::config::write_private(&target, text) {
                    self.shared.error(format!("Couldn't restore the Spotify login: {e}"));
                }
            }
        }
        if !file.playlists.is_empty() {
            let (playlists, tracks) = {
                let lib = self.shared.library.read().unwrap();
                crate::backup::merge_playlists(&lib, &file)
            };
            self.store_tracks(&tracks);
            self.save_playlists(playlists, &[]);
        }
        self.cfg = cfg.clone();
        if let Err(e) = self.cfg.save(&self.paths) {
            self.shared.error(format!("Couldn't save the imported settings: {e:#}"));
            return;
        }
        // Logins and connections are set up from scratch on the next start.
        self.shared.feed.write().unwrap().imported_settings = Some(Box::new(cfg));
        self.shared.repaint();
    }

    // ---------------------------------------------------------------- playlists

    fn add_to_playlist(&mut self, playlist_id: &str, tracks: &[Track]) {
        self.store_tracks(tracks);
        let updated = {
            let mut lib = self.shared.library.write().unwrap();
            lib.playlist_mut(playlist_id).map(|p| {
                p.track_ids.extend(tracks.iter().map(|t| t.id.clone()));
                if p.art.is_none() {
                    p.art = tracks.iter().find_map(|t| t.art.clone());
                }
                p.clone()
            })
        };
        if let Some(p) = updated {
            self.shared
                .info(format!("Added {} to “{}”", songs_text(tracks.len()), p.name));
            self.save_playlists(vec![p], &[]);
        }
    }

    fn add_links_to_playlist(&mut self, playlist_id: String, links: Vec<String>) {
        use futures_util::StreamExt;
        let spotify_links = links.iter().any(|l| l.contains("spotify"));
        let session = (spotify_links && self.cfg.spotify.enabled && self.spotify_auth.has_login())
            .then(|| self.spotify_session())
            .flatten();
        let soundcloud = self.soundcloud.clone();
        let http = self.http.clone();
        let shared = self.shared.clone();
        let tx = self.internal_tx.clone();
        if links.len() > 1 {
            self.shared
                .info(format!("Adding the songs behind {} links…", links.len()));
        }
        tokio::spawn(async move {
            let lookups = links.into_iter().map(|link| {
                let (session, soundcloud, http, shared) =
                    (session.clone(), soundcloud.clone(), http.clone(), shared.clone());
                async move { songs_behind_link(&link, session.as_ref(), &soundcloud, &http, &shared).await }
            });
            let found: Vec<Option<Vec<Track>>> = futures_util::stream::iter(lookups).buffered(6).collect().await;
            let failed = found.iter().filter(|f| f.is_none()).count();
            let tracks = found.into_iter().flatten().flatten().collect();
            let _ = tx.send(Internal::LinksResolved {
                playlist_id,
                tracks,
                failed,
            });
        });
    }

    // ---------------------------------------------------------------- tag editor

    fn edit_tags(
        &mut self,
        paths: Vec<PathBuf>,
        changes: Vec<(library::tags::Field, String)>,
        cover: Option<CoverChange>,
    ) {
        use library::tags::{self, CoverEdit};
        let http = self.http.clone();
        let folders = self.cfg.library.folders.clone();
        let tx = self.internal_tx.clone();
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let cover = match cover {
                Some(CoverChange::Remove) => Some(CoverEdit::Remove),
                Some(CoverChange::From(src)) => match load_cover(&http, &src).await {
                    Ok(data) => Some(CoverEdit::Set(data)),
                    Err(e) => {
                        shared.error(format!("Couldn't use that cover: {e:#}"));
                        return;
                    }
                },
                None => None,
            };
            let cover_changed = cover.is_some();
            let edited = tokio::task::spawn_blocking(move || {
                let mut tracks = Vec::new();
                let mut errors = Vec::new();
                for path in paths {
                    if let Err(e) = tags::edit(&path, &changes, cover.as_ref()) {
                        errors.push(format!("{e:#}"));
                        continue;
                    }
                    let root = folders.iter().find(|f| path.starts_with(f)).map(PathBuf::as_path);
                    let folder_cover = path.parent().and_then(library::scanner::find_cover);
                    let mut track = library::scanner::read_track_in(&path, root, folder_cover, now_unix());
                    if matches!(cover, Some(CoverEdit::Set(_))) {
                        // Its own new cover, not the folder's picture.
                        track.art = Some(track.uri.clone());
                    }
                    tracks.push((track, library::scanner::mtime_of(&path)));
                }
                (tracks, errors)
            })
            .await;
            let Ok((tracks, errors)) = edited else { return };
            let _ = tx.send(Internal::TagsEdited {
                tracks,
                errors,
                cover_changed,
            });
        });
    }

    fn tags_edited(&mut self, edited: Vec<(Track, i64)>, errors: Vec<String>, cover_changed: bool) {
        let mut mtimes = HashMap::new();
        let mut tracks = Vec::with_capacity(edited.len());
        let mut covers = Vec::new();
        {
            let lib = self.shared.library.read().unwrap();
            for (mut t, mtime) in edited {
                if let Some(old) = lib.tracks.get(&t.id) {
                    if old.added_at != 0 {
                        t.added_at = old.added_at;
                    }
                    if cover_changed {
                        covers.extend(old.art.clone());
                    } else {
                        t.art = old.art.clone();
                    }
                }
                if cover_changed {
                    covers.extend(t.art.clone());
                }
                mtimes.insert(t.id.clone(), mtime);
                tracks.push(t);
            }
        }
        if let Err(e) = self.db.upsert_tracks(&tracks, &mtimes) {
            self.shared.error(format!("Database error: {e:#}"));
        }
        let updated: HashMap<String, Track> = tracks.into_iter().map(|t| (t.id.clone(), t)).collect();
        {
            let mut lib = self.shared.library.write().unwrap();
            for t in updated.values() {
                lib.tracks.insert(t.id.clone(), t.clone());
            }
            lib.reindex();
        }
        self.queue.refresh(&updated);
        if let Some(new) = self.playing.as_ref().and_then(|p| updated.get(&p.id)) {
            self.playing = Some(new.clone());
        }
        self.publish_player();
        self.publish_queue();
        if cover_changed {
            let mut feed = self.shared.feed.write().unwrap();
            feed.art_changed.0 += 1;
            feed.art_changed.1 = covers;
        }
        // The playing song's lyrics may have changed (they come from its file first).
        if let Some(current) = self.queue.current().filter(|t| updated.contains_key(&t.id)).cloned() {
            self.shared.feed.write().unwrap().lyrics.track_id.clear();
            self.request_lyrics(&current);
            self.update_presence();
        }
        if !updated.is_empty() {
            self.shared
                .info(format!("Saved the details of {}", songs_text(updated.len())));
        }
        if let Some(first) = errors.first() {
            let more = match errors.len() {
                1 => String::new(),
                n => format!(" (and {} more)", n - 1),
            };
            self.shared.error(format!("{first}{more}"));
        }
    }

    fn look_up_tags(&mut self, request: u64, track: Track) {
        let tx = self.internal_tx.clone();
        let Some((auth, api)) = self.spotify_search() else {
            let _ = tx.send(Internal::TagLookup {
                request,
                result: Err("Log in to Spotify in Settings to look songs up".into()),
            });
            return;
        };
        let session = self.spotify.as_ref().and_then(|e| e.session());
        tokio::spawn(async move {
            let result = spotify_tags(&auth, &api, session.as_ref(), &track).await;
            let _ = tx.send(Internal::TagLookup { request, result });
        });
    }

    // ---------------------------------------------------------------- pages

    fn set_page(&self, page: PageState) {
        let mut feed = self.shared.feed.write().unwrap();
        if feed.page.key == page.key {
            feed.page = page;
        }
        drop(feed);
        self.shared.repaint();
    }

    fn page_failed(&self, key: &str, error: impl Into<String>) {
        self.set_page(PageState {
            key: key.to_string(),
            error: Some(error.into()),
            ..Default::default()
        });
    }

    fn open_page(&mut self, key: String) {
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

    fn load_page(&mut self, key: String, target: Target) {
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

    // ---------------------------------------------------------------- downloads

    fn load_downloads(&mut self) {
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
    fn file_of(&self, track: &Track) -> Option<String> {
        match track.source {
            Source::Local => Some(track.uri.clone()),
            _ => self.downloaded_file(track),
        }
    }

    fn downloaded_file(&self, track: &Track) -> Option<String> {
        let feed = self.shared.feed.read().unwrap();
        let path = feed.downloaded.get(&track.id)?;
        path.exists().then(|| path.to_string_lossy().into_owned())
    }

    fn download(&mut self, tracks: Vec<Track>) {
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
    fn pump_downloads(&mut self) {
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

    fn download_finished(&mut self, track: Track, result: Result<Saved>) {
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
    fn cancel_downloads(&mut self, id: Option<&str>) {
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
    fn clear_download_leftovers(&self) {
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
    fn add_downloaded_file(&mut self, path: &Path) {
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
    fn spotify_lookup(&mut self) -> Option<SpotifyLookup> {
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
    fn spotify_session(&mut self) -> Option<librespot_core::session::Session> {
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

    fn lastfm_login(&mut self) {
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

    async fn update_config(&mut self, mut cfg: Config) {
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

    fn save_config(&self) {
        if let Err(e) = self.cfg.save(&self.paths) {
            self.shared.error(format!("Couldn't save settings: {e:#}"));
        }
    }

    // ---------------------------------------------------------------- session

    fn save_session(&mut self) {
        if self.queue.current().is_none() {
            return;
        }
        let extra: Vec<Track> = {
            let lib = self.shared.library.read().unwrap();
            let mut seen = HashSet::new();
            self.queue
                .all_tracks()
                .filter(|t| !lib.tracks.contains_key(&t.id) && seen.insert(t.id.as_str()))
                .cloned()
                .collect()
        };
        let session = SavedSession {
            tracks: Vec::new(),
            index: 0,
            position: self.current_position(),
            context: self.queue.context_name.clone(),
            shuffle: self.queue.shuffle,
            repeat: self.queue.repeat,
            queue: Some(self.queue.state()),
            extra,
        };
        if let Ok(text) = serde_json::to_string(&session) {
            let _ = self.db.set_kv("session", &text);
        }
        self.save_position();
    }

    fn save_position(&mut self) {
        self.position_saved_at = Instant::now();
        let Some(track) = self.queue.current() else {
            return;
        };
        let saved = SavedPosition {
            track: track.id.clone(),
            position: self.current_position(),
            scrobbled: self.scrobble.scrobbled(&track.id),
        };
        if let Ok(text) = serde_json::to_string(&saved) {
            let _ = self.db.set_kv("session_position", &text);
        }
    }

    fn restore_session(&mut self) {
        let Some(session) = self
            .db
            .get_kv("session")
            .and_then(|s| serde_json::from_str::<SavedSession>(&s).ok())
        else {
            return;
        };
        let restored = match session.queue.clone() {
            Some(state) => {
                let lib = self.shared.library.read().unwrap();
                let extra: HashMap<&str, &Track> = session.extra.iter().map(|t| (t.id.as_str(), t)).collect();
                let song = |id: &str| lib.tracks.get(id).or_else(|| extra.get(id).copied()).cloned();
                self.queue.restore(state, song)
            }
            None => false,
        };
        if !restored {
            // A session from before 0.5.3, or one whose songs are gone: what is still there.
            let ids: Vec<String> = match &session.queue {
                Some(state) => state.current.iter().chain(&state.tracks).cloned().collect(),
                None => session.tracks.clone(),
            };
            let tracks = self.shared.library.read().unwrap().tracks_for(&ids);
            if tracks.is_empty() {
                return;
            }
            self.queue.repeat = session.repeat;
            self.queue.set_context(tracks, session.index, session.context);
            self.queue.shuffle = session.shuffle;
        }
        let Some(current) = self.queue.current().cloned() else {
            return;
        };
        self.duration = current.duration_secs();
        self.request_lyrics(&current);
        // The position saved during playback is newer than the one saved with the queue.
        let saved = self
            .db
            .get_kv("session_position")
            .and_then(|s| serde_json::from_str::<SavedPosition>(&s).ok())
            .filter(|p| p.track == current.id);
        let position = saved.as_ref().map_or(session.position, |p| p.position);
        if saved.is_some_and(|p| p.scrobbled) {
            self.resumed_scrobbled = Some(current.id.clone());
        }
        self.resume_position = Some(position);
        self.set_position(position);
        self.status = PlayStatus::Paused;
    }

    // ---------------------------------------------------------------- internal results

    async fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::StreamReady { seq, track, url } => {
                if seq == self.load_seq {
                    let start = self.position;
                    self.start_mpv(track, &url, start).await;
                }
            }
            Internal::Resolved {
                seq,
                original,
                resolved,
            } => {
                if seq == self.load_seq {
                    let start = self.position;
                    self.dispatch_resolved(original, resolved, start).await;
                }
            }
            Internal::LoadFailed { seq, error } => {
                if seq == self.load_seq {
                    self.load_failed(error).await;
                }
            }
            Internal::SpotifyReady(r) => {
                self.spotify_connecting = false;
                match r {
                    Ok(engine) => {
                        if self.spotify.is_none() {
                            self.spotify = Some(engine);
                        } else {
                            engine.shutdown();
                        }
                        if let Some((seq, track, start)) = self.spotify_pending_play.take() {
                            if seq == self.load_seq {
                                self.start_spotify(track, start).await;
                            }
                        }
                        if std::mem::take(&mut self.spotify_sync_pending) {
                            self.start_spotify_sync();
                        } else if !self.spotify_syncing {
                            let name = self.db.get_kv("spotify_user").unwrap_or_default();
                            self.set_account(|f| &mut f.spotify, AccountStatus::Connected(name));
                        }
                        if let Some(key) = self.spotify_pending_page.take() {
                            self.open_page(key);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Spotify connect failed: {e:#}");
                        if let Some((seq, ..)) = self.spotify_pending_play.take() {
                            if seq == self.load_seq {
                                self.load_failed(format!("Spotify: {e:#}")).await;
                            }
                        }
                        self.set_account(
                            |f| &mut f.spotify,
                            AccountStatus::Error(format!("Couldn't connect: {e:#}")),
                        );
                        if std::mem::take(&mut self.spotify_sync_pending) {
                            self.start_spotify_web_sync();
                        }
                        if let Some(key) = self.spotify_pending_page.take() {
                            self.page_failed(&key, format!("Couldn't connect to Spotify: {e:#}"));
                        }
                    }
                }
            }
            Internal::SpotifySynced(r) => {
                self.spotify_syncing = false;
                match r {
                    Ok(sync) => self.merge_spotify(sync),
                    Err(e) => {
                        self.set_account(|f| &mut f.spotify, AccountStatus::Error(format!("Sync failed: {e:#}")));
                        self.shared.error(format!("Spotify sync failed: {e:#}"));
                    }
                }
            }
            Internal::SoundCloudSynced(r) => match r {
                Ok(sync) => self.merge_soundcloud(sync),
                Err(e) => {
                    self.set_account(|f| &mut f.soundcloud, AccountStatus::Error(format!("{e:#}")));
                    self.shared.error(format!("SoundCloud sync failed: {e:#}"));
                }
            },
            Internal::AppleImported(r) => match r {
                Ok((songs, playlists)) => self.merge_apple(songs, playlists),
                Err(e) => self.shared.error(format!("Apple Music import failed: {e:#}")),
            },
            Internal::UpdateChecked { manual, result } => {
                let mut feed = self.shared.feed.write().unwrap();
                let current = env!("CARGO_PKG_VERSION");
                match result {
                    Ok(release) if crate::updater::is_newer(&release.version, current) => {
                        tracing::info!("Sumo {} is available", release.version);
                        feed.update.available = Some(release);
                        feed.update.status = UpdateStatus::Idle;
                    }
                    Ok(_) => {
                        feed.update.available = None;
                        feed.update.status = if manual {
                            UpdateStatus::UpToDate
                        } else {
                            UpdateStatus::Idle
                        };
                    }
                    Err(e) => {
                        tracing::info!("update check: {e}");
                        feed.update.status = if manual {
                            UpdateStatus::Failed(e)
                        } else {
                            UpdateStatus::Idle
                        };
                    }
                }
                drop(feed);
                self.shared.repaint();
            }
            Internal::UpdateFailed(e) => {
                self.shared.feed.write().unwrap().update.status = UpdateStatus::Failed(e);
                self.shared.repaint();
            }
            Internal::UpdateDownloaded(file) => {
                self.update_package = Some(file);
                self.ask_update_password(false);
            }
            Internal::UpdateInstalled(result) => self.update_installed(result),
            Internal::TagsEdited {
                tracks,
                errors,
                cover_changed,
            } => self.tags_edited(tracks, errors, cover_changed),
            Internal::TagLookup { request, result } => {
                self.shared.feed.write().unwrap().tag_lookup = Some((request, result));
                self.shared.repaint();
            }
            Internal::LinksResolved {
                playlist_id,
                tracks,
                failed,
            } => {
                if !tracks.is_empty() {
                    self.add_to_playlist(&playlist_id, &tracks);
                }
                if failed > 0 {
                    let spotify = if self.spotify_auth.has_login() {
                        ""
                    } else {
                        " (Spotify links need a Spotify login)"
                    };
                    self.shared.error(format!(
                        "Couldn't find the songs behind {failed} pasted link{}{spotify}",
                        if failed == 1 { "" } else { "s" }
                    ));
                }
            }
            Internal::M3uImported(r) => match r {
                Ok(p) => {
                    let name = p.name.clone();
                    let playlist = Playlist {
                        id: format!("m3u:{}", p.remote_id),
                        name: p.name,
                        kind: PlaylistKind::M3u,
                        remote_id: Some(p.remote_id),
                        description: p.description,
                        art: p.art,
                        track_ids: p.tracks.iter().map(|t| t.id.clone()).collect(),
                    };
                    self.merge_imported(p.tracks, vec![playlist], &[]);
                    self.shared.info(format!("Imported playlist “{name}”"));
                }
                Err(e) => self.shared.error(format!("Import failed: {e:#}")),
            },
            Internal::ScanDone(result) => self.merge_scan(result),
            Internal::Lyrics { track_id, lyrics } => {
                let mut feed = self.shared.feed.write().unwrap();
                if feed.lyrics.track_id == track_id {
                    feed.lyrics.loading = false;
                    feed.lyrics.lyrics = lyrics;
                }
                drop(feed);
                self.shared.repaint();
            }
            Internal::SearchResults {
                query,
                source,
                tracks,
                artists,
                albums,
                playlists,
            } => {
                let mut feed = self.shared.feed.write().unwrap();
                let search = &mut feed.search;
                if search.query == query {
                    match (source, tracks) {
                        (Source::Spotify, Ok(t)) => search.spotify = t,
                        (_, Ok(t)) => search.soundcloud = t,
                        (Source::Spotify, Err(e)) => {
                            search.errors.push(format!("Spotify: {}", friendly_spotify_error(&e)))
                        }
                        (_, Err(e)) => search
                            .errors
                            .push(format!("{}: {}", source.label(), friendly_net_error(&e))),
                    }
                    match source {
                        Source::Spotify => search.spotify_pending = false,
                        _ => search.soundcloud_pending = false,
                    }
                    search.artists.extend(artists);
                    rank_artists(&mut search.artists, &query);
                    // Spotify's and SoundCloud's take turns, so both show up front.
                    search.albums = interleave(std::mem::take(&mut search.albums), albums);
                    search.playlists = interleave(std::mem::take(&mut search.playlists), playlists);
                }
                drop(feed);
                self.shared.repaint();
            }
            Internal::SoundCloudClientId(id) => {
                if self.db.get_kv("soundcloud_client_id").as_deref() != Some(id.as_str()) {
                    let _ = self.db.set_kv("soundcloud_client_id", &id);
                }
            }
            Internal::PageLoaded { key, result } => {
                let page = match result {
                    Ok(mut page) => {
                        page.key = key;
                        self.page_cache.retain(|p| p.key != page.key);
                        self.page_cache.push(page.clone());
                        if self.page_cache.len() > 8 {
                            self.page_cache.remove(0);
                        }
                        page
                    }
                    Err(e) => {
                        tracing::warn!("loading page {key}: {e:#}");
                        PageState {
                            key,
                            error: Some(friendly_page_error(&e)),
                            ..Default::default()
                        }
                    }
                };
                self.set_page(page);
            }
            Internal::ShortLink { key, result } => {
                if self.shared.feed.read().unwrap().page.key != key {
                    return;
                }
                match result.map(|url| links::target(&url)) {
                    Ok(Some(target)) if !matches!(target, Target::Short(_)) => self.load_page(key, target),
                    Ok(_) => self.page_failed(
                        &key,
                        "That short link doesn't lead to a song, album, playlist or artist",
                    ),
                    Err(e) => self.page_failed(&key, format!("Couldn't open the link: {e:#}")),
                }
            }
            Internal::Quality { track_id, quality } => {
                if self.playing.as_ref().is_some_and(|t| t.id == track_id) {
                    self.quality = quality;
                    self.publish_player();
                }
            }
            Internal::SyncProgress(text) => {
                if self.spotify_syncing {
                    self.set_account(|f| &mut f.spotify, AccountStatus::Working(text));
                }
            }
            Internal::AudioDevices(devices) => {
                self.shared.feed.write().unwrap().audio_devices = devices;
                self.shared.repaint();
            }
            Internal::DownloadDone { track, result } => self.download_finished(track, result),
            Internal::YtDlpChecked { program, result } => {
                self.shared.feed.write().unwrap().ytdlp = Some((program, result));
                self.shared.repaint();
            }
            Internal::LastfmSession(r) => match r {
                Ok((key, user)) => {
                    self.cfg.lastfm.session_key = key;
                    self.cfg.lastfm.username = user.clone();
                    self.cfg.lastfm.enabled = true;
                    self.save_config();
                    self.lastfm = make_lastfm(&self.cfg, &self.http, &self.paths);
                    self.set_account(|f| &mut f.lastfm, AccountStatus::Connected(user));
                }
                Err(e) => self.set_account(|f| &mut f.lastfm, AccountStatus::Error(format!("{e:#}"))),
            },
            Internal::LastfmStats { user, request, result } => {
                let mut feed = self.shared.feed.write().unwrap();
                if feed.profile.user == user {
                    let fetch = feed.profile.stats.entry(request).or_default();
                    fetch.loading = false;
                    fetch.at = Some(Instant::now());
                    match result {
                        Ok(data) => {
                            fetch.data = Some(data);
                            fetch.error = None;
                        }
                        Err(e) => {
                            tracing::warn!("lastfm: {request:?}: {e}");
                            fetch.error = Some(e);
                        }
                    }
                }
                drop(feed);
                self.shared.repaint();
            }
            Internal::LastfmPicture {
                user,
                artist,
                title,
                found,
            } => {
                let mut feed = self.shared.feed.write().unwrap();
                if feed.profile.user == user {
                    let key = crate::integrations::lastfm_stats::art_key(&artist, &title);
                    feed.profile
                        .art
                        .insert(key, found.map(ArtLookup::Found).unwrap_or(ArtLookup::Missing));
                }
                drop(feed);
                self.shared.repaint();
            }
        }
    }

    /// The stats loader for the signed-in Last.fm account (`None` when not signed in or the
    /// API key is missing). Switching accounts starts the profile page over.
    fn profile_stats(&mut self) -> Option<Arc<ProfileStats>> {
        let lastfm = self.lastfm.clone()?;
        let user = self.cfg.lastfm.username.trim().to_string();
        if user.is_empty() || self.cfg.lastfm.session_key.is_empty() {
            return None;
        }
        if let Some(stats) = self.profile_stats.as_ref().filter(|s| s.serves(&lastfm, &user)) {
            return Some(stats.clone());
        }
        let stats = Arc::new(ProfileStats::new(lastfm, &user));
        self.profile_stats = Some(stats.clone());
        let mut feed = self.shared.feed.write().unwrap();
        if feed.profile.user != user {
            feed.profile = ProfileFeed {
                user,
                ..ProfileFeed::default()
            };
        }
        Some(stats)
    }

    fn lastfm_stats(&mut self, request: StatsRequest) {
        let Some(stats) = self.profile_stats() else {
            // Say why instead of being asked again and again.
            let why = if self.lastfm.is_none() {
                "Add your Last.fm API key and secret in Settings"
            } else {
                "Connect your Last.fm account in Settings"
            };
            let mut feed = self.shared.feed.write().unwrap();
            let fetch = feed.profile.stats.entry(request).or_default();
            if fetch.wanted(request) {
                fetch.error = Some(why.into());
                fetch.at = Some(Instant::now());
            }
            return;
        };
        {
            let mut feed = self.shared.feed.write().unwrap();
            let fetch = feed.profile.stats.entry(request).or_default();
            if !fetch.wanted(request) {
                return;
            }
            fetch.loading = true;
        }
        self.shared.repaint();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = stats.load(request).await.map_err(|e| friendly_lastfm_error(&e));
            let _ = tx.send(Internal::LastfmStats {
                user: stats.user.clone(),
                request,
                result,
            });
        });
    }

    fn lastfm_pictures(&mut self, wanted: Vec<(String, String)>) {
        let Some(stats) = self.profile_stats() else { return };
        let mut feed = self.shared.feed.write().unwrap();
        for (artist, title) in wanted {
            let key = crate::integrations::lastfm_stats::art_key(&artist, &title);
            if feed.profile.art.contains_key(&key) || artist.trim().is_empty() {
                continue;
            }
            feed.profile.art.insert(key, ArtLookup::Pending);
            let stats = stats.clone();
            let tx = self.internal_tx.clone();
            tokio::spawn(async move {
                let found = stats.picture(&artist, &title).await;
                let _ = tx.send(Internal::LastfmPicture {
                    user: stats.user.clone(),
                    artist,
                    title,
                    found,
                });
            });
        }
    }
}

/// Why the profile page couldn't load something, in a few words.
fn friendly_lastfm_error(e: &anyhow::Error) -> String {
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

/// Database key prefix of downloaded songs (`download:<track id>` → file path).
const DOWNLOAD_KEY: &str = "download:";

/// Short reason for the Downloads list (the song's title is shown next to it).
fn friendly_download_error(e: &anyhow::Error) -> String {
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

fn make_lastfm(cfg: &Config, http: &reqwest::Client, paths: &Paths) -> Option<Arc<Lastfm>> {
    let l = &cfg.lastfm;
    if l.api_key.trim().is_empty() || l.api_secret.trim().is_empty() {
        return None;
    }
    let lfm = Lastfm::new(
        http.clone(),
        l.api_key.trim(),
        l.api_secret.trim(),
        if l.enabled { l.session_key.trim() } else { "" },
        paths.data_dir.join("scrobble-queue.json"),
    )
    .with_genius(genius::shared());
    Some(Arc::new(lfm))
}

/// Imports playlists and Liked Songs through the playback session (not rate limited like the
/// public Web API). Only tracks we don't know yet get their metadata fetched.
async fn spotify_sync_internal(
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
async fn spotify_sync_web(
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

/// Fraction (0..=1) of a fade of `len` seconds that started at `start`.
fn fade_progress(start: Instant, len: f64, now: Instant) -> f32 {
    if len <= 0.0 {
        return 1.0;
    }
    (now.saturating_duration_since(start).as_secs_f64() / len).clamp(0.0, 1.0) as f32
}

/// Smooth start and end of a fade (smoothstep).
fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// How early to start a crossfade so the next track has time to load.
fn load_lead(source: Source) -> f64 {
    match source {
        Source::Local => 0.3,
        Source::Spotify => 1.5,
        Source::SoundCloud => 2.0,
        Source::AppleMusic => 2.5,
    }
}

/// Whether `current` should crossfade into `next`. Songs of the same album stay gapless
/// unless `same_album_too`; repeat-one never fades a song into itself.
fn crossfades_into(
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

fn same_album(a: &Track, b: &Track) -> bool {
    let album = a.album.trim();
    !album.is_empty()
        && album.eq_ignore_ascii_case(b.album.trim())
        && normalize_artist(&a.artist) == normalize_artist(&b.artist)
}

/// Exact and prefix name matches first; Spotify before SoundCloud on ties. Stable, so each
/// service's own relevance order is kept.
/// The library id of a saved album.
pub fn saved_album_id(kind: PlaylistKind, remote: &str) -> String {
    match kind {
        PlaylistKind::SpotifyAlbum => format!("spotify_album:{remote}"),
        _ => format!("soundcloud_album:{remote}"),
    }
}

/// `a` and `b` taking turns (what's left of the longer one at the end).
fn interleave<T>(a: Vec<T>, b: Vec<T>) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut a, mut b) = (a.into_iter(), b.into_iter());
    loop {
        match (a.next(), b.next()) {
            (None, None) => return out,
            (x, y) => out.extend(x.into_iter().chain(y)),
        }
    }
}

fn rank_artists(artists: &mut [ArtistHit], query: &str) {
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

async fn spotify_page(session: librespot_core::session::Session, kind: LinkKind, id: &str) -> Result<PageState> {
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

/// Where the tag editor's new cover comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum CoverChange {
    /// An image file or a link to one.
    From(String),
    Remove,
}

/// "1 song" / "3 songs".
fn songs_text(n: usize) -> String {
    if n == 1 {
        "1 song".into()
    } else {
        format!("{n} songs")
    }
}

/// Cover data for a file's tags from an image file or link.
async fn load_cover(http: &reqwest::Client, src: &str) -> Result<Vec<u8>> {
    let src = src.trim();
    let bytes = if src.starts_with("http://") || src.starts_with("https://") {
        let resp = http.get(src).send().await?.error_for_status()?;
        crate::http::read_capped(resp, crate::http::MAX_IMAGE).await?
    } else {
        let path = local_path(src).unwrap_or_else(|| PathBuf::from(src));
        tokio::fs::read(&path)
            .await
            .map_err(|e| anyhow!("couldn't read {}: {e}", path.display()))?
    };
    tokio::task::spawn_blocking(move || library::tags::cover_data(bytes)).await?
}

/// What Spotify knows about a song, for the tag editor: the full details when the playback
/// session is up, else what its search result says.
async fn spotify_tags(
    auth: &SpotifyAuth,
    api: &SpotifyApi,
    session: Option<&librespot_core::session::Session>,
    track: &Track,
) -> Result<library::tags::Metadata, String> {
    let (artist, title) = crate::integrations::lyrics::song_names(track);
    let unknown = artist.is_empty() || artist == library::scanner::UNKNOWN_ARTIST;
    let query = if unknown {
        title.clone()
    } else {
        format!("{artist} {title}")
    };
    let token = auth.token().await.map_err(|e| format!("Spotify: {e:#}"))?;
    let results = api
        .search(&token, &query, 10)
        .await
        .map_err(|e| format!("Spotify search failed: {e:#}"))?;
    let wanted = Track {
        artist: if unknown { String::new() } else { artist.clone() },
        title: title.clone(),
        ..track.clone()
    };
    let found = if unknown {
        same_title_and_length(&wanted, &results)
    } else {
        best_match(&wanted, &results)
    };
    let Some(found) = found else {
        let by = if unknown {
            String::new()
        } else {
            format!(" by {artist}")
        };
        return Err(format!("Spotify has no song called “{title}”{by}"));
    };
    if let Some(session) = session {
        let details = crate::providers::spotify_internal::track_details(session, &found.id);
        if let Ok(Ok(meta)) = tokio::time::timeout(Duration::from_secs(20), details).await {
            if !meta.title.is_empty() {
                return Ok(meta);
            }
        }
    }
    Ok(downloader::basic_metadata(&found))
}

/// A file path or `file://` link (as file managers copy them) as a path; `None` for anything
/// else. `file:///C:/Music/a.mp3` is `C:/Music/a.mp3` on Windows.
pub fn local_path(line: &str) -> Option<PathBuf> {
    let line = line.trim();
    let path = match line.strip_prefix("file://") {
        Some(rest) => {
            let decoded = urlencoding::decode(rest).map_or_else(|_| rest.to_string(), |p| p.into_owned());
            let bytes = decoded.as_bytes();
            // "/C:/…" → "C:/…"
            if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' && bytes[1].is_ascii_alphabetic() {
                decoded[1..].to_string()
            } else {
                decoded
            }
        }
        None => line.to_string(),
    };
    let path = PathBuf::from(path);
    (path.is_absolute() || path.has_root()).then_some(path)
}

/// The songs a pasted line stands for: a Spotify or SoundCloud song, album or playlist link, or
/// a music file (a path or a `file://` link from a file manager). `None` when it can't be found.
async fn songs_behind_link(
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
async fn lastfm_track(spotify: Option<(Arc<SpotifyAuth>, Arc<SpotifyApi>)>, track: Track) -> Track {
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
fn same_title_and_length(target: &Track, candidates: &[Track]) -> Option<Track> {
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
struct SpotifyLookup {
    auth: Arc<SpotifyAuth>,
    api: Arc<SpotifyApi>,
    session: librespot_core::session::Session,
}

/// An artist found on one service by name.
struct ArtistFound {
    source: Source,
    image: Option<String>,
    url: String,
    tracks: Vec<Track>,
}

/// Same artist name, ignoring case, spaces and punctuation ("Yung Lean" = "yunglean").
fn same_artist_name(a: &str, b: &str) -> bool {
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
async fn soundcloud_artist(sc: &SoundCloud, name: &str) -> Option<ArtistFound> {
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
async fn spotify_artist(l: &SpotifyLookup, name: &str) -> Option<ArtistFound> {
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
async fn with_spotify_songs(
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
fn artist_subtitle(tracks: &[Track]) -> String {
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
fn song_key(t: &Track) -> String {
    normalize_title(&crate::integrations::lastfm::scrobble_names(t).1)
}

/// Whether `a` and `b` are the same song: same title and about the same length (a little
/// looser across services, which encode differently).
#[cfg(test)]
fn same_song(a: &Track, b: &Track) -> bool {
    same_song_keyed(&song_key(a), a, &song_key(b), b)
}

fn same_song_keyed(ka: &str, a: &Track, kb: &str, b: &Track) -> bool {
    if ka.is_empty() || ka != kb {
        return false;
    }
    let tolerance = if a.source == b.source { 4_000 } else { 12_000 };
    a.duration_ms == 0 || b.duration_ms == 0 || a.duration_ms.abs_diff(b.duration_ms) <= tolerance
}

/// Appends the songs of `extra` that `tracks` doesn't have yet. Returns how many were added.
pub fn add_songs(tracks: &mut Vec<Track>, extra: Vec<Track>) -> usize {
    let mut keys: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        keys.entry(song_key(t)).or_default().push(i);
    }
    let mut added = 0;
    for t in extra {
        let key = song_key(&t);
        let known = keys
            .get(&key)
            .is_some_and(|ids| ids.iter().any(|&i| same_song_keyed(&key, &tracks[i], &key, &t)));
        if !known {
            keys.entry(key).or_default().push(tracks.len());
            tracks.push(t);
            added += 1;
        }
    }
    added
}

/// The list without repeats (a single that is also on the album, the same upload twice).
pub fn dedupe_songs(tracks: Vec<Track>) -> Vec<Track> {
    let mut out = Vec::with_capacity(tracks.len());
    add_songs(&mut out, tracks);
    out
}

fn soundcloud_user_page(user: &ScUser, tracks: Vec<Track>) -> PageState {
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
async fn expand_short_link(http: &reqwest::Client, url: &str) -> Result<String> {
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

fn friendly_page_error(e: &anyhow::Error) -> String {
    if apple_music::is_auth_error(e) {
        "Apple Music refused the request. Try again, or paste a developer token in Settings → Apple Music.".into()
    } else {
        friendly_net_error(e)
    }
}

/// Connection problems get a short message instead of a chain of wrapped errors.
fn friendly_net_error(e: &anyhow::Error) -> String {
    let offline = e.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|r| r.is_connect() || r.is_timeout())
    });
    if offline {
        format!("couldn't connect ({e}). Check your internet connection.")
    } else {
        format!("{e:#}")
    }
}

/// Turns Web API errors into something actionable.
fn friendly_spotify_error(e: &anyhow::Error) -> String {
    if spotify_api::error_status(e) == Some(429) || format!("{e:#}").contains("429") {
        "Spotify's shared Web API key is rate limited right now. For reliable search, add your own \
         Spotify app in Settings → Spotify → Advanced."
            .into()
    } else {
        format!("{e:#}")
    }
}

/// Finds a playable version of an imported (Apple Music) song on Spotify or SoundCloud.
struct Resolver {
    spotify: Option<(Arc<SpotifyAuth>, Arc<SpotifyApi>)>,
    soundcloud: Option<Arc<SoundCloud>>,
}

impl Resolver {
    async fn resolve(&self, track: &Track) -> Option<Track> {
        let first_artist = track.artist.split([',', '&']).next().unwrap_or("").trim();
        let query = format!("{} {}", first_artist, track.title);
        if let Some((auth, api)) = &self.spotify {
            if let Ok(token) = auth.token().await {
                if let Ok(results) = api.search(&token, &query, 10).await {
                    if let Some(t) = best_match(track, &results) {
                        return Some(t);
                    }
                }
            }
        }
        if let Some(sc) = &self.soundcloud {
            if let Ok(results) = sc.search(&query, 10).await {
                if let Some(t) = best_match(track, &results) {
                    return Some(t);
                }
            }
        }
        None
    }
}

/// Picks the candidate that is clearly the same song (title + artist, close duration).
pub fn best_match(target: &Track, candidates: &[Track]) -> Option<Track> {
    ranked_matches(target, candidates).into_iter().next()
}

/// Candidates that are the same song as `target`, best first.
pub fn ranked_matches(target: &Track, candidates: &[Track]) -> Vec<Track> {
    let title = normalize_title(&target.title);
    let artist = normalize_artist(&target.artist);
    let mut matches: Vec<(&Track, u64)> = candidates
        .iter()
        .filter_map(|c| {
            let ct = normalize_title(&c.title);
            let ca = normalize_artist(&c.artist);
            let title_ok = ct == title || (!title.is_empty() && (ct.contains(&title) || title.contains(&ct)));
            // SoundCloud uploads often put the artist in the title instead.
            let artist_ok = ca == artist
                || (!artist.is_empty() && (ca.contains(&artist) || artist.contains(&ca) || ct.contains(&artist)));
            if !title_ok || !artist_ok {
                return None;
            }
            let dur_diff = if target.duration_ms > 0 && c.duration_ms > 0 {
                (target.duration_ms as i64 - c.duration_ms as i64).unsigned_abs()
            } else {
                0
            };
            if dur_diff > 10_000 {
                return None;
            }
            let exact = (ct == title) as u64 + (ca == artist) as u64;
            Some((c, (2 - exact) * 100_000 + dur_diff))
        })
        .collect();
    matches.sort_by_key(|(_, score)| *score);
    matches.into_iter().map(|(c, _)| c.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossfade_decisions() {
        let mut a = track(Source::Local, "Neon Coast", "Intro", 200_000);
        a.album = "Midnight Drive".into();
        let mut b = track(Source::Spotify, "Neon Coast", "Afterglow", 180_000);
        b.album = "Midnight Drive".into();
        let mut c = track(Source::SoundCloud, "Bladee", "Topman", 194_000);
        c.album = "Icedancer".into();
        // Different albums (and sources) fade; the same album stays gapless unless asked.
        assert!(crossfades_into(6.0, false, RepeatMode::Off, Some(&a), Some(&c)));
        assert!(!crossfades_into(6.0, false, RepeatMode::Off, Some(&a), Some(&b)));
        assert!(crossfades_into(6.0, true, RepeatMode::Off, Some(&a), Some(&b)));
        // Off, nothing next, or repeat-one: no crossfade.
        assert!(!crossfades_into(0.0, true, RepeatMode::Off, Some(&a), Some(&c)));
        assert!(!crossfades_into(6.0, true, RepeatMode::Off, Some(&a), None));
        assert!(!crossfades_into(6.0, true, RepeatMode::One, Some(&a), Some(&a)));
        assert!(crossfades_into(6.0, false, RepeatMode::All, Some(&c), Some(&a)));
        // "Greatest Hits" by two artists isn't one album.
        let mut d = track(Source::Local, "Other Band", "Song", 100_000);
        d.album = "midnight drive".into();
        assert!(same_album(&a, &b));
        assert!(!same_album(&a, &d));
        let mut no_album = a.clone();
        no_album.album.clear();
        assert!(!same_album(&no_album, &no_album.clone()));
    }

    #[test]
    fn fade_curves() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert!((ease(0.5) - 0.5).abs() < 1e-6);
        assert!(ease(0.25) < 0.25 && ease(0.75) > 0.75);
        let start = Instant::now();
        assert_eq!(fade_progress(start, 4.0, start), 0.0);
        assert!((fade_progress(start, 4.0, start + Duration::from_secs(1)) - 0.25).abs() < 1e-6);
        assert_eq!(fade_progress(start, 4.0, start + Duration::from_secs(9)), 1.0);
        assert_eq!(fade_progress(start, 0.0, start), 1.0);
        // Streams get more time to load than local files.
        assert!(load_lead(Source::Local) < load_lead(Source::Spotify));
        assert!(load_lead(Source::Spotify) < load_lead(Source::SoundCloud));
    }

    #[test]
    fn artists_rank_exact_then_prefix_then_spotify() {
        let hit = |name: &str, source: Source| ArtistHit {
            key: format!("{}:{name}", source.as_str()),
            name: name.into(),
            image: None,
            source,
            subtitle: String::new(),
        };
        let mut artists = vec![
            hit("The Neon Coast Band", Source::SoundCloud),
            hit("neon coast", Source::SoundCloud),
            hit("Neon Coastline", Source::Spotify),
            hit("Neon Coast", Source::Spotify),
        ];
        rank_artists(&mut artists, "Neon Coast");
        let order: Vec<(&str, Source)> = artists.iter().map(|a| (a.name.as_str(), a.source)).collect();
        assert_eq!(
            order,
            vec![
                ("Neon Coast", Source::Spotify),
                ("neon coast", Source::SoundCloud),
                ("Neon Coastline", Source::Spotify),
                ("The Neon Coast Band", Source::SoundCloud),
            ]
        );
    }

    /// A short link that redirects to a page linking the real target.
    #[tokio::test]
    async fn short_links_are_followed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if head.starts_with("GET /short ") {
                    "HTTP/1.1 302 Found\r\nLocation: /landing\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                } else {
                    let body = "<html><a href=\"https://open.spotify.com/album/4m2880jivSbbyEGAKfITCa?si=1&amp;x=2\">Open</a></html>";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = expand_short_link(&http, &format!("{base}/short")).await.unwrap();
        assert_eq!(url, "https://open.spotify.com/album/4m2880jivSbbyEGAKfITCa");
        assert_eq!(
            links::target(&url),
            Some(Target::Spotify(LinkKind::Album, "4m2880jivSbbyEGAKfITCa".into()))
        );
    }

    /// Sessions saved by older versions still load (they come back as a plain list).
    #[test]
    fn old_sessions_still_load() {
        let old = r#"{"tracks":["a","b"],"index":0,"position":12.5,"context":"Mix","shuffle":true,"repeat":"All"}"#;
        let s: SavedSession = serde_json::from_str(old).unwrap();
        assert_eq!((s.tracks.len(), s.position, s.repeat), (2, 12.5, RepeatMode::All));
        assert!(s.queue.is_none() && s.extra.is_empty());
        let new = SavedSession {
            tracks: Vec::new(),
            index: 0,
            position: 3.0,
            context: String::new(),
            shuffle: false,
            repeat: RepeatMode::Off,
            queue: Some(QueueState::default()),
            extra: Vec::new(),
        };
        let back: SavedSession = serde_json::from_str(&serde_json::to_string(&new).unwrap()).unwrap();
        assert_eq!(back.queue, Some(QueueState::default()));
    }

    #[test]
    fn download_errors_are_short() {
        let e =
            anyhow!("SoundCloud /tracks/1 returned 404 Not Found").context("couldn't load \"Song\" from SoundCloud");
        assert_eq!(
            friendly_download_error(&e),
            "SoundCloud /tracks/1 returned 404 Not Found"
        );
        let e = anyhow!("\"Song\" can't be downloaded: only a 30 second preview is available (Go+ track)");
        assert!(friendly_download_error(&e).starts_with("\"Song\" can't be downloaded"));
    }

    #[test]
    fn artist_songs_without_repeats() {
        let sp = |title: &str, dur: u64| track(Source::Spotify, "Bladee", title, dur);
        let sc = |title: &str, dur: u64| track(Source::SoundCloud, "bladee", title, dur);
        // A single that is also on the album; "Intro"s of different albums are different songs.
        let spotify = dedupe_songs(vec![
            sp("Waster", 195_000),
            sp("Be Nice 2 Me", 182_000),
            sp("Waster", 195_000),
            sp("Intro", 60_000),
            sp("Intro", 95_000),
            sp("Waster - Remastered 2020", 196_000),
        ]);
        let titles: Vec<&str> = spotify.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["Waster", "Be Nice 2 Me", "Intro", "Intro"]);

        // The same songs on SoundCloud are left out; versions and SoundCloud-only songs stay.
        let mut songs = spotify;
        let added = add_songs(
            &mut songs,
            vec![
                sc("Bladee - Waster [Free DL]", 197_000),
                sc("be nice 2 me", 0),
                sc("Waster (Remix)", 210_000),
                sc("Waster - Slowed", 260_000),
                sc("Unreleased Demo", 120_000),
                sc("Be Nice 2 Me (Live)", 190_000),
            ],
        );
        assert_eq!(added, 4);
        let extra: Vec<&str> = songs[4..].iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            extra,
            vec![
                "Waster (Remix)",
                "Waster - Slowed",
                "Unreleased Demo",
                "Be Nice 2 Me (Live)"
            ]
        );
        assert!(same_song(&sp("Waster", 195_000), &sc("Waster", 205_000)));
        assert!(!same_song(&sp("Waster", 195_000), &sc("Waster", 240_000)));
        assert_eq!(artist_subtitle(&songs), "Artist · 8 songs on Spotify and SoundCloud");
        assert_eq!(artist_subtitle(&songs[..4]), "Artist · 4 songs on Spotify");
    }

    #[test]
    fn artist_names_match_exactly() {
        assert!(same_artist_name("Yung Lean", "yunglean"));
        assert!(same_artist_name("Bladee", "BLADEE"));
        assert!(!same_artist_name("Bladee", "Bladee Fan Page"));
        assert!(!same_artist_name("", ""));
    }

    fn track(source: Source, artist: &str, title: &str, dur: u64) -> Track {
        Track {
            id: format!("{}:{artist}{title}", source.as_str()),
            source,
            title: title.into(),
            artist: artist.into(),
            album: String::new(),
            duration_ms: dur,
            track_no: None,
            art: None,
            uri: String::new(),
            added_at: 0,
        }
    }

    #[test]
    fn best_match_prefers_exact_and_close_duration() {
        let target = track(
            Source::AppleMusic,
            "Daft Punk",
            "Get Lucky (feat. Pharrell Williams)",
            248_000,
        );
        let candidates = vec![
            track(Source::Spotify, "Daft Punk", "Get Lucky - Radio Edit", 250_000),
            track(Source::Spotify, "Daft Punk, Pharrell Williams", "Get Lucky", 248_500),
            track(Source::Spotify, "Some Cover Band", "Get Lucky", 248_000),
        ];
        let m = best_match(&target, &candidates).unwrap();
        assert_eq!(m.artist, "Daft Punk, Pharrell Williams");
    }

    #[test]
    fn best_match_rejects_wrong_songs() {
        let target = track(Source::AppleMusic, "Adele", "Hello", 295_000);
        let candidates = vec![
            track(Source::Spotify, "Lionel Richie", "Hello", 250_000),
            track(Source::Spotify, "Adele", "Hello", 400_000),
        ];
        assert!(best_match(&target, &candidates).is_none());
    }

    #[test]
    fn songs_without_an_artist_match_by_title_and_length() {
        let mut target = track(Source::Local, "", "Waster", 200_000);
        let candidates = vec![
            track(Source::Spotify, "Someone", "Waster", 150_000),
            track(Source::Spotify, "Bladee", "Waster", 201_500),
            track(Source::Spotify, "Bladee", "Waster (Remix)", 200_000),
        ];
        let found = same_title_and_length(&target, &candidates).unwrap();
        assert_eq!(found.artist, "Bladee");
        // Without a length there's too little to go on.
        target.duration_ms = 0;
        assert!(same_title_and_length(&target, &candidates).is_none());
    }

    #[tokio::test]
    async fn untagged_files_drop_the_folder_album_for_lastfm() {
        let dir = std::env::temp_dir().join(format!("multimusic-lfm-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Bladee - Waster.mp3");
        std::fs::write(&path, b"no tags here").unwrap();
        let mut t = track(Source::Local, "Bladee", "Waster", 200_000);
        t.album = "Downloads".into();
        t.uri = path.to_string_lossy().into();
        let sent = lastfm_track(None, t.clone()).await;
        assert_eq!((sent.artist.as_str(), sent.title.as_str()), ("Bladee", "Waster"));
        assert_eq!(sent.album, "");
        // Other sources are left alone.
        let sc = Track {
            source: Source::Spotify,
            ..t.clone()
        };
        assert_eq!(lastfm_track(None, sc).await.album, "Downloads");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn pasted_files_and_links_become_songs() {
        let dir = std::env::temp_dir().join(format!("multimusic-paste-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Bladee - Waster.mp3");
        std::fs::write(&path, b"no tags").unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let sc = SoundCloud::new(http.clone(), "", "");
        let shared = Shared::default();
        let paste = |line: String| {
            let (sc, http, shared) = (&sc, &http, &shared);
            async move { songs_behind_link(&line, None, sc, http, shared).await }
        };
        // A path, or a file manager's file:// link.
        let found = paste(path.to_string_lossy().into()).await.unwrap();
        assert_eq!(
            (found[0].artist.as_str(), found[0].title.as_str()),
            ("Bladee", "Waster")
        );
        let link = format!("file://{}", path.to_string_lossy().replace(' ', "%20"));
        assert_eq!(paste(link).await.unwrap()[0].id, found[0].id);
        // A song already in the library comes from there.
        let mut known = found[0].clone();
        known.title = "Known".into();
        shared.library.write().unwrap().tracks.insert(known.id.clone(), known);
        assert_eq!(paste(path.to_string_lossy().into()).await.unwrap()[0].title, "Known");
        // Not music, gone, or a Spotify link without a session.
        assert!(paste(dir.to_string_lossy().into()).await.is_none());
        assert!(paste("/nowhere/x.mp3".into()).await.is_none());
        assert!(paste("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC".into())
            .await
            .is_none());
        assert!(paste("hello".into()).await.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pasted_paths() {
        assert_eq!(local_path("/music/a b.mp3"), Some(PathBuf::from("/music/a b.mp3")));
        assert_eq!(
            local_path("file:///music/a%20b.mp3"),
            Some(PathBuf::from("/music/a b.mp3"))
        );
        // Windows file manager links drop the slash before the drive.
        let win = local_path("file:///C:/Music/a.mp3");
        if cfg!(windows) {
            assert_eq!(win, Some(PathBuf::from("C:/Music/a.mp3")));
        } else {
            assert_eq!(win, None);
        }
        assert_eq!(local_path("https://soundcloud.com/a/b"), None);
        assert_eq!(local_path("spotify:track:1"), None);
        assert_eq!(local_path("relative/a.mp3"), None);
    }

    #[tokio::test]
    async fn covers_load_from_files_and_links() {
        let dir = std::env::temp_dir().join(format!("multimusic-cover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("my cover.png");
        image::RgbImage::new(4, 4).save(&path).unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let data = load_cover(&http, &path.to_string_lossy()).await.unwrap();
        assert!(data.starts_with(b"\x89PNG"));
        let link = format!("file://{}", path.to_string_lossy().replace(' ', "%20"));
        assert_eq!(load_cover(&http, &link).await.unwrap(), data);
        std::fs::write(dir.join("notes.txt"), b"not an image").unwrap();
        assert!(load_cover(&http, &dir.join("notes.txt").to_string_lossy())
            .await
            .is_err());
        assert!(load_cover(&http, "/nowhere/cover.jpg").await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn position_extrapolates_only_when_playing() {
        let mut pv = PlayerView {
            position: 10.0,
            position_at: Instant::now() - Duration::from_secs(2),
            duration: 11.0,
            status: PlayStatus::Paused,
            ..Default::default()
        };
        assert_eq!(pv.position_now(), 10.0);
        pv.status = PlayStatus::Playing;
        assert_eq!(pv.position_now(), 11.0); // clamped to duration
    }
}
