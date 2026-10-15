//! In-memory music library, backed by SQLite.

pub mod db;
pub mod m3u;
pub mod quality;
pub mod scanner;
pub mod tags;

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::model::{match_key, Playlist, PlaylistKind, Source, Track};

pub use db::Db;

pub const LIKED_ID: &str = "liked";

#[derive(Debug, Clone)]
pub struct Album {
    pub key: String,
    pub name: String,
    pub artist: String,
    pub art: Option<String>,
    pub track_ids: Vec<String>,
}

/// An artist with songs in the library (local files or any playlist).
#[derive(Debug, Clone)]
pub struct Artist {
    pub name: String,
    /// Lower-cased name; the artist page is `local:artist:<key>`.
    pub key: String,
    /// Cover of one of their songs (there are no artist pictures for library artists).
    pub art: Option<String>,
    /// Local files first (by album and track number), then everything else by title.
    pub track_ids: Vec<String>,
}

#[derive(Default)]
pub struct Library {
    pub tracks: HashMap<String, Track>,
    pub playlists: Vec<Playlist>,
    /// Local track ids sorted by artist, album, track number.
    pub local: Vec<String>,
    pub albums: Vec<Album>,
    /// Artists with the most songs first.
    pub artists: Vec<Artist>,
    /// Recently played track ids, newest first.
    pub recent: Vec<String>,
    /// Bumped on every change so views can invalidate caches.
    pub version: u64,
    liked: HashSet<String>,
    /// match key -> local track id, to play imported songs from local files.
    local_match: HashMap<String, String>,
    /// Artist key -> index into `artists`.
    artist_index: HashMap<String, usize>,
}

impl Library {
    pub fn load(db: &Db) -> Result<Library> {
        let mut lib = Library {
            tracks: db.load_tracks()?.into_iter().map(|t| (t.id.clone(), t)).collect(),
            playlists: db.load_playlists()?,
            recent: db.recent_plays(50)?,
            ..Default::default()
        };
        if !lib.playlists.iter().any(|p| p.id == LIKED_ID) {
            lib.playlists.insert(0, liked_playlist());
        }
        lib.reindex();
        Ok(lib)
    }

    /// Recomputes derived data after tracks/playlists changed.
    pub fn reindex(&mut self) {
        let mut local: Vec<&Track> = self.tracks.values().filter(|t| t.source == Source::Local).collect();
        // Keys made once per song, not twice per comparison (big libraries re-sort on every
        // change while the UI waits).
        local.sort_by_cached_key(|t| {
            (
                sort_key(&t.artist),
                sort_key(&t.album),
                t.track_no.unwrap_or(0),
                t.title.clone(),
            )
        });

        let mut albums: Vec<Album> = Vec::new();
        let mut album_index: HashMap<String, usize> = HashMap::new();
        self.local_match.clear();
        for t in &local {
            self.local_match.entry(t.match_key()).or_insert_with(|| t.id.clone());
            let key = format!("{}\u{1f}{}", sort_key(&t.album), sort_key(&t.artist));
            let idx = *album_index.entry(key.clone()).or_insert_with(|| {
                albums.push(Album {
                    key,
                    name: t.album.clone(),
                    artist: t.artist.clone(),
                    art: t.art.clone(),
                    track_ids: Vec::new(),
                });
                albums.len() - 1
            });
            albums[idx].track_ids.push(t.id.clone());
        }
        self.local = local.iter().map(|t| t.id.clone()).collect();
        albums.sort_by_cached_key(|a| (sort_key(&a.artist), a.name.clone()));
        self.albums = albums;

        self.liked = self
            .playlists
            .iter()
            .find(|p| p.id == LIKED_ID)
            .map(|p| p.track_ids.iter().cloned().collect())
            .unwrap_or_default();
        self.sort_playlists();
        self.index_artists();
        self.version += 1;
    }

    /// Groups local files and playlist songs by artist.
    fn index_artists(&mut self) {
        let mut seen: HashSet<&str> = HashSet::new();
        let members: Vec<&Track> = self
            .local
            .iter()
            .chain(self.playlists.iter().flat_map(|p| p.track_ids.iter()))
            .filter(|id| seen.insert(id.as_str()))
            .filter_map(|id| self.tracks.get(id))
            .collect();
        let solo: HashSet<String> = members.iter().map(|t| t.artist.trim().to_lowercase()).collect();

        let mut artists: Vec<Artist> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for t in &members {
            for name in credited_artists(&t.artist, &solo) {
                let key = name.to_lowercase();
                let i = *index.entry(key.clone()).or_insert_with(|| {
                    artists.push(Artist {
                        name: name.to_string(),
                        key,
                        art: None,
                        track_ids: Vec::new(),
                    });
                    artists.len() - 1
                });
                let a = &mut artists[i];
                a.track_ids.push(t.id.clone());
                if a.art.is_none() {
                    a.art = t.art.clone();
                }
            }
        }
        for a in &mut artists {
            let tracks = &self.tracks;
            a.track_ids.sort_by_cached_key(|id| {
                let t = &tracks[id];
                let local = t.source == Source::Local;
                (
                    !local,
                    if local { sort_key(&t.album) } else { String::new() },
                    if local { t.track_no.unwrap_or(0) } else { 0 },
                    sort_key(&t.title),
                )
            });
        }
        artists.sort_by_cached_key(|a| (std::cmp::Reverse(a.track_ids.len()), sort_key(&a.name)));
        self.artist_index = artists.iter().enumerate().map(|(i, a)| (a.key.clone(), i)).collect();
        self.artists = artists;
    }

