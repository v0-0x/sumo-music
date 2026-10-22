//! Core data types shared by every part of the app.

use serde::{Deserialize, Serialize};

/// Where a track comes from (and therefore which engine plays it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    Local,
    Spotify,
    SoundCloud,
    /// Imported from Apple Music. Apple Music streams are DRM protected and can't be
    /// played on Linux, so these are resolved to a local/Spotify/SoundCloud match at play time.
    AppleMusic,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Local => "local",
            Source::Spotify => "spotify",
            Source::SoundCloud => "soundcloud",
            Source::AppleMusic => "applemusic",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "local" => Source::Local,
            "spotify" => Source::Spotify,
            "soundcloud" => Source::SoundCloud,
            "applemusic" => Source::AppleMusic,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Source::Local => "Local",
            Source::Spotify => "Spotify",
            Source::SoundCloud => "SoundCloud",
            Source::AppleMusic => "Apple Music",
        }
    }
}

/// A single playable item from any source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Globally unique key, prefixed by source:
    /// `local:/music/a.flac`, `spotify:track:<base62>`, `soundcloud:<numeric id>`, `applemusic:<id>`.
    pub id: String,
    pub source: Source,
    pub title: String,
    /// Display artist string ("A, B" for multiple artists).
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub track_no: Option<u32>,
    /// Cover art: an http(s) URL, or a local path (image file, or audio file with embedded art).
    pub art: Option<String>,
    /// Source specific playback locator:
    /// local file path, `spotify:track:<id>` URI, SoundCloud permalink URL, or empty for Apple Music.
    pub uri: String,
    /// Unix seconds when this was added to the library/playlist (0 = unknown).
    pub added_at: i64,
    /// The album's artist when it isn't the song's (compilations, songs "feat." someone).
    /// Empty: the song's artist.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub album_artist: String,
    /// Disc of a multi-disc album.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_no: Option<u32>,
}

impl Track {
    /// Whose album this is: the album artist, else the song's artist.
    pub fn album_by(&self) -> &str {
        if self.album_artist.trim().is_empty() {
            &self.artist
        } else {
            &self.album_artist
        }
    }

    pub fn local_id(path: &str) -> String {
        format!("local:{path}")
    }

    pub fn soundcloud_id(id: u64) -> String {
        format!("soundcloud:{id}")
    }

    pub fn applemusic_id(id: &str) -> String {
        format!("applemusic:{id}")
    }

    /// Duration in seconds as f64.
    pub fn duration_secs(&self) -> f64 {
        self.duration_ms as f64 / 1000.0
    }

    /// Normalized "artist - title" key used for fuzzy cross-source matching.
    pub fn match_key(&self) -> String {
        match_key(&self.artist, &self.title)
    }
}

