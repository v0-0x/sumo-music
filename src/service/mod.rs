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

mod downloads;
mod pages;
mod playback;
mod sync;
mod updates;

use self::{pages::*, sync::*};

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
    /// Plays (or with `queue`, queues) songs known only by artist and title, like Last.fm's
    /// top tracks: the library's file when there is one, else the match on Spotify or
    /// SoundCloud.
    PlayNamed {
        songs: Vec<(String, String)>,
        context: String,
        queue: bool,
    },
    /// Pauses after a while, or when the playing song ends; `None` turns it off.
    SetSleep(Option<SleepTimer>),
    Enqueue(Vec<Track>),
    PlayNext(Vec<Track>),
    /// Entry of the upcoming list and the song shown there (in case the queue moved on).
    JumpTo(usize, String),
    RemoveUpcoming(usize, String),
    /// Song at entry `from` of the upcoming list (with its id) to entry `to`.
    MoveUpcoming {
        from: usize,
        to: usize,
        id: String,
    },
    /// Clears the rest of the playlist; the songs you queued stay.
    ClearUpcoming,
    /// Clears the songs you queued.
    ClearQueued,
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
    /// Drag and drop in a playlist: the songs at `rows` (index, id) go before position `to`.
    MovePlaylistTracks {
        playlist_id: String,
        rows: Vec<(usize, String)>,
        to: usize,
    },
    /// Saves songs to Liked Songs (`liked`) or removes them.
    SetLiked {
        tracks: Vec<Track>,
        liked: bool,
    },
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

/// When the sleep timer pauses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepTimer {
    Minutes(u32),
    EndOfSong,
}

/// The sleep timer as it runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Sleep {
    At(Instant),
    /// When this many songs have started (the next one after the one playing).
    AfterSong(u64),
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
    pub sleep: Option<Sleep>,
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
            sleep: None,
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
    /// Songs found for [`Command::PlayNamed`], in order; `missing` weren't found.
    NamedFound {
        tracks: Vec<Track>,
        missing: usize,
        context: String,
        queue: bool,
    },
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

/// Plays kept in the listening history (for Recently played and Most played).
const MAX_PLAYS: usize = 200_000;
/// Disk space for downloaded covers and for lyrics.
const MAX_ART_CACHE: u64 = 400 << 20;
const MAX_LYRICS_CACHE: u64 = 40 << 20;

/// GitHub's API, where releases (updates) come from.
const GITHUB_API: &str = "https://api.github.com";

