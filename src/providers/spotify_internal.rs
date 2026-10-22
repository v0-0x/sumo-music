//! Spotify library import through librespot's own session (the "spclient" endpoints the
//! official apps use). Unlike the public Web API this isn't rate limited per client id, so
//! syncing a big library works even when the shared Web API client id is throttled.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use http::{HeaderMap, HeaderValue, Method};
use librespot_core::session::Session;
use librespot_protocol::extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery};
use librespot_protocol::extension_kind::ExtensionKind;
use librespot_protocol::metadata;
use librespot_protocol::playlist4_external::SelectedListContent;
use protobuf::{CodedInputStream, CodedOutputStream, EnumOrUnknown, Message};

use crate::library::tags::Metadata;
use crate::model::{Source, Track};
use crate::providers::spotify_api::SpotifyPlaylistMeta;

/// Track metadata is fetched in batches of this many URIs.
const METADATA_BATCH: usize = 100;
const ROOTLIST_PAGE: usize = 120;

/// A playlist with its item URIs (`spotify:track:…`) and when each was added.
pub struct PlaylistContents {
    pub meta: SpotifyPlaylistMeta,
    pub items: Vec<(String, i64)>,
}

/// The user's playlists in library order (followed and own), as base62 ids.
pub async fn rootlist(session: &Session) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut from = 0;
    loop {
        let bytes = session
            .spclient()
            .get_rootlist(from, Some(ROOTLIST_PAGE))
            .await
            .map_err(|e| anyhow!("loading your playlists: {e}"))?;
        let list = SelectedListContent::parse_from_bytes(&bytes).context("parsing rootlist")?;
        let items = &list.contents.items;
        for item in items.iter() {
            if let Some(id) = playlist_id_from_uri(item.uri()) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        from += items.len();
        let total = list.length().max(0) as usize;
        if items.len() < ROOTLIST_PAGE || from >= total || items.is_empty() {
            break;
        }
    }
    Ok(ids)
}

/// `spotify:playlist:ID` or legacy `spotify:user:NAME:playlist:ID` -> `ID`.
pub fn playlist_id_from_uri(uri: &str) -> Option<String> {
    let parts: Vec<&str> = uri.split(':').collect();
    match parts.as_slice() {
        ["spotify", "playlist", id] => Some(id.to_string()),
        ["spotify", "user", _, "playlist", id] => Some(id.to_string()),
        _ => None,
    }
}

/// Loads one playlist with all its items.
pub async fn playlist(session: &Session, id: &str) -> Result<PlaylistContents> {
    let mut items: Vec<(String, i64)> = Vec::new();
    let mut endpoint = format!("/playlist/v2/playlist/{id}");
    let mut meta: Option<SpotifyPlaylistMeta> = None;
    for _ in 0..200 {
        let bytes = session
            .spclient()
            .request(&Method::GET, &endpoint, None, None)
            .await
            .map_err(|e| anyhow!("loading playlist {id}: {e}"))?;
        let list = SelectedListContent::parse_from_bytes(&bytes).context("parsing playlist")?;
        if meta.is_none() {
            meta = Some(playlist_meta(id, &list));
        }
        let before = items.len();
        for item in list.contents.items.iter() {
            let added = item.attributes.timestamp() / 1000;
            items.push((item.uri().to_string(), added));
        }
        let total = list.length().max(0) as usize;
        let truncated = list.contents.truncated();
        if items.len() >= total || items.len() == before || !truncated && list.contents.items.len() >= total {
            break;
        }
        endpoint = format!("/playlist/v2/playlist/{id}?from={}&length=500", items.len());
    }
    let mut meta = meta.ok_or_else(|| anyhow!("empty playlist response"))?;
    meta.total = items.len() as u32;
    Ok(PlaylistContents { meta, items })
}