/// Lowercases, strips punctuation and common decorations ("(feat. X)", "- Remastered 2011")
/// so the same song from different sources produces the same key.
pub fn normalize_title(s: &str) -> String {
    let lower = s.to_lowercase();
    // Cut decorations that differ between services.
    let mut cut = lower.as_str();
    for marker in [
        " (feat",
        " [feat",
        " (ft.",
        " (ft ",
        " [ft.",
        " [ft ",
        " (with ",
        " feat.",
        " ft.",
        " - remaster",
        " (remaster",
        " [remaster",
        " - single",
        " - radio edit",
    ] {
        if let Some(i) = cut.find(marker) {
            cut = &cut[..i];
        }
    }
    cut.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The first credited artist, normalized.
pub fn normalize_artist(s: &str) -> String {
    let lower = s.to_lowercase();
    let first = lower
        .split([',', '&', ';', '/'])
        .next()
        .unwrap_or("")
        .split(" feat")
        .next()
        .unwrap_or("")
        .split(" x ")
        .next()
        .unwrap_or("");
    first
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn match_key(artist: &str, title: &str) -> String {
    format!("{}\u{1f}{}", normalize_artist(artist), normalize_title(title))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlaylistKind {
    /// User created playlist (can mix sources).
    Custom,
    /// The built-in cross-source "Liked Songs" playlist.
    Liked,
    Spotify,
    SpotifyLiked,
    SoundCloud,
    SoundCloudLikes,
    AppleMusic,
    /// Imported from an .m3u/.m3u8 file.
    M3u,
    /// A Spotify album saved to the library (shown with the albums, not the playlists).
    SpotifyAlbum,
    /// A SoundCloud album saved to the library.
    SoundCloudAlbum,
}

impl PlaylistKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlaylistKind::Custom => "custom",
            PlaylistKind::Liked => "liked",
            PlaylistKind::Spotify => "spotify",
            PlaylistKind::SpotifyLiked => "spotify_liked",
            PlaylistKind::SoundCloud => "soundcloud",
            PlaylistKind::SoundCloudLikes => "soundcloud_likes",
            PlaylistKind::AppleMusic => "applemusic",
            PlaylistKind::M3u => "m3u",
            PlaylistKind::SpotifyAlbum => "spotify_album",
            PlaylistKind::SoundCloudAlbum => "soundcloud_album",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "custom" => PlaylistKind::Custom,
            "liked" => PlaylistKind::Liked,
            "spotify" => PlaylistKind::Spotify,
            "spotify_liked" => PlaylistKind::SpotifyLiked,
            "soundcloud" => PlaylistKind::SoundCloud,
            "soundcloud_likes" => PlaylistKind::SoundCloudLikes,
            "applemusic" => PlaylistKind::AppleMusic,
            "m3u" => PlaylistKind::M3u,
            "spotify_album" => PlaylistKind::SpotifyAlbum,
            "soundcloud_album" => PlaylistKind::SoundCloudAlbum,
            _ => return None,
        })
    }

    /// Saved albums, shown under Albums instead of the playlists.
    pub fn is_album(self) -> bool {
        matches!(self, PlaylistKind::SpotifyAlbum | PlaylistKind::SoundCloudAlbum)
    }

    /// Playlists the user can edit inside the app.
    pub fn is_editable(self) -> bool {
        matches!(self, PlaylistKind::Custom | PlaylistKind::Liked | PlaylistKind::M3u)
    }

    pub fn source(self) -> Option<Source> {
        match self {
            PlaylistKind::Spotify | PlaylistKind::SpotifyLiked | PlaylistKind::SpotifyAlbum => Some(Source::Spotify),
            PlaylistKind::SoundCloud | PlaylistKind::SoundCloudLikes | PlaylistKind::SoundCloudAlbum => {
                Some(Source::SoundCloud)
            }
            PlaylistKind::AppleMusic => Some(Source::AppleMusic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    /// Local key, e.g. `custom:<uuid>`, `spotify:<playlist id>`, `liked`.
    pub id: String,
    pub name: String,
    pub kind: PlaylistKind,
    /// Id on the remote service, if any.
    pub remote_id: Option<String>,
    pub description: String,
    pub art: Option<String>,
    pub track_ids: Vec<String>,
}

/// A playlist fetched from a remote service (or file) that still has to be merged into the library.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedPlaylist {
    pub remote_id: String,
    pub name: String,
    pub description: String,
    pub art: Option<String>,
    pub tracks: Vec<Track>,
}

/// An artist (or SoundCloud user) in search results; `key` opens their page.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtistHit {
    /// Page key: `local:artist:<name>`, `soundcloud:user:<id>`, `spotify:artist:<id>`, ...
    pub key: String,
    pub name: String,
    pub image: Option<String>,
    pub source: Source,
    pub subtitle: String,
}

/// An album or playlist in search results; `key` opens its page.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectionHit {
    /// Page key: `spotify:album:<id>`, `spotify:playlist:<id>` or a SoundCloud set's link.
    pub key: String,
    pub title: String,
    /// The artist of an album, the owner of a playlist.
    pub by: String,
    pub image: Option<String>,
    pub source: Source,
    pub album: bool,
    /// How many songs, when known.
    pub songs: Option<u32>,
    /// Release year of an album, when known.
    pub year: Option<u16>,
}

impl CollectionHit {
    /// "Artist · 2024" for albums, "by Owner · 32 songs" for playlists.
    pub fn subtitle(&self) -> String {
        let mut parts = Vec::new();
        if self.album {
            if !self.by.is_empty() {
                parts.push(self.by.clone());
            }
            if let Some(y) = self.year {
                parts.push(y.to_string());
            }
        } else {
            if !self.by.is_empty() {
                parts.push(format!("by {}", self.by));
            }
            if let Some(n) = self.songs {
                parts.push(if n == 1 { "1 song".into() } else { format!("{n} songs") });
            }
        }
        if parts.is_empty() {
            return if self.album { "Album".into() } else { "Playlist".into() };
        }
        parts.join(" · ")
    }
}

/// 1234 -> "1.2K", 2500000 -> "2.5M".
pub fn human_count(n: u64) -> String {
    let f = |v: f64, unit: &str| {
        let s = format!("{v:.1}");
        format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => f(n as f64 / 1_000.0, "K"),
        _ => f(n as f64 / 1_000_000.0, "M"),
    }
}

/// One line of time-synced lyrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LyricLine {
    pub time_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Lyrics {
    /// Sorted by time. Empty if only plain lyrics are available.
    pub synced: Vec<LyricLine>,
    /// Plain text lyrics (always filled when any lyrics exist).
    pub plain: String,
    pub instrumental: bool,
    /// Where they came from ("LRCLIB", "Embedded tag", "Sidecar .lrc").
    pub provider: String,
}

