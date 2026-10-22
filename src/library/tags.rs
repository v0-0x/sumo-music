//! Writes tags (and cover art) into downloaded files.

use std::path::Path;

use anyhow::{Context, Result};
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::items::Timestamp;
use lofty::tag::Tag;

/// Everything known about a song, for its file's tags. Empty fields are left out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    pub title: String,
    /// All credited artists ("A, B").
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub genre: String,
    /// "2021", "2021-03" or "2021-03-05".
    pub date: String,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub isrc: String,
    pub label: String,
    pub copyright: String,
    /// The song's page (written as the comment).
    pub url: String,
    /// Plain or time-synced (LRC) lyrics.
    pub lyrics: String,
    /// Cover art to download, best first. Not written itself.
    pub cover_urls: Vec<String>,
}

/// Writes `meta` and `cover` (JPEG or PNG) into the file. With `keep_existing`, only fills in
/// what the file doesn't have yet (for files their uploader tagged).
pub fn write(path: &Path, meta: &Metadata, cover: Option<&[u8]>, keep_existing: bool) -> Result<()> {
    let mut file = lofty::read_from_path(path).with_context(|| format!("couldn't read {}", path.display()))?;
    let kind = file.primary_tag_type();
    if file.tag(kind).is_none() {
        file.insert_tag(Tag::new(kind));
    }
    let tag = file.tag_mut(kind).context("no tag")?;

    let texts = [
        (ItemKey::TrackTitle, &meta.title),
        (ItemKey::TrackArtist, &meta.artist),
        (ItemKey::AlbumTitle, &meta.album),
        (ItemKey::AlbumArtist, &meta.album_artist),
        (ItemKey::Genre, &meta.genre),
        (ItemKey::Isrc, &meta.isrc),
        (ItemKey::Label, &meta.label),
        (ItemKey::CopyrightMessage, &meta.copyright),
        (ItemKey::Comment, &meta.url),
        (ItemKey::Lyrics, &meta.lyrics),
    ];
    for (key, value) in texts {
        let value = value.trim();
        let has = tag.get_string(key).is_some_and(|v| !v.trim().is_empty());
        if !value.is_empty() && !(keep_existing && has) {
            let stored = tag.insert_text(key, value.to_string());
            // ID3 has no label field (taggers use the publisher frame) and keeps lyrics, synced
            // ones in LRC form too, in USLT. Other fields a format can't hold are left out.
            let fallback = match key {
                ItemKey::Label => Some(ItemKey::Publisher),
                ItemKey::Lyrics => Some(ItemKey::UnsyncLyrics),
                _ => None,
            };
            if let Some(other) = fallback.filter(|_| !stored) {
                tag.insert_text(other, value.to_string());
            }
        }
    }
    let numbers = [
        (meta.track, tag.track(), 0),
        (meta.track_total, tag.track_total(), 1),
        (meta.disc, tag.disk(), 2),
        (meta.disc_total, tag.disk_total(), 3),
    ];
    for (value, old, which) in numbers {
        let Some(n) = value.filter(|n| *n > 0) else { continue };
        if keep_existing && old.is_some() {
            continue;
        }
        match which {
            0 => tag.set_track(n),
            1 => tag.set_track_total(n),
            2 => tag.set_disk(n),
            _ => tag.set_disk_total(n),
        }
    }
    if let Ok(date) = meta.date.trim().parse::<Timestamp>() {
        if !(keep_existing && tag.date().is_some()) {
            tag.set_date(date);
        }
    }
    let has_cover = tag.pictures().iter().any(|p| p.pic_type() == PictureType::CoverFront);
    if let Some((data, mime)) = cover.and_then(|d| Some((d, image_mime(d)?))) {
        if !(keep_existing && has_cover) {
            tag.remove_picture_type(PictureType::CoverFront);
            tag.push_picture(
                Picture::unchecked(data.to_vec())
                    .pic_type(PictureType::CoverFront)
                    .mime_type(mime)
                    .build(),
            );
        }
    }
    tag.save_to_path(path, WriteOptions::default())
        .with_context(|| format!("couldn't write tags to {}", path.display()))
}

/// A detail the tag editor shows and changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Genre,
    Date,
    Track,
    TrackTotal,
    Disc,
    DiscTotal,
    Lyrics,
}