fn playlist_meta(id: &str, list: &SelectedListContent) -> SpotifyPlaylistMeta {
    let attrs = &list.attributes;
    let art = attrs
        .picture_size
        .iter()
        .find(|p| p.target_name() == "default")
        .or_else(|| attrs.picture_size.first())
        .map(|p| p.url().to_string())
        .filter(|u| u.starts_with("http"))
        .or_else(|| (!attrs.picture().is_empty()).then(|| image_url(attrs.picture())));
    SpotifyPlaylistMeta {
        id: id.to_string(),
        name: attrs.name().to_string(),
        description: attrs.description().to_string(),
        art,
        total: list.length().max(0) as u32,
        snapshot_id: hex(list.revision()),
        owner: list
            .contents
            .meta_items
            .first()
            .map(|m| m.owner_username().to_string())
            .unwrap_or_default(),
    }
}

/// Everything in the "collection" set: liked songs (`spotify:track:…`) and saved albums
/// (`spotify:album:…`), newest first.
pub async fn collection(session: &Session) -> Result<Vec<(String, i64)>> {
    let username = session.username();
    let mut headers = HeaderMap::new();
    let ct = HeaderValue::from_static("application/vnd.collection-v2.spotify.proto");
    headers.insert(http::header::CONTENT_TYPE, ct.clone());
    headers.insert(http::header::ACCEPT, ct);
    let mut token = String::new();
    let mut out = Vec::new();
    for _ in 0..500 {
        let body = encode_page_request(&username, "collection", &token, 300)?;
        let bytes = session
            .spclient()
            .request(
                &Method::POST,
                "/collection/v2/paging",
                Some(headers.clone()),
                Some(&body),
            )
            .await
            .map_err(|e| anyhow!("loading your library: {e}"))?;
        let page = decode_page_response(&bytes)?;
        out.extend(
            page.items
                .into_iter()
                .filter(|i| !i.is_removed)
                .map(|i| (i.uri, i.added_at)),
        );
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    out.sort_by_key(|item| std::cmp::Reverse(item.1));
    Ok(out)
}

/// Raw extended-metadata payloads for many URIs, in batches.
async fn fetch_batch(session: &Session, uris: &[String], kind: ExtensionKind) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::with_capacity(uris.len());
    for chunk in uris.chunks(METADATA_BATCH) {
        let request = BatchedEntityRequest {
            entity_request: chunk
                .iter()
                .map(|uri| EntityRequest {
                    entity_uri: uri.clone(),
                    query: vec![ExtensionQuery {
                        extension_kind: EnumOrUnknown::new(kind),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let response = session
            .spclient()
            .get_extended_metadata(request)
            .await
            .map_err(|e| anyhow!("loading details from Spotify: {e}"))?;
        for array in response.extended_metadata.iter() {
            for data in array.extension_data.iter() {
                if let Some(any) = data.extension_data.as_ref() {
                    out.push((data.entity_uri.clone(), any.value.clone()));
                }
            }
        }
    }
    Ok(out)
}

/// Fetches metadata for `spotify:track:` URIs in batches.
pub async fn tracks(session: &Session, uris: &[String]) -> Result<HashMap<String, Track>> {
    let mut out = HashMap::with_capacity(uris.len());
    for (uri, bytes) in fetch_batch(session, uris, ExtensionKind::TRACK_V4).await? {
        let Ok(track) = metadata::Track::parse_from_bytes(&bytes) else {
            continue;
        };
        if let Some(t) = convert_track(&uri, &track) {
            out.insert(t.id.clone(), t);
        }
    }
    Ok(out)
}

/// Header and tracks of an artist / album / playlist page.
pub struct PageData {
    pub title: String,
    pub subtitle: String,
    /// An album's artist (empty for other pages).
    pub artist: String,
    pub image: Option<String>,
    pub tracks: Vec<Track>,
}

/// Tracks for `uris`, in that order, skipping anything without metadata.
async fn ordered_tracks(session: &Session, uris: &[String]) -> Result<Vec<Track>> {
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<String> = uris
        .iter()
        .filter(|u| u.starts_with("spotify:track:") && seen.insert(u.as_str()))
        .cloned()
        .collect();
    let mut map = tracks(session, &unique).await?;
    Ok(unique.iter().filter_map(|u| map.remove(u)).collect())
}

fn gid_uri(kind: &str, gid: &[u8]) -> Option<String> {
    let id = librespot_core::SpotifyId::from_raw(gid).ok()?.to_base62().ok()?;
    Some(format!("spotify:{kind}:{id}"))
}

fn pick_image<'a>(group: &'a [metadata::Image], fallback: &'a [metadata::Image]) -> Option<String> {
    let imgs = if group.is_empty() { fallback } else { group };
    imgs.iter()
        .find(|i| i.size() == metadata::image::Size::LARGE)
        .or_else(|| imgs.iter().find(|i| i.size() == metadata::image::Size::DEFAULT))
        .or_else(|| imgs.first())
        .map(|i| image_url(i.file_id()))
}

/// An artist's popular tracks followed by tracks of their latest releases.
pub async fn artist_page(session: &Session, id: &str) -> Result<PageData> {
    let uri = format!("spotify:artist:{id}");
    let (_, bytes) = fetch_batch(session, std::slice::from_ref(&uri), ExtensionKind::ARTIST_V4)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Spotify returned nothing for this artist"))?;
    let artist = metadata::Artist::parse_from_bytes(&bytes).context("parsing artist")?;
    let country = session.country();
    let top = artist
        .top_track
        .iter()
        .find(|t| t.country() == country)
        .or_else(|| artist.top_track.first());
    let mut uris: Vec<String> = top
        .map(|t| t.track.iter().filter_map(|t| gid_uri("track", t.gid())).collect())
        .unwrap_or_default();

    // Then the most recent albums and singles (Spotify lists them newest first).
    let album_uris: Vec<String> = artist
        .album_group
        .iter()
        .take(6)
        .chain(artist.single_group.iter().take(6))
        .filter_map(|g| g.album.first())
        .filter_map(|a| gid_uri("album", a.gid()))
        .collect();
    if !album_uris.is_empty() {
        for (_, bytes) in fetch_batch(session, &album_uris, ExtensionKind::ALBUM_V4)
            .await
            .unwrap_or_default()
        {
            if let Ok(album) = metadata::Album::parse_from_bytes(&bytes) {
                uris.extend(
                    album
                        .disc
                        .iter()
                        .flat_map(|d| d.track.iter())
                        .filter_map(|t| gid_uri("track", t.gid())),
                );
            }
        }
    }
    uris.truncate(400);
    let tracks = ordered_tracks(session, &uris).await?;
    Ok(PageData {
        title: artist.name().to_string(),
        subtitle: format!("Artist · {} songs", tracks.len()),
        artist: String::new(),
        image: pick_image(&artist.portrait_group.image, &artist.portrait),
        tracks,
    })
}

pub async fn album_page(session: &Session, id: &str) -> Result<PageData> {
    let uri = format!("spotify:album:{id}");
    let (_, bytes) = fetch_batch(session, std::slice::from_ref(&uri), ExtensionKind::ALBUM_V4)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Spotify returned nothing for this album"))?;
    let album = metadata::Album::parse_from_bytes(&bytes).context("parsing album")?;
    let uris: Vec<String> = album
        .disc
        .iter()
        .flat_map(|d| d.track.iter())
        .filter_map(|t| gid_uri("track", t.gid()))
        .collect();
    let tracks = ordered_tracks(session, &uris).await?;
    let artists = album.artist.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ");
    Ok(PageData {
        title: album.name().to_string(),
        subtitle: format!("Album · {artists} · {} songs", tracks.len()),
        artist: artists,
        image: pick_image(&album.cover_group.image, &album.cover),
        tracks,
    })
}

pub async fn playlist_page(session: &Session, id: &str) -> Result<PageData> {
    let list = playlist(session, id).await?;
    let uris: Vec<String> = list.items.iter().map(|(u, _)| u.clone()).collect();
    let tracks = ordered_tracks(session, &uris).await?;
    let mut subtitle = format!("Playlist · {} songs", tracks.len());
    if !list.meta.owner.is_empty() {
        subtitle = format!("Playlist · {} · {} songs", list.meta.owner, tracks.len());
    }
    Ok(PageData {
        title: list.meta.name,
        subtitle,
        artist: String::new(),
        image: list.meta.art.or_else(|| tracks.iter().find_map(|t| t.art.clone())),
        tracks,
    })
}

pub async fn track_page(session: &Session, id: &str) -> Result<PageData> {
    let tracks = ordered_tracks(session, &[format!("spotify:track:{id}")]).await?;
    let t = tracks
        .first()
        .ok_or_else(|| anyhow!("Spotify returned nothing for this track"))?;
    Ok(PageData {
        title: t.title.clone(),
        subtitle: format!("Song · {}", t.artist),
        artist: String::new(),
        image: t.art.clone(),
        tracks,
    })
}

/// Everything about a track for tagging a downloaded copy: album, numbering, release date,
/// ISRC, label, copyright and the largest cover.
pub async fn track_details(session: &Session, uri: &str) -> Result<Metadata> {
    let (_, bytes) = fetch_batch(session, &[uri.to_string()], ExtensionKind::TRACK_V4)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Spotify returned nothing for this track"))?;
    let track = metadata::Track::parse_from_bytes(&bytes).context("parsing track")?;
    // The track only carries part of its album; the album itself has the dates and discs.
    let album = match track.album.as_ref().and_then(|a| gid_uri("album", a.gid())) {
        Some(album_uri) => fetch_batch(session, &[album_uri], ExtensionKind::ALBUM_V4)
            .await
            .ok()
            .and_then(|found| found.into_iter().next())
            .and_then(|(_, bytes)| metadata::Album::parse_from_bytes(&bytes).ok()),
        None => None,
    };
    Ok(details(uri, &track, album.as_ref()))
}

fn details(uri: &str, t: &metadata::Track, full_album: Option<&metadata::Album>) -> Metadata {
    let names = |artists: &[metadata::Artist]| {
        artists
            .iter()
            .map(|a| a.name())
            .filter(|n| !n.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let album = full_album.or(t.album.as_ref());
    let date = album
        .map(|a| match (a.date.year(), a.date.month(), a.date.day()) {
            (y, m, d) if y > 0 && m > 0 && d > 0 => format!("{y:04}-{m:02}-{d:02}"),
            (y, m, _) if y > 0 && m > 0 => format!("{y:04}-{m:02}"),
            (y, _, _) if y > 0 => format!("{y:04}"),
            _ => String::new(),
        })
        .unwrap_or_default();
    let number = |n: i32| (n > 0).then_some(n as u32);
    let discs = full_album.map(|a| a.disc.as_slice()).unwrap_or_default();
    let track_total = discs
        .iter()
        .find(|d| d.number() == t.disc_number().max(1))
        .map(|d| d.track.len() as u32)
        .filter(|n| *n > 0);
    let copyright = album.and_then(|a| {
        [metadata::copyright::Type::P, metadata::copyright::Type::C]
            .iter()
            .find_map(|kind| a.copyright.iter().find(|c| c.type_() == *kind))
            .map(|c| c.text().trim().to_string())
    });
    let covers = album.map(|a| {
        let mut images: Vec<&metadata::Image> = if a.cover_group.image.is_empty() {
            a.cover.iter().collect()
        } else {
            a.cover_group.image.iter().collect()
        };
        // XLARGE, LARGE, DEFAULT (~300 px), SMALL.
        images.sort_by_key(|i| match i.size() {
            metadata::image::Size::XLARGE => 0,
            metadata::image::Size::LARGE => 1,
            metadata::image::Size::DEFAULT => 2,
            metadata::image::Size::SMALL => 3,
        });
        images.iter().map(|i| image_url(i.file_id())).collect::<Vec<_>>()
    });
    let id = uri.rsplit(':').next().unwrap_or_default();
    Metadata {
        title: t.name().to_string(),
        artist: names(&t.artist),
        album: album.map(|a| a.name().to_string()).unwrap_or_default(),
        album_artist: album.map(|a| names(&a.artist)).unwrap_or_default(),
        date,
        track: number(t.number()),
        track_total,
        disc: number(t.disc_number()),
        disc_total: (discs.len() > 1).then_some(discs.len() as u32),
        isrc: t
            .external_id
            .iter()
            .find(|e| e.type_().eq_ignore_ascii_case("isrc"))
            .map(|e| e.id().to_string())
            .unwrap_or_default(),
        label: album.map(|a| a.label().trim().to_string()).unwrap_or_default(),
        copyright: copyright.unwrap_or_default(),
        url: format!("https://open.spotify.com/track/{id}"),
        cover_urls: covers.unwrap_or_default(),
        ..Metadata::default()
    }
}

/// The user's display name, falling back to the username.
pub async fn display_name(session: &Session) -> String {
    let username = session.username();
    match session.spclient().get_user_profile(&username, Some(0), Some(0)).await {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .filter(|n| !n.is_empty())
            .unwrap_or(username),
        Err(_) => username,
    }
}

fn convert_track(uri: &str, t: &metadata::Track) -> Option<Track> {
    if !uri.starts_with("spotify:track:") || t.name().is_empty() {
        return None;
    }
    let artist = t
        .artist
        .iter()
        .map(|a| a.name())
        .filter(|n| !n.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let album = t.album.as_ref();
    let images = album.map(|a| {
        let group: Vec<&metadata::Image> = a.cover_group.image.iter().collect();
        if group.is_empty() {
            a.cover.iter().collect()
        } else {
            group
        }
    });
    let art = images.and_then(|imgs| {
        // DEFAULT is ~300px: plenty for every place we draw covers.
        imgs.iter()
            .find(|i| i.size() == metadata::image::Size::DEFAULT)
            .or_else(|| imgs.iter().find(|i| i.size() == metadata::image::Size::LARGE))
            .or_else(|| imgs.first())
            .map(|i| image_url(i.file_id()))
    });
    Some(Track {
        album_artist: String::new(),
        disc_no: None,
        id: uri.to_string(),
        source: Source::Spotify,
        title: t.name().to_string(),
        artist,
        album: album.map(|a| a.name().to_string()).unwrap_or_default(),
        duration_ms: t.duration().max(0) as u64,
        track_no: (t.number() > 0).then_some(t.number() as u32),
        art,
        uri: uri.to_string(),
        added_at: 0,
    })
}

fn image_url(file_id: &[u8]) -> String {
    format!("https://i.scdn.co/image/{}", hex(file_id))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// collection2v2.proto isn't compiled into librespot-protocol, and the two messages we need
// are tiny, so they're encoded by hand.

#[derive(Debug, Default, PartialEq)]
struct CollectionItem {
    uri: String,
    added_at: i64,
    is_removed: bool,
}

#[derive(Debug, Default, PartialEq)]
struct PageResponse {
    items: Vec<CollectionItem>,
    next_page_token: String,
}

fn encode_page_request(username: &str, set: &str, token: &str, limit: i32) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut os = CodedOutputStream::vec(&mut buf);
        os.write_string(1, username)?;
        os.write_string(2, set)?;
        if !token.is_empty() {
            os.write_string(3, token)?;
        }
        os.write_int32(4, limit)?;
        os.flush()?;
    }
    Ok(buf)
}

fn decode_page_response(bytes: &[u8]) -> Result<PageResponse> {
    let mut page = PageResponse::default();
    let mut is = CodedInputStream::from_bytes(bytes);
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        let (field, wire) = (tag >> 3, tag & 7);
        match (field, wire) {
            (1, 2) => page.items.push(decode_item(&is.read_bytes()?)?),
            (2, 2) => page.next_page_token = is.read_string()?,
            _ => is.skip_field(protobuf::rt::WireType::new(wire).ok_or_else(|| anyhow!("bad wire type"))?)?,
        }
    }
    Ok(page)
}

fn decode_item(bytes: &[u8]) -> Result<CollectionItem> {
    let mut item = CollectionItem::default();
    let mut is = CodedInputStream::from_bytes(bytes);
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        let (field, wire) = (tag >> 3, tag & 7);
        match (field, wire) {
            (1, 2) => item.uri = is.read_string()?,
            (2, 0) => item.added_at = is.read_int32()? as i64,
            (3, 0) => item.is_removed = is.read_bool()?,
            _ => is.skip_field(protobuf::rt::WireType::new(wire).ok_or_else(|| anyhow!("bad wire type"))?)?,
        }
    }
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_uris() {
        assert_eq!(
            playlist_id_from_uri("spotify:playlist:37i9dQZF1DX"),
            Some("37i9dQZF1DX".into())
        );
        assert_eq!(
            playlist_id_from_uri("spotify:user:bob:playlist:abc"),
            Some("abc".into())
        );
        assert_eq!(playlist_id_from_uri("spotify:start-group:123:Folder"), None);
        assert_eq!(playlist_id_from_uri("spotify:end-group:123"), None);
    }

    #[test]
    fn collection_paging_roundtrip() {
        let req = encode_page_request("user", "collection", "tok", 300).unwrap();
        // field 1 "user", field 2 "collection", field 3 "tok", field 4 = 300
        assert_eq!(&req[..6], &[0x0a, 4, b'u', b's', b'e', b'r']);
        assert_eq!(&req[req.len() - 3..], &[0x20, 0xac, 0x02]);

        // Build a response: two items (one removed) and a next page token.
        let mut item1 = Vec::new();
        {
            let mut os = CodedOutputStream::vec(&mut item1);
            os.write_string(1, "spotify:track:aaa").unwrap();
            os.write_int32(2, 1_700_000_000).unwrap();
            os.flush().unwrap();
        }
        let mut item2 = Vec::new();
        {
            let mut os = CodedOutputStream::vec(&mut item2);
            os.write_string(1, "spotify:track:bbb").unwrap();
            os.write_bool(3, true).unwrap();
            os.write_string(4, "spotify:album:x").unwrap();
            os.flush().unwrap();
        }
        let mut resp = Vec::new();
        {
            let mut os = CodedOutputStream::vec(&mut resp);
            os.write_bytes(1, &item1).unwrap();
            os.write_bytes(1, &item2).unwrap();
            os.write_string(2, "next").unwrap();
            os.write_string(3, "sync").unwrap();
            os.flush().unwrap();
        }
        let page = decode_page_response(&resp).unwrap();
        assert_eq!(page.next_page_token, "next");
        assert_eq!(
            page.items,
            vec![
                CollectionItem {
                    uri: "spotify:track:aaa".into(),
                    added_at: 1_700_000_000,
                    is_removed: false
                },
                CollectionItem {
                    uri: "spotify:track:bbb".into(),
                    added_at: 0,
                    is_removed: true
                },
            ]
        );
    }

    #[test]
    fn converts_track_metadata() {
        let mut t = metadata::Track::new();
        t.set_name("Get Lucky".into());
        t.set_duration(248_000);
        t.set_number(8);
        let mut a1 = metadata::Artist::new();
        a1.set_name("Daft Punk".into());
        let mut a2 = metadata::Artist::new();
        a2.set_name("Pharrell Williams".into());
        t.artist = vec![a1, a2];
        let mut album = metadata::Album::new();
        album.set_name("Random Access Memories".into());
        let mut small = metadata::Image::new();
        small.set_file_id(vec![0xab, 0xcd]);
        small.set_size(metadata::image::Size::SMALL);
        let mut def = metadata::Image::new();
        def.set_file_id(vec![0x01, 0x02]);
        def.set_size(metadata::image::Size::DEFAULT);
        album.cover_group.mut_or_insert_default().image = vec![small, def];
        t.album = protobuf::MessageField::some(album);

        let out = convert_track("spotify:track:abc", &t).unwrap();
        assert_eq!(out.id, "spotify:track:abc");
        assert_eq!(out.artist, "Daft Punk, Pharrell Williams");
        assert_eq!(out.album, "Random Access Memories");
        assert_eq!(out.duration_ms, 248_000);
        assert_eq!(out.track_no, Some(8));
        assert_eq!(out.art.as_deref(), Some("https://i.scdn.co/image/0102"));
        assert!(convert_track("spotify:episode:x", &t).is_none());
    }

    #[test]
    fn track_details_for_tags() {
        let artist = |name: &str| {
            let mut a = metadata::Artist::new();
            a.set_name(name.into());
            a
        };
        let image = |id: u8, size| {
            let mut i = metadata::Image::new();
            i.set_file_id(vec![id]);
            i.set_size(size);
            i
        };
        let mut t = metadata::Track::new();
        t.set_name("Get Lucky".into());
        t.set_number(8);
        t.set_disc_number(1);
        t.artist = vec![artist("Daft Punk"), artist("Pharrell Williams")];
        let mut isrc = metadata::ExternalId::new();
        isrc.set_type("isrc".into());
        isrc.set_id("USQX91300108".into());
        t.external_id = vec![isrc];

        let mut album = metadata::Album::new();
        album.set_name("Random Access Memories".into());
        album.artist = vec![artist("Daft Punk")];
        album.set_label("Columbia".into());
        let date = album.date.mut_or_insert_default();
        date.set_year(2013);
        date.set_month(5);
        date.set_day(17);
        let mut disc = metadata::Disc::new();
        disc.set_number(1);
        disc.track = vec![metadata::Track::new(); 13];
        album.disc = vec![disc];
        let mut c = metadata::Copyright::new();
        c.set_type(metadata::copyright::Type::C);
        c.set_text("© 2013 Daft Life".into());
        let mut p = metadata::Copyright::new();
        p.set_type(metadata::copyright::Type::P);
        p.set_text("℗ 2013 Daft Life".into());
        album.copyright = vec![c, p];
        album.cover_group.mut_or_insert_default().image = vec![
            image(1, metadata::image::Size::DEFAULT),
            image(2, metadata::image::Size::LARGE),
            image(3, metadata::image::Size::SMALL),
        ];

        let m = details("spotify:track:69kOkLUCkxIZYexIgSG8rq", &t, Some(&album));
        assert_eq!(m.title, "Get Lucky");
        assert_eq!(m.artist, "Daft Punk, Pharrell Williams");
        assert_eq!(
            (m.album.as_str(), m.album_artist.as_str()),
            ("Random Access Memories", "Daft Punk")
        );
        assert_eq!(m.date, "2013-05-17");
        assert_eq!(
            (m.track, m.track_total, m.disc, m.disc_total),
            (Some(8), Some(13), Some(1), None)
        );
        assert_eq!(m.isrc, "USQX91300108");
        assert_eq!(m.label, "Columbia");
        assert_eq!(m.copyright, "℗ 2013 Daft Life");
        assert_eq!(m.url, "https://open.spotify.com/track/69kOkLUCkxIZYexIgSG8rq");
        assert_eq!(
            m.cover_urls,
            vec![
                "https://i.scdn.co/image/02".to_string(),
                "https://i.scdn.co/image/01".to_string(),
                "https://i.scdn.co/image/03".to_string()
            ]
        );

        // Without the full album: what the track itself carries.
        let m = details("spotify:track:x", &t, None);
        assert_eq!((m.album.as_str(), m.date.as_str(), m.track_total), ("", "", None));
    }
}
