//! SQLite persistence for tracks, playlists and small bits of state.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{Playlist, PlaylistKind, Source, Track};

pub struct Db {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tracks (
    id          TEXT PRIMARY KEY,
    source      TEXT NOT NULL,
    title       TEXT NOT NULL,
    artist      TEXT NOT NULL,
    album       TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    track_no    INTEGER,
    art         TEXT,
    uri         TEXT NOT NULL,
    added_at    INTEGER NOT NULL DEFAULT 0,
    mtime       INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS playlists (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    remote_id   TEXT,
    description TEXT NOT NULL DEFAULT '',
    art         TEXT,
    position    INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS playlist_tracks (
    playlist_id TEXT NOT NULL,
    pos         INTEGER NOT NULL,
    track_id    TEXT NOT NULL,
    PRIMARY KEY (playlist_id, pos)
);
CREATE TABLE IF NOT EXISTS plays (
    track_id    TEXT NOT NULL,
    played_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS plays_time ON plays(played_at);
CREATE TABLE IF NOT EXISTS kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Brings a database made by an older version up to date. `user_version` counts the steps
/// done; each runs once.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let steps: &[&str] = &[
        // 1 (0.6.0): album artist and disc number of local files; plays looked up by song.
        "ALTER TABLE tracks ADD COLUMN album_artist TEXT NOT NULL DEFAULT '';
         ALTER TABLE tracks ADD COLUMN disc_no INTEGER;
         CREATE INDEX IF NOT EXISTS plays_track ON plays(track_id);",
    ];
    for (i, step) in steps.iter().enumerate().skip(version.max(0) as usize) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(step)
            .with_context(|| format!("updating the database (step {})", i + 1))?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Keep SQLite's page cache small; the library is held in memory anyway.
        conn.pragma_update(None, "cache_size", -512)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Db { conn })
    }