impl Lyrics {
    /// Index of the line that should be highlighted at `pos_ms`.
    pub fn line_at(&self, pos_ms: u64) -> Option<usize> {
        if self.synced.is_empty() || pos_ms < self.synced[0].time_ms {
            return None;
        }
        let idx = self.synced.partition_point(|l| l.time_ms <= pos_ms);
        Some(idx.saturating_sub(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn cycle(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_keys_ignore_decorations() {
        assert_eq!(
            match_key("Daft Punk", "Get Lucky (feat. Pharrell Williams)"),
            match_key("Daft Punk, Pharrell Williams", "Get Lucky")
        );
        assert_eq!(
            match_key("The Beatles", "Let It Be - Remastered 2009"),
            match_key("the beatles", "Let It Be")
        );
        // SoundCloud-style credits.
        assert_eq!(normalize_title("SIDE BY SIDE (FT. THAIBOY DIGITAL)"), "side by side");
        assert_eq!(normalize_title("Side By Side [ft. Thaiboy Digital]"), "side by side");
        assert_eq!(normalize_title("Side By Side (with Thaiboy Digital)"), "side by side");
        // Versions stay apart.
        assert_eq!(normalize_title("Side By Side (Live)"), "side by side live");
    }

    #[test]
    fn human_counts() {
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1_234), "1.2K");
        assert_eq!(human_count(10_000), "10K");
        assert_eq!(human_count(2_500_000), "2.5M");
    }

    #[test]
    fn lyric_line_lookup() {
        let l = Lyrics {
            synced: vec![
                LyricLine {
                    time_ms: 1000,
                    text: "a".into(),
                },
                LyricLine {
                    time_ms: 2000,
                    text: "b".into(),
                },
                LyricLine {
                    time_ms: 3000,
                    text: "c".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(l.line_at(500), None);
        assert_eq!(l.line_at(1000), Some(0));
        assert_eq!(l.line_at(2500), Some(1));
        assert_eq!(l.line_at(99999), Some(2));
    }
}

/// Format details of what's playing, shown as "FLAC · 24-bit / 96 kHz".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AudioQuality {
    pub codec: String,
    pub lossless: bool,
    pub bits: Option<u8>,
    pub sample_rate: Option<u32>,
    pub bitrate_kbps: Option<u32>,
}

impl AudioQuality {
    /// Lossless with more than CD resolution (16-bit / 48 kHz).
    pub fn hi_res(&self) -> bool {
        self.lossless && (self.bits.unwrap_or(16) > 16 || self.sample_rate.unwrap_or(44_100) > 48_000)
    }

    pub fn label(&self) -> String {
        let mut parts = vec![self.codec.clone()];
        if self.lossless {
            let rate = self.sample_rate.map(|r| {
                let khz = r as f32 / 1000.0;
                if khz.fract() == 0.0 {
                    format!("{khz:.0} kHz")
                } else {
                    format!("{khz:.1} kHz")
                }
            });
            match (self.bits, rate) {
                (Some(b), Some(r)) => parts.push(format!("{b}-bit / {r}")),
                (None, Some(r)) => parts.push(r),
                (Some(b), None) => parts.push(format!("{b}-bit")),
                (None, None) => {}
            }
        } else if let Some(k) = self.bitrate_kbps.filter(|k| *k > 0) {
            parts.push(format!("{k} kbps"));
        }
        parts.join(" · ")
    }

    /// Codec names as mpv reports them (`audio-codec-name`).
    pub fn from_mpv(codec: &str, bitrate_bps: Option<f64>, sample_rate: Option<u32>) -> AudioQuality {
        let (name, lossless) = match codec {
            "flac" => ("FLAC", true),
            "alac" => ("ALAC", true),
            "ape" => ("APE", true),
            "wavpack" => ("WavPack", true),
            "tta" => ("TTA", true),
            c if c.starts_with("pcm_") => ("PCM", true),
            "mp3" | "mp3float" => ("MP3", false),
            "aac" => ("AAC", false),
            "opus" => ("Opus", false),
            "vorbis" => ("Ogg Vorbis", false),
            other => (other, false),
        };
        AudioQuality {
            codec: name.to_string(),
            lossless,
            bits: None,
            sample_rate,
            bitrate_kbps: bitrate_bps.map(|b| (b / 1000.0).round() as u32),
        }
    }
}

#[cfg(test)]
mod quality_tests {
    use super::*;

    #[test]
    fn labels() {
        let flac = AudioQuality {
            codec: "FLAC".into(),
            lossless: true,
            bits: Some(24),
            sample_rate: Some(96_000),
            bitrate_kbps: Some(2900),
        };
        assert_eq!(flac.label(), "FLAC · 24-bit / 96 kHz");
        assert!(flac.hi_res());
        let cd = AudioQuality {
            bits: Some(16),
            sample_rate: Some(44_100),
            ..flac.clone()
        };
        assert_eq!(cd.label(), "FLAC · 16-bit / 44.1 kHz");
        assert!(!cd.hi_res());
        let ogg = AudioQuality {
            codec: "Ogg Vorbis".into(),
            bitrate_kbps: Some(320),
            ..Default::default()
        };
        assert_eq!(ogg.label(), "Ogg Vorbis · 320 kbps");
        assert!(AudioQuality::from_mpv("pcm_s24le", None, Some(48_000)).lossless);
        assert_eq!(
            AudioQuality::from_mpv("opus", Some(160_000.0), None).label(),
            "Opus · 160 kbps"
        );
    }
}