/// Bumped when the scanner learns to read more from files, so the library is read once again.
const SCAN_VERSION: &str = "3";

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
    sleep: Option<Sleep>,
    /// Songs started so far (repeats too).
    songs_started: u64,
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
            sleep: None,
            songs_started: 0,
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
            if self.sleep.is_some() {
                self.check_sleep().await;
            }
        }
        self.shutdown().await;
    }

    fn startup(&mut self) {
        self.trim_caches();
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

    /// Covers, lyrics and the listening history are kept to a sensible size.
    fn trim_caches(&self) {
        match self.db.trim_plays(MAX_PLAYS) {
            Ok(n) if n > 0 => tracing::info!("dropped the {n} oldest plays"),
            Ok(_) => {}
            Err(e) => tracing::warn!("trimming plays: {e:#}"),
        }
        let (art, lyrics) = (self.paths.art_cache(), self.paths.lyrics_cache());
        tokio::task::spawn_blocking(move || {
            let freed = crate::cache::trim(&art, MAX_ART_CACHE) + crate::cache::trim(&lyrics, MAX_LYRICS_CACHE);
            if freed > 0 {
                tracing::info!("cleared {} MB of old covers and lyrics", freed >> 20);
            }
        });
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
        // Picking another song moves "after this song" to it.
        let picks_song = matches!(
            cmd,
            Command::Play { .. } | Command::Next | Command::Previous | Command::JumpTo(..)
        );
        self.handle_command(cmd).await;
        if picks_song && matches!(self.sleep, Some(Sleep::AfterSong(_))) {
            self.sleep = Some(Sleep::AfterSong(self.end_of_song()));
        }
    }

    /// The count of started songs at which the one playing (or loading) has ended.
    fn end_of_song(&self) -> u64 {
        self.songs_started + 1 + u64::from(!self.started_reported)
    }

    async fn handle_command(&mut self, cmd: Command) {
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
            Command::PlayNamed { songs, context, queue } => self.play_named(songs, context, queue),
            Command::SetSleep(timer) => {
                self.sleep = match timer {
                    Some(SleepTimer::Minutes(m)) => {
                        Some(Sleep::At(Instant::now() + Duration::from_secs(60 * u64::from(m))))
                    }
                    Some(SleepTimer::EndOfSong) => self.queue.current().map(|_| Sleep::AfterSong(self.end_of_song())),
                    None => None,
                };
                match timer {
                    Some(SleepTimer::Minutes(m)) => self.shared.info(format!("Pausing in {m} minutes")),
                    Some(SleepTimer::EndOfSong) if self.sleep.is_some() => {
                        self.shared.info("Pausing after this song".to_string())
                    }
                    Some(SleepTimer::EndOfSong) => {}
                    None => self.shared.info("Sleep timer off".to_string()),
                }
                self.publish_player();
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
            Command::ClearQueued => {
                self.queue.clear_queued();
                self.after_queue_change().await;
            }
            Command::MoveUpcoming { from, to, id } => {
                self.queue.move_upcoming(from, to, &id);
                self.after_queue_change().await;
            }
            Command::ToggleLike(track) => self.toggle_like(track),
            Command::SetLiked { tracks, liked } => self.set_liked(tracks, liked),
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
            Command::MovePlaylistTracks { playlist_id, rows, to } => {
                let updated = {
                    let mut lib = self.shared.library.write().unwrap();
                    lib.playlist_mut(&playlist_id).map(|p| {
                        library::move_rows(&mut p.track_ids, &rows, to);
                        p.clone()
                    })
                };
                if let Some(p) = updated {
                    self.save_playlists(vec![p], &[]);
                }
            }
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

    // ---------------------------------------------------------------- playlists

    fn add_to_playlist(&mut self, playlist_id: &str, tracks: &[Track]) {
        self.store_tracks(tracks);
        let updated = {
            let mut lib = self.shared.library.write().unwrap();
            lib.playlist_mut(playlist_id).map(|p| {
                // Songs already in the playlist aren't added twice.
                let mut have: HashSet<&str> = p.track_ids.iter().map(String::as_str).collect();
                let new: Vec<String> = tracks
                    .iter()
                    .filter(|t| have.insert(t.id.as_str()))
                    .map(|t| t.id.clone())
                    .collect();
                let skipped = tracks.len() - new.len();
                p.track_ids.extend(new);
                if p.art.is_none() {
                    p.art = tracks.iter().find_map(|t| t.art.clone());
                }
                (p.clone(), skipped)
            })
        };
        let Some((p, skipped)) = updated else { return };
        let added = tracks.len() - skipped;
        self.shared.info(match (added, skipped) {
            (_, 0) => format!("Added {} to “{}”", songs_text(added), p.name),
            (0, 1) => format!("“{}” is already in “{}”", tracks[0].title, p.name),
            (0, _) => format!("These songs are already in “{}”", p.name),
            _ => format!(
                "Added {} to “{}” ({} already in it)",
                songs_text(added),
                p.name,
                songs_text(skipped)
            ),
        });
        if added > 0 {
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
            Internal::NamedFound {
                tracks,
                missing,
                context,
                queue,
            } => {
                if missing > 0 {
                    self.shared.error(format!(
                        "Couldn't find {} of {} on your services",
                        songs_text(missing),
                        songs_text(missing + tracks.len())
                    ));
                }
                if tracks.is_empty() {
                    return;
                }
                self.store_tracks(&tracks);
                let cmd = if queue {
                    Command::Enqueue(tracks)
                } else {
                    Command::Play {
                        tracks,
                        start: 0,
                        context,
                    }
                };
                Box::pin(self.handle(cmd)).await;
            }
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

/// Database key prefix of downloaded songs (`download:<track id>` → file path).
const DOWNLOAD_KEY: &str = "download:";

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

/// Exact and prefix name matches first; Spotify before SoundCloud on ties. Stable, so each
/// service's own relevance order is kept.
/// The library id of a saved album.
pub fn saved_album_id(kind: PlaylistKind, remote: &str) -> String {
    match kind {
        PlaylistKind::SpotifyAlbum => format!("spotify_album:{remote}"),
        _ => format!("soundcloud_album:{remote}"),
    }
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
        crate::http::get_image(http, src).await?
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

/// A song known only by name, to look up.
fn named_track(artist: &str, title: &str) -> Track {
    Track {
        id: format!("named:{artist}\u{1f}{title}"),
        source: Source::AppleMusic,
        title: title.to_string(),
        artist: artist.to_string(),
        album: String::new(),
        album_artist: String::new(),
        duration_ms: 0,
        track_no: None,
        disc_no: None,
        art: None,
        uri: String::new(),
        added_at: 0,
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
    use super::{downloads::*, playback::*};

    fn listed(id: &str, kind: PlaylistKind, songs: &[&str]) -> Playlist {
        Playlist {
            id: id.into(),
            name: id.into(),
            kind,
            remote_id: None,
            description: String::new(),
            art: None,
            track_ids: songs.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A sync keeps what it couldn't load as it was, and only takes lists it didn't bring
    /// (and could have) for deleted.
    #[test]
    fn syncs_keep_what_failed() {
        let existing: HashMap<String, Playlist> = [
            listed("spotify:liked", PlaylistKind::SpotifyLiked, &["a", "b"]),
            listed("spotify:same", PlaylistKind::Spotify, &["c"]),
            listed("spotify:failed", PlaylistKind::Spotify, &["d"]),
            listed("spotify:deleted", PlaylistKind::Spotify, &["e"]),
        ]
        .into_iter()
        .map(|p| (p.id.clone(), p))
        .collect();
        let meta = |id: &str| SpotifyPlaylistMeta {
            id: id.into(),
            name: id.into(),
            ..Default::default()
        };
        // Liked Songs failed, "same" is unchanged, "new" is new, "failed" timed out.
        let synced = spotify_playlists(
            None,
            vec![(meta("same"), None), (meta("new"), Some(vec!["f".into()]))],
            &["failed".to_string()],
            &existing,
        );
        let songs = |id: &str| synced.iter().find(|p| p.id == id).map(|p| p.track_ids.clone());
        assert_eq!(songs("spotify:liked"), Some(vec!["a".to_string(), "b".to_string()]));
        assert_eq!(songs("spotify:same"), Some(vec!["c".to_string()]));
        assert_eq!(songs("spotify:new"), Some(vec!["f".to_string()]));
        assert_eq!(songs("spotify:failed"), Some(vec!["d".to_string()]));
        let have: Vec<Playlist> = existing.values().cloned().collect();
        let kinds = [PlaylistKind::Spotify, PlaylistKind::SpotifyLiked];
        assert_eq!(
            removed_playlists(&have, &synced, &kinds),
            vec!["spotify:deleted".to_string()]
        );

        // SoundCloud playlists that couldn't be loaded aren't taken for deleted.
        let sc = [
            listed("soundcloud:likes", PlaylistKind::SoundCloudLikes, &["x"]),
            listed("soundcloud:pl:1", PlaylistKind::SoundCloud, &["y"]),
        ];
        let likes = [listed("soundcloud:likes", PlaylistKind::SoundCloudLikes, &["x"])];
        let failed: std::result::Result<(), String> = Err("timeout".into());
        assert!(removed_playlists(&sc, &likes, soundcloud_prune(&failed)).is_empty());
        let loaded: std::result::Result<(), String> = Ok(());
        assert_eq!(
            removed_playlists(&sc, &likes, soundcloud_prune(&loaded)),
            vec!["soundcloud:pl:1".to_string()]
        );
    }

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
            album_artist: String::new(),
            disc_no: None,
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
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
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
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
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
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
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