    pub fn load_tracks(&self) -> Result<Vec<Track>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source, title, artist, album, duration_ms, track_no, art, uri, added_at, album_artist, disc_no
             FROM tracks",
        )?;
        let rows = stmt.query_map([], |r| {
            let source: String = r.get(1)?;
            Ok(Track {
                album_artist: r.get(10)?,
                disc_no: r.get::<_, Option<i64>>(11)?.map(|n| n as u32),
                id: r.get(0)?,
                source: Source::parse(&source).unwrap_or(Source::Local),
                title: r.get(2)?,
                artist: r.get(3)?,
                album: r.get(4)?,
                duration_ms: r.get::<_, i64>(5)? as u64,
                track_no: r.get::<_, Option<i64>>(6)?.map(|n| n as u32),
                art: r.get(7)?,
                uri: r.get(8)?,
                added_at: r.get(9)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Insert or update tracks. `mtimes` holds file modification times for local files.
    pub fn upsert_tracks(&mut self, tracks: &[Track], mtimes: &HashMap<String, i64>) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO tracks (id, source, title, artist, album, duration_ms, track_no, art, uri, added_at, mtime,
                    album_artist, disc_no)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(id) DO UPDATE SET
                    source = excluded.source, title = excluded.title, artist = excluded.artist,
                    album = excluded.album, duration_ms = excluded.duration_ms, track_no = excluded.track_no,
                    art = excluded.art, uri = excluded.uri,
                    album_artist = excluded.album_artist, disc_no = excluded.disc_no,
                    added_at = CASE WHEN tracks.added_at = 0 THEN excluded.added_at ELSE tracks.added_at END,
                    mtime = excluded.mtime",
            )?;
            for t in tracks {
                stmt.execute(params![
                    t.id,
                    t.source.as_str(),
                    t.title,
                    t.artist,
                    t.album,
                    t.duration_ms as i64,
                    t.track_no.map(|n| n as i64),
                    t.art,
                    t.uri,
                    t.added_at,
                    mtimes.get(&t.id).copied().unwrap_or(0),
                    t.album_artist,
                    t.disc_no.map(|n| n as i64),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_tracks(&mut self, ids: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM tracks WHERE id = ?1")?;
            for id in ids {
                stmt.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// `track id -> mtime` for local files, used for incremental rescans.
    pub fn local_mtimes(&self) -> Result<HashMap<String, i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, mtime FROM tracks WHERE source = 'local'")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn load_playlists(&self) -> Result<Vec<Playlist>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, kind, remote_id, description, art FROM playlists ORDER BY position, rowid")?;
        let mut playlists: Vec<Playlist> = stmt
            .query_map([], |r| {
                let kind: String = r.get(2)?;
                Ok(Playlist {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: PlaylistKind::parse(&kind).unwrap_or(PlaylistKind::Custom),
                    remote_id: r.get(3)?,
                    description: r.get(4)?,
                    art: r.get(5)?,
                    track_ids: Vec::new(),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut stmt = self
            .conn
            .prepare("SELECT playlist_id, track_id FROM playlist_tracks ORDER BY playlist_id, pos")?;
        let mut by_playlist: HashMap<String, Vec<String>> = HashMap::new();
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for (pid, tid) in rows.flatten() {
            by_playlist.entry(pid).or_default().push(tid);
        }
        for p in &mut playlists {
            if let Some(ids) = by_playlist.remove(&p.id) {
                p.track_ids = ids;
            }
        }
        Ok(playlists)
    }

    pub fn save_playlist(&mut self, p: &Playlist, position: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO playlists (id, name, kind, remote_id, description, art, position)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, kind = excluded.kind,
                remote_id = excluded.remote_id, description = excluded.description,
                art = excluded.art, position = excluded.position",
            params![
                p.id,
                p.name,
                p.kind.as_str(),
                p.remote_id,
                p.description,
                p.art,
                position
            ],
        )?;
        tx.execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1", [&p.id])?;
        {
            let mut stmt =
                tx.prepare("INSERT INTO playlist_tracks (playlist_id, pos, track_id) VALUES (?1, ?2, ?3)")?;
            for (i, tid) in p.track_ids.iter().enumerate() {
                stmt.execute(params![p.id, i as i64, tid])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Stores the order of the playlists (`ids` first to last), without touching their songs.
    pub fn set_playlist_order(&mut self, ids: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE playlists SET position = ?1 WHERE id = ?2")?;
            for (i, id) in ids.iter().enumerate() {
                stmt.execute(params![i as i64, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_playlist(&mut self, id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1", [id])?;
        tx.execute("DELETE FROM playlists WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_play(&self, track_id: &str, at: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO plays (track_id, played_at) VALUES (?1, ?2)",
            params![track_id, at],
        )?;
        Ok(())
    }

    /// Most recently played distinct track ids, newest first.
    pub fn recent_plays(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT track_id, MAX(played_at) AS t FROM plays GROUP BY track_id ORDER BY t DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Ids of songs played at least twice, most played (then most recently played) first.
    pub fn top_plays(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT track_id, COUNT(*) AS c, MAX(played_at) AS last FROM plays GROUP BY track_id
             HAVING c >= 2 ORDER BY c DESC, last DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Keeps the newest `keep` plays (the history only grows otherwise). Returns how many
    /// were dropped.
    pub fn trim_plays(&self, keep: usize) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM plays WHERE rowid NOT IN (SELECT rowid FROM plays ORDER BY played_at DESC LIMIT ?1)",
            [keep as i64],
        )?)
    }

    pub fn get_kv(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_kv(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Entries whose key starts with `prefix`, with the prefix cut off.
    pub fn kv_with_prefix(&self, prefix: &str) -> Vec<(String, String)> {
        let rows = self
            .conn
            .prepare("SELECT key, value FROM kv WHERE substr(key, 1, ?2) = ?1")
            .and_then(|mut stmt| {
                stmt.query_map(params![prefix, prefix.chars().count() as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            });
        rows.unwrap_or_default()
            .into_iter()
            .filter_map(|(k, v)| Some((k.strip_prefix(prefix)?.to_string(), v)))
            .collect()
    }

    /// Removes tracks of remote sources that are no longer referenced by any playlist.
    pub fn prune_orphans(&mut self) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM tracks WHERE source != 'local'
               AND id NOT IN (SELECT DISTINCT track_id FROM playlist_tracks)",
            [],
        )?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, source: Source) -> Track {
        Track {
            album_artist: String::new(),
            disc_no: None,
            id: id.into(),
            source,
            title: format!("Title {id}"),
            artist: "Artist".into(),
            album: "Album".into(),
            duration_ms: 1000,
            track_no: Some(1),
            art: None,
            uri: id.into(),
            added_at: 5,
        }
    }

    /// An older database gets the new columns once; album artist and disc come back.
    #[test]
    fn old_databases_are_updated() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("library.db");
        {
            // As 0.5 made it: no migrations run yet.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO tracks (id, source, title, artist, album, duration_ms, uri) VALUES ('local:/a', 'local', 'A', 'B', 'C', 1, '/a')",
                [],
            )
            .unwrap();
        }
        let mut db = Db::open(&path).unwrap();
        let old = db.load_tracks().unwrap();
        assert_eq!((old.len(), old[0].album_artist.as_str(), old[0].disc_no), (1, "", None));
        let mut t = track("local:/b", Source::Local);
        t.album_artist = "Various Artists".into();
        t.disc_no = Some(2);
        db.upsert_tracks(std::slice::from_ref(&t), &HashMap::new()).unwrap();
        drop(db);
        // Opening again doesn't run the steps twice.
        let db = Db::open(&path).unwrap();
        let back = db.load_tracks().unwrap().into_iter().find(|x| x.id == t.id).unwrap();
        assert_eq!(back, t);
        let version: i64 = db.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 1);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tracks_and_playlists_roundtrip() {
        let mut db = Db::open_in_memory().unwrap();
        let tracks = vec![
            track("local:/a.flac", Source::Local),
            track("spotify:track:x", Source::Spotify),
        ];
        db.upsert_tracks(&tracks, &HashMap::new()).unwrap();
        let mut loaded = db.load_tracks().unwrap();
        loaded.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0], tracks[0]);

        let p = Playlist {
            id: "custom:1".into(),
            name: "Mix".into(),
            kind: PlaylistKind::Custom,
            remote_id: None,
            description: String::new(),
            art: None,
            track_ids: vec!["spotify:track:x".into(), "local:/a.flac".into()],
        };
        db.save_playlist(&p, 0).unwrap();
        assert_eq!(db.load_playlists().unwrap(), vec![p.clone()]);

        // The order can change without rewriting the songs.
        let q = Playlist {
            id: "custom:2".into(),
            name: "Second".into(),
            ..p.clone()
        };
        db.save_playlist(&q, 1).unwrap();
        db.set_playlist_order(&["custom:2".into(), "custom:1".into()]).unwrap();
        let order: Vec<String> = db.load_playlists().unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(order, ["custom:2", "custom:1"]);
        assert_eq!(db.load_playlists().unwrap()[1].track_ids, p.track_ids);
        db.delete_playlist("custom:2").unwrap();

        // Orphan pruning keeps referenced and local tracks.
        db.upsert_tracks(&[track("soundcloud:9", Source::SoundCloud)], &HashMap::new())
            .unwrap();
        assert_eq!(db.prune_orphans().unwrap(), 1);
        db.delete_playlist("custom:1").unwrap();
        assert!(db.load_playlists().unwrap().is_empty());
    }

    #[test]
    fn plays_and_kv() {
        let db = Db::open_in_memory().unwrap();
        db.record_play("a", 1).unwrap();
        db.record_play("b", 2).unwrap();
        db.record_play("a", 3).unwrap();
        assert_eq!(db.recent_plays(10).unwrap(), vec!["a".to_string(), "b".to_string()]);
        // Played once isn't "most played" yet.
        assert_eq!(db.top_plays(10).unwrap(), vec!["a".to_string()]);
        assert_eq!(db.trim_plays(2).unwrap(), 1);
        assert_eq!(db.recent_plays(10).unwrap(), vec!["a".to_string(), "b".to_string()]);
        db.set_kv("k", "v").unwrap();
        assert_eq!(db.get_kv("k").as_deref(), Some("v"));
        db.set_kv("download:soundcloud:1", "/a.mp3").unwrap();
        db.set_kv("download:soundcloud:2", "/b.mp3").unwrap();
        db.set_kv("downloads", "x").unwrap();
        let mut got = db.kv_with_prefix("download:");
        got.sort();
        assert_eq!(
            got,
            vec![
                ("soundcloud:1".to_string(), "/a.mp3".to_string()),
                ("soundcloud:2".to_string(), "/b.mp3".to_string())
            ]
        );
    }
}