impl Field {
    pub const ALL: [Field; 11] = [
        Field::Title,
        Field::Artist,
        Field::Album,
        Field::AlbumArtist,
        Field::Genre,
        Field::Date,
        Field::Track,
        Field::TrackTotal,
        Field::Disc,
        Field::DiscTotal,
        Field::Lyrics,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::Title => "Title",
            Field::Artist => "Artist",
            Field::Album => "Album",
            Field::AlbumArtist => "Album artist",
            Field::Genre => "Genre",
            Field::Date => "Year or date",
            Field::Track => "Track",
            Field::TrackTotal => "Tracks on the album",
            Field::Disc => "Disc",
            Field::DiscTotal => "Discs",
            Field::Lyrics => "Lyrics",
        }
    }

    /// Whether it makes sense to give several songs the same value.
    pub fn shared(self) -> bool {
        !matches!(self, Field::Title | Field::Track | Field::Lyrics)
    }

    /// The field's text in `meta` ("" when not set).
    pub fn get(self, meta: &Metadata) -> String {
        let number = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
        match self {
            Field::Title => meta.title.clone(),
            Field::Artist => meta.artist.clone(),
            Field::Album => meta.album.clone(),
            Field::AlbumArtist => meta.album_artist.clone(),
            Field::Genre => meta.genre.clone(),
            Field::Date => meta.date.clone(),
            Field::Track => number(meta.track),
            Field::TrackTotal => number(meta.track_total),
            Field::Disc => number(meta.disc),
            Field::DiscTotal => number(meta.disc_total),
            Field::Lyrics => meta.lyrics.clone(),
        }
    }

    /// Whether `value` can be written (numbers for numbers, dates for the date).
    pub fn check(self, value: &str) -> Result<(), String> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(());
        }
        match self {
            Field::Date if value.parse::<Timestamp>().is_err() => {
                Err(format!("“{value}” isn't a date: use 2021 or 2021-03-05"))
            }
            Field::Track | Field::TrackTotal | Field::Disc | Field::DiscTotal
                if !value.parse::<u32>().is_ok_and(|n| n > 0) =>
            {
                Err(format!("{} has to be a number", self.label()))
            }
            _ => Ok(()),
        }
    }

    fn key(self) -> Option<ItemKey> {
        Some(match self {
            Field::Title => ItemKey::TrackTitle,
            Field::Artist => ItemKey::TrackArtist,
            Field::Album => ItemKey::AlbumTitle,
            Field::AlbumArtist => ItemKey::AlbumArtist,
            Field::Genre => ItemKey::Genre,
            Field::Lyrics => ItemKey::Lyrics,
            _ => return None,
        })
    }
}

/// What a file's tags say, for the tag editor (cover art left out).
pub fn read(path: &Path) -> Result<Metadata> {
    let file = lofty::probe::Probe::open(path)
        .and_then(|p| {
            p.options(lofty::config::ParseOptions::new().read_cover_art(false))
                .read()
        })
        .with_context(|| format!("couldn't read {}", path.display()))?;
    let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) else {
        return Ok(Metadata::default());
    };
    let text = |key: ItemKey| tag.get_string(key).map(str::trim).unwrap_or_default().to_string();
    let mut lyrics = text(ItemKey::Lyrics);
    if lyrics.is_empty() {
        lyrics = text(ItemKey::UnsyncLyrics);
    }
    Ok(Metadata {
        title: text(ItemKey::TrackTitle),
        artist: text(ItemKey::TrackArtist),
        album: text(ItemKey::AlbumTitle),
        album_artist: text(ItemKey::AlbumArtist),
        genre: text(ItemKey::Genre),
        date: tag.date().map(|d| d.to_string()).unwrap_or_default(),
        // "0/12" (a total without a number) means no number.
        track: tag.track().filter(|n| *n > 0),
        track_total: tag.track_total().filter(|n| *n > 0),
        disc: tag.disk().filter(|n| *n > 0),
        disc_total: tag.disk_total().filter(|n| *n > 0),
        lyrics,
        ..Metadata::default()
    })
}

/// A new cover for [`edit`].
pub enum CoverEdit {
    /// JPEG or PNG data.
    Set(Vec<u8>),
    Remove,
}