    pub fn artist(&self, key: &str) -> Option<&Artist> {
        self.artist_index
            .get(&key.to_lowercase())
            .and_then(|&i| self.artists.get(i))
    }

    /// Library artists whose name contains every word of `query`; exact and prefix matches first.
    pub fn search_artists(&self, query: &str, limit: usize) -> Vec<&Artist> {
        let q = query.trim().to_lowercase();
        let terms: Vec<&str> = q.split_whitespace().collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(u8, usize, &Artist)> = self
            .artists
            .iter()
            .enumerate()
            .filter(|(_, a)| terms.iter().all(|term| a.key.contains(term)))
            .map(|(i, a)| {
                let rank = if a.key == q {
                    0
                } else if a.key.starts_with(&q) {
                    1
                } else {
                    2
                };
                (rank, i, a)
            })
            .collect();
        hits.sort_by_key(|&(rank, i, _)| (rank, i));
        hits.into_iter().take(limit).map(|(_, _, a)| a).collect()
    }

    fn sort_playlists(&mut self) {
        fn rank(k: PlaylistKind) -> u8 {
            match k {
                PlaylistKind::Liked => 0,
                PlaylistKind::Custom => 1,
                PlaylistKind::M3u => 2,
                PlaylistKind::SpotifyLiked => 3,
                PlaylistKind::Spotify => 4,
                PlaylistKind::SoundCloudLikes => 5,
                PlaylistKind::SoundCloud => 6,
                PlaylistKind::AppleMusic => 7,
                PlaylistKind::SpotifyAlbum => 8,
                PlaylistKind::SoundCloudAlbum => 9,
            }
        }
        // Stable: keeps service order (e.g. Spotify's own playlist order) within a kind.
        self.playlists.sort_by_key(|p| rank(p.kind));
    }

    pub fn get(&self, id: &str) -> Option<&Track> {
        self.tracks.get(id)
    }

    pub fn playlist(&self, id: &str) -> Option<&Playlist> {
        self.playlists.iter().find(|p| p.id == id)
    }

    pub fn playlist_mut(&mut self, id: &str) -> Option<&mut Playlist> {
        self.playlists.iter_mut().find(|p| p.id == id)
    }

    pub fn tracks_for(&self, ids: &[String]) -> Vec<Track> {
        ids.iter().filter_map(|id| self.tracks.get(id).cloned()).collect()
    }

    pub fn is_liked(&self, id: &str) -> bool {
        self.liked.contains(id)
    }

    /// Finds a local file for a song from another source.
    pub fn local_match(&self, artist: &str, title: &str) -> Option<&Track> {
        self.local_match
            .get(&match_key(artist, title))
            .and_then(|id| self.tracks.get(id))
    }

    /// Searches local tracks plus every track that is in a playlist.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Track> {
        let terms: Vec<String> = query.to_lowercase().split_whitespace().map(String::from).collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(&Track, u32)> = self
            .tracks
            .values()
            .filter_map(|t| {
                let title = t.title.to_lowercase();
                let hay = format!("{title} {} {}", t.artist.to_lowercase(), t.album.to_lowercase());
                if terms.iter().all(|term| hay.contains(term.as_str())) {
                    // Rank title matches first, then local files.
                    let mut score = 0;
                    if terms.iter().all(|term| title.contains(term.as_str())) {
                        score += 2;
                    }
                    if t.source == Source::Local {
                        score += 1;
                    }
                    Some((t, score))
                } else {
                    None
                }
            })
            .collect();
        hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.title.cmp(&b.0.title)));
        hits.into_iter().take(limit).map(|(t, _)| t.clone()).collect()
    }
}