/// Changes some details of a file: each field gets the text given, and an empty text removes
/// the field. Everything else in the file stays as it is.
pub fn edit(path: &Path, changes: &[(Field, String)], cover: Option<&CoverEdit>) -> Result<()> {
    let mut file = lofty::read_from_path(path).with_context(|| format!("couldn't read {}", path.display()))?;
    let kind = file.primary_tag_type();
    if file.tag(kind).is_none() {
        // Keep what another kind of tag in the file says (an MP3 with only ID3v1, say).
        let mut tag = file.first_tag().cloned().unwrap_or_else(|| Tag::new(kind));
        tag.re_map(kind);
        file.insert_tag(tag);
    }
    let tag = file.tag_mut(kind).context("no tag")?;
    for (field, value) in changes {
        let value = value.trim();
        if let Some(key) = field.key() {
            tag.remove_key(key);
            if *field == Field::Lyrics {
                tag.remove_key(ItemKey::UnsyncLyrics);
            }
            if !value.is_empty() && !tag.insert_text(key, value.to_string()) && *field == Field::Lyrics {
                tag.insert_text(ItemKey::UnsyncLyrics, value.to_string());
            }
            continue;
        }
        field.check(value).map_err(anyhow::Error::msg)?;
        if *field == Field::Date {
            match value.parse::<Timestamp>() {
                Ok(date) => tag.set_date(date),
                Err(_) => tag.remove_date(),
            }
            continue;
        }
        let number = value.parse::<u32>().ok();
        match (field, number) {
            (Field::Track, Some(n)) => tag.set_track(n),
            (Field::Track, None) => tag.remove_track(),
            (Field::TrackTotal, Some(n)) => tag.set_track_total(n),
            (Field::TrackTotal, None) => tag.remove_track_total(),
            (Field::Disc, Some(n)) => tag.set_disk(n),
            (Field::Disc, None) => tag.remove_disk(),
            (Field::DiscTotal, Some(n)) => tag.set_disk_total(n),
            _ => tag.remove_disk_total(),
        }
    }
    match cover {
        Some(CoverEdit::Set(data)) => {
            let mime = image_mime(data).context("the cover has to be a JPEG or PNG image")?;
            tag.remove_picture_type(PictureType::CoverFront);
            tag.push_picture(
                Picture::unchecked(data.clone())
                    .pic_type(PictureType::CoverFront)
                    .mime_type(mime)
                    .build(),
            );
        }
        Some(CoverEdit::Remove) => tag.remove_picture_type(PictureType::CoverFront),
        None => {}
    }
    tag.save_to_path(path, WriteOptions::default())
        .with_context(|| format!("couldn't write tags to {}", path.display()))
}

/// An image as cover data a tag can hold: JPEG and PNG as they are, anything else (WebP)
/// turned into a JPEG.
pub fn cover_data(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if image_mime(&bytes).is_some() {
        return Ok(bytes);
    }
    let img = image::load_from_memory(&bytes).context("that isn't an image")?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.to_rgb8()
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .context("couldn't convert the image")?;
    Ok(out.into_inner())
}

fn image_mime(data: &[u8]) -> Option<MimeType> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(MimeType::Jpeg)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(MimeType::Png)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny valid WAV file (0.1 s of silence).
    fn wav() -> Vec<u8> {
        let samples = 4410u32;
        let data_len = samples * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&44100u32.to_le_bytes());
        out.extend_from_slice(&88200u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(out.len() + data_len as usize, 0);
        out
    }

    #[test]
    fn writes_full_metadata_and_cover() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tags.wav");
        std::fs::write(&path, wav()).unwrap();
        let cover = [&[0xFF, 0xD8, 0xFF, 0xE0][..], &[0u8; 32]].concat();
        let meta = Metadata {
            title: "Song".into(),
            artist: "Someone, Other".into(),
            album: "The Album".into(),
            album_artist: "Someone".into(),
            genre: "Electronic".into(),
            date: "2021-03-05".into(),
            track: Some(3),
            track_total: Some(12),
            disc: Some(1),
            disc_total: Some(2),
            isrc: "USUM72100001".into(),
            label: "Label".into(),
            copyright: "℗ 2021 Label".into(),
            url: "https://open.spotify.com/track/x".into(),
            lyrics: "[00:01.00]Hello\n[00:02.50]World".into(),
            cover_urls: vec![],
        };
        write(&path, &meta, Some(&cover), false).unwrap();

        let file = lofty::read_from_path(&path).unwrap();
        let tag = file.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Song"));
        assert_eq!(tag.artist().as_deref(), Some("Someone, Other"));
        assert_eq!(tag.album().as_deref(), Some("The Album"));
        assert_eq!(tag.get_string(ItemKey::AlbumArtist), Some("Someone"));
        assert_eq!(tag.genre().as_deref(), Some("Electronic"));
        assert_eq!(tag.get_string(ItemKey::Isrc), Some("USUM72100001"));
        let label = tag.get_string(ItemKey::Label).or(tag.get_string(ItemKey::Publisher));
        assert_eq!(label, Some("Label"));
        assert_eq!(tag.get_string(ItemKey::CopyrightMessage), Some("℗ 2021 Label"));
        assert_eq!((tag.track(), tag.track_total()), (Some(3), Some(12)));
        assert_eq!((tag.disk(), tag.disk_total()), (Some(1), Some(2)));
        let date = tag.date().unwrap();
        assert_eq!((date.year, date.month, date.day), (2021, Some(3), Some(5)));
        assert_eq!(tag.pictures().len(), 1);
        assert_eq!(tag.pictures()[0].mime_type(), Some(&MimeType::Jpeg));

        // The app reads the same details (and synced lyrics) back.
        let track = super::super::scanner::read_track(&path, None, 0);
        assert_eq!(
            (track.title.as_str(), track.artist.as_str()),
            ("Song", "Someone, Other")
        );
        assert_eq!((track.album.as_str(), track.track_no), ("The Album", Some(3)));
        assert_eq!(track.duration_ms, 100);
        let lyrics = crate::integrations::lyrics::local_lyrics(&path).unwrap();
        assert_eq!(lyrics.synced.len(), 2);
        assert_eq!(lyrics.synced[1].time_ms, 2500);

        // Filling in keeps what is there.
        let more = Metadata {
            title: "Other".into(),
            genre: "Pop".into(),
            ..Metadata::default()
        };
        write(&path, &more, None, true).unwrap();
        let file = lofty::read_from_path(&path).unwrap();
        let tag = file.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Song"));
        assert_eq!(tag.genre().as_deref(), Some("Electronic"));
        assert_eq!(tag.pictures().len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn editing_changes_only_what_is_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("edit.wav");
        std::fs::write(&path, wav()).unwrap();
        let start = Metadata {
            title: "Song".into(),
            artist: "Someone".into(),
            album: "Album".into(),
            genre: "Pop".into(),
            track: Some(2),
            ..Metadata::default()
        };
        write(&path, &start, None, false).unwrap();
        let read_back = read(&path).unwrap();
        assert_eq!((read_back.title.as_str(), read_back.track), ("Song", Some(2)));

        let cover = [&[0xFF, 0xD8, 0xFF, 0xE0][..], &[0u8; 32]].concat();
        let changes = vec![
            (Field::Title, "New Title".to_string()),
            (Field::Genre, String::new()),
            (Field::Date, "2019-04-01".to_string()),
            (Field::Track, "5".to_string()),
            (Field::TrackTotal, "12".to_string()),
            (Field::Lyrics, "[00:01.00]Hi".to_string()),
        ];
        edit(&path, &changes, Some(&CoverEdit::Set(cover))).unwrap();
        let m = read(&path).unwrap();
        assert_eq!(
            (m.title.as_str(), m.artist.as_str(), m.album.as_str()),
            ("New Title", "Someone", "Album")
        );
        assert_eq!(m.genre, "");
        assert!(m.date.starts_with("2019-04-01"), "{}", m.date);
        assert_eq!((m.track, m.track_total), (Some(5), Some(12)));
        assert_eq!(m.lyrics, "[00:01.00]Hi");
        let file = lofty::read_from_path(&path).unwrap();
        assert_eq!(file.primary_tag().unwrap().pictures().len(), 1);

        // Bad numbers and dates are refused and leave the file alone.
        assert!(edit(&path, &[(Field::Disc, "two".into())], None).is_err());
        assert!(edit(&path, &[(Field::Date, "soon".into())], None).is_err());
        edit(&path, &[(Field::Track, String::new())], Some(&CoverEdit::Remove)).unwrap();
        let m = read(&path).unwrap();
        assert_eq!((m.title.as_str(), m.track), ("New Title", None));
        let file = lofty::read_from_path(&path).unwrap();
        assert!(file.primary_tag().unwrap().pictures().is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn field_checks() {
        assert!(Field::Track.check("3").is_ok());
        assert!(Field::Track.check("").is_ok());
        assert!(Field::Track.check("0").is_err());
        assert!(Field::Date.check("2021").is_ok());
        assert!(Field::Date.check("2021-03-05").is_ok());
        assert!(Field::Date.check("March").is_err());
        assert!(Field::Title.check("anything").is_ok());
    }

    #[test]
    fn webp_covers_become_jpeg() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(4, 4)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let png = png.into_inner();
        assert_eq!(cover_data(png.clone()).unwrap(), png);
        let mut webp = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(4, 4)
            .write_to(&mut webp, image::ImageFormat::WebP)
            .unwrap();
        let jpeg = cover_data(webp.into_inner()).unwrap();
        assert_eq!(image_mime(&jpeg), Some(MimeType::Jpeg));
        assert!(cover_data(b"nope".to_vec()).is_err());
    }

    #[test]
    fn unknown_images_are_skipped() {
        assert_eq!(image_mime(b"GIF89a"), None);
        assert_eq!(image_mime(b"\x89PNG\r\n\x1a\n...."), Some(MimeType::Png));
    }
}