/// The artists credited on a track. "A, B" is split only when "A" also appears as an artist on
/// its own (so "Tyler, The Creator" stays one name); featured artists are dropped.
pub fn credited_artists<'a>(artist: &'a str, solo: &HashSet<String>) -> Vec<&'a str> {
    const FEATURING: &[&str] = &[" feat. ", " feat ", " ft. ", " featuring ", " (feat. "];
    let main_len = artist
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| {
            FEATURING.iter().any(|sep| {
                artist
                    .get(i..i + sep.len())
                    .is_some_and(|s| s.eq_ignore_ascii_case(sep))
            })
        })
        .unwrap_or(artist.len());
    let main = artist[..main_len].trim();
    if main.is_empty() {
        return Vec::new();
    }
    let parts: Vec<&str> = main.split(", ").map(str::trim).filter(|p| !p.is_empty()).collect();
    if parts.len() > 1 && solo.contains(&parts[0].to_lowercase()) {
        let mut out: Vec<&str> = Vec::with_capacity(parts.len());
        for p in parts {
            if !out.iter().any(|o| o.eq_ignore_ascii_case(p)) {
                out.push(p);
            }
        }
        out
    } else {
        vec![main]
    }
}

pub fn liked_playlist() -> Playlist {
    Playlist {
        id: LIKED_ID.into(),
        name: "Liked Songs".into(),
        kind: PlaylistKind::Liked,
        remote_id: None,
        description: "Your favourites from every source".into(),
        art: None,
        track_ids: Vec::new(),
    }
}

/// Case-insensitive sort key that ignores a leading "The ".
pub fn sort_key(s: &str) -> String {
    let l = s.trim().to_lowercase();
    l.strip_prefix("the ").map(str::to_string).unwrap_or(l)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, artist: &str, album: &str, title: &str, no: u32) -> Track {
        Track {
            id: id.into(),
            source: if id.starts_with("local:") {
                Source::Local
            } else {
                Source::Spotify
            },
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            duration_ms: 1,
            track_no: Some(no),
            art: None,
            uri: id.into(),
            added_at: 0,
        }
    }

    #[test]
    fn indexes_albums_and_matches() {
        let mut lib = Library::default();
        for tr in [
            t("local:/b2", "The Band", "B", "Two", 2),
            t("local:/b1", "The Band", "B", "One", 1),
            t("local:/a1", "Abba", "Gold", "Waterloo", 1),
            t("spotify:track:1", "Abba", "Gold", "Waterloo", 1),
        ] {
            lib.tracks.insert(tr.id.clone(), tr);
        }
        lib.playlists.push(liked_playlist());
        lib.reindex();
        assert_eq!(lib.local, vec!["local:/a1", "local:/b1", "local:/b2"]);
        assert_eq!(lib.albums.len(), 2);
        assert_eq!(lib.albums[1].track_ids, vec!["local:/b1", "local:/b2"]);
        assert_eq!(lib.local_match("ABBA", "Waterloo").unwrap().id, "local:/a1");
        let hits = lib.search("waterloo", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].source, Source::Local);
    }

    #[test]
    fn credits_split_only_known_artists() {
        let solo: HashSet<String> = ["daft punk", "abba"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            credited_artists("Daft Punk, Pharrell Williams", &solo),
            vec!["Daft Punk", "Pharrell Williams"]
        );
        assert_eq!(
            credited_artists("Tyler, The Creator", &solo),
            vec!["Tyler, The Creator"]
        );
        assert_eq!(credited_artists("ABBA feat. Someone", &solo), vec!["ABBA"]);
        assert_eq!(credited_artists("Artist FT. Guest", &solo), vec!["Artist"]);
        assert_eq!(credited_artists("Beyoncé (feat. Jay-Z)", &solo), vec!["Beyoncé"]);
        assert_eq!(credited_artists("  ", &solo), Vec::<&str>::new());
    }

    #[test]
    fn indexes_artists_from_files_and_playlists() {
        let mut lib = Library::default();
        for tr in [
            t("local:/a2", "Abba", "Gold", "SOS", 2),
            t("local:/a1", "Abba", "Gold", "Waterloo", 1),
            t("spotify:track:1", "Daft Punk", "Discovery", "One More Time", 1),
            t("spotify:track:2", "Daft Punk, Pharrell Williams", "RAM", "Get Lucky", 8),
            t("spotify:track:3", "Not In Any Playlist", "X", "Y", 1),
        ] {
            lib.tracks.insert(tr.id.clone(), tr);
        }
        let mut p = liked_playlist();
        p.track_ids = vec!["spotify:track:2".into(), "spotify:track:1".into(), "local:/a1".into()];
        lib.playlists.push(p);
        lib.reindex();
        let names: Vec<&str> = lib.artists.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Abba", "Daft Punk", "Pharrell Williams"]);
        assert_eq!(lib.artist("ABBA").unwrap().track_ids, vec!["local:/a1", "local:/a2"]);
        assert_eq!(
            lib.artist("daft punk").unwrap().track_ids,
            vec!["spotify:track:2", "spotify:track:1"]
        );
        assert!(lib.artist("not in any playlist").is_none());
        let found: Vec<&str> = lib.search_artists("punk", 5).iter().map(|a| a.name.as_str()).collect();
        assert_eq!(found, vec!["Daft Punk"]);
        assert_eq!(lib.search_artists("p", 5)[0].name, "Pharrell Williams");
    }
}
