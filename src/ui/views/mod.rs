//! Main content views.

use egui::{vec2, Align, Align2, Color32, CornerRadius, CursorIcon, Id, Layout, Margin, Pos2, Rect, Sense, Ui};
use egui_phosphor::regular as icon;

mod downloads;
mod home;
mod library;
mod now_playing;
mod pages;

use self::{downloads::*, home::*, library::*, now_playing::*, pages::*};

use super::panels;
use super::theme::{self, *};
use super::widgets::{self, text_trunc, TableOpts};
use super::{Action, Cx, View};
use crate::library::Album;
use crate::library::Library;
use crate::links;
use crate::model::{ArtistHit, CollectionHit, Playlist, PlaylistKind, Source, Track};
use crate::service::{Command, DownloadState, PlayStatus};

pub struct ViewState<'a> {
    pub view: &'a View,
    /// Where downloads are saved: the chosen folder, or the library folder holding the
    /// per-service folders.
    pub download_dir: &'a std::path::Path,
    pub download_custom: bool,
    /// The yt-dlp program, when YouTube downloads are on.
    pub ytdlp: Option<&'a str>,
    /// What's typed in the top bar.
    pub search_text: &'a str,
    pub search_cache: &'a mut (String, u64, Vec<String>),
    pub filter_text: &'a mut String,
}

pub fn show(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    match st.view.clone() {
        View::Home => home(ui, cx),
        View::Search => search(ui, cx, st),
        View::Songs => songs(ui, cx, st),
        View::Albums => albums(ui, cx, st),
        View::Artists => artists(ui, cx, st),
        View::Album(key) => album(ui, cx, st, &key),
        View::Playlist(id) => playlist(ui, cx, st, &id),
        View::Artist(key) => library_artist(ui, cx, st, &key),
        View::Page(key) => remote_page(ui, cx, st, &key),
        View::NowPlaying => now_playing(ui, cx),
        View::Downloads => downloads(ui, cx, st),
        View::Settings | View::Profile => {}
    }
}

/// Scrollable page below the toolbar.
pub(super) fn page(ui: &mut Ui, id: &str, content: impl FnOnce(&mut Ui, Rect, f32)) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let origin = ui.max_rect().top();
            egui::Frame::new()
                .inner_margin(Margin {
                    left: 20,
                    right: 20,
                    top: 4,
                    bottom: 28,
                })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    content(ui, viewport, origin)
                });
        });
}

fn rounded_top_gradient(ui: &Ui, rect: Rect, top: Color32, bottom: Color32) {
    let r = RADIUS as f32;
    ui.painter().rect_filled(
        Rect::from_min_size(rect.min, vec2(rect.width(), r)),
        CornerRadius {
            nw: RADIUS,
            ne: RADIUS,
            sw: 0,
            se: 0,
        },
        top,
    );
    widgets::gradient(ui, Rect::from_min_max(rect.min + vec2(0.0, r), rect.max), top, bottom);
}

/// Heading with a "Show all" link on the right. Returns true when the link was clicked.
fn heading_with_link(ui: &mut Ui, text: &str, link: &str) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        widgets::heading(ui, text);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            clicked = ui
                .add(
                    egui::Label::new(egui::RichText::new(link).font(theme::bold_font(14.0)).color(TEXT_DIM))
                        .sense(Sense::click()),
                )
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked();
        });
    });
    clicked
}

// ------------------------------------------------------------------ shared by the pages

fn tiles_per_row(ui: &Ui, min_w: f32) -> usize {
    (((ui.available_width() + 20.0) / (min_w + 20.0)).floor() as usize).max(1)
}

/// An album in the library: one made of local files, or one saved from Spotify or SoundCloud.
#[derive(Clone, Copy)]
enum LibAlbum<'a> {
    Local(&'a Album),
    Saved(&'a Playlist),
}

impl<'a> LibAlbum<'a> {
    fn name(self) -> &'a str {
        match self {
            LibAlbum::Local(a) => &a.name,
            LibAlbum::Saved(p) => &p.name,
        }
    }

    fn artist(self) -> &'a str {
        match self {
            LibAlbum::Local(a) => &a.artist,
            LibAlbum::Saved(p) => &p.description,
        }
    }

    fn art(self) -> Option<&'a str> {
        match self {
            LibAlbum::Local(a) => a.art.as_deref(),
            LibAlbum::Saved(p) => p.art.as_deref(),
        }
    }

    fn source(self) -> Source {
        match self {
            LibAlbum::Local(_) => Source::Local,
            LibAlbum::Saved(p) => p.kind.source().unwrap_or(Source::Local),
        }
    }

    fn track_ids(self) -> &'a [String] {
        match self {
            LibAlbum::Local(a) => &a.track_ids,
            LibAlbum::Saved(p) => &p.track_ids,
        }
    }

    /// When its newest song came into the library.
    fn added(self, lib: &Library) -> i64 {
        match self {
            LibAlbum::Local(a) => a.added,
            LibAlbum::Saved(p) => p
                .track_ids
                .iter()
                .filter_map(|id| lib.get(id))
                .map(|t| t.added_at)
                .max()
                .unwrap_or(0),
        }
    }
}

/// Every album in the library by artist, then title; `only` keeps the ones from one source.
fn library_albums(lib: &Library, only: Option<Source>) -> Vec<LibAlbum<'_>> {
    let mut all: Vec<LibAlbum> = lib
        .albums
        .iter()
        .map(LibAlbum::Local)
        .chain(lib.playlists.iter().filter(|p| p.kind.is_album()).map(LibAlbum::Saved))
        .filter(|a| only.is_none_or(|s| a.source() == s))
        .collect();
    all.sort_by_cached_key(|a| (a.artist().to_lowercase(), a.name().to_lowercase()));
    all
}

fn album_grid(ui: &mut Ui, cx: &mut Cx, albums: &[LibAlbum]) {
    widgets::grid(ui, albums.len(), 168.0, |ui, i, w| {
        let a = albums[i];
        let source = a.source();
        let (resp, play) = widgets::tile(ui, w, a.name(), a.artist(), cx.accent, |ui, r| {
            widgets::cover(
                ui,
                cx.art,
                a.art(),
                r,
                6,
                (theme::mix(source_color(source), PANEL, 0.4), icon::VINYL_RECORD),
            );
            if source != Source::Local {
                source_corner(ui, r, source);
            }
        });
        if play {
            cx.actions.push(Action::Cmd(Command::Play {
                tracks: cx.lib.tracks_for(a.track_ids()),
                start: widgets::first_song(cx, a.track_ids().len()),
                context: a.name().to_string(),
            }));
        } else if resp.clicked() {
            cx.actions.push(Action::Go(match a {
                LibAlbum::Local(album) => View::Album(album.key.clone()),
                LibAlbum::Saved(p) => View::Playlist(p.id.clone()),
            }));
        }
        resp.context_menu(|ui| {
            let go = (!a.artist().is_empty()).then(|| widgets::artist_action(cx.lib, a.artist()));
            widgets::songs_menu(ui, cx, a.track_ids(), a.name(), go);
        });
    });
}

/// The saved album a page (by its key) is, if it's saved.
fn saved_album<'a>(lib: &'a Library, key: &str, source: Option<Source>) -> Option<&'a Playlist> {
    let id = match (source, crate::links::target(key)) {
        (Some(Source::Spotify), Some(crate::links::Target::Spotify(crate::links::LinkKind::Album, id))) => {
            crate::service::saved_album_id(PlaylistKind::SpotifyAlbum, &id)
        }
        (Some(Source::SoundCloud), _) => crate::service::saved_album_id(PlaylistKind::SoundCloudAlbum, key),
        _ => return None,
    };
    lib.playlist(&id)
}

/// A row of albums or playlists from search (Spotify's and SoundCloud's taking turns).
fn collection_row(ui: &mut Ui, cx: &mut Cx, title: &str, hits: &[CollectionHit]) {
    if hits.is_empty() {
        return;
    }
    let per_row = tiles_per_row(ui, 150.0);
    let more_id = Id::new(("search-show-all", title));
    let all = ui.data(|d| d.get_temp::<bool>(more_id)).unwrap_or(false);
    if hits.len() > per_row {
        let link = if all { "Show less" } else { "Show all" };
        if heading_with_link(ui, title, link) {
            ui.data_mut(|d| d.insert_temp(more_id, !all));
        }
    } else {
        widgets::heading(ui, title);
    }
    let n = if all { hits.len() } else { hits.len().min(per_row) };
    widgets::grid(ui, n, 150.0, |ui, i, w| {
        let h = &hits[i];
        let glyph = if h.album { icon::VINYL_RECORD } else { icon::PLAYLIST };
        let (resp, play) = widgets::tile(ui, w, &h.title, &h.subtitle(), cx.accent, |ui, r| {
            widgets::cover(
                ui,
                cx.art,
                h.image.as_deref(),
                r,
                6,
                (theme::mix(source_color(h.source), PANEL, 0.4), glyph),
            );
            source_corner(ui, r, h.source);
        });
        // Its songs come with its page.
        if play || resp.clicked() {
            cx.actions.push(Action::Open(h.key.clone()));
        }
        resp.context_menu(|ui| widgets::page_menu(ui, cx, &h.key));
    });
    ui.add_space(8.0);
}

fn play_playlist(cx: &mut Cx, p: &Playlist, shuffle: bool) {
    let tracks = cx.lib.tracks_for(&p.track_ids);
    let refs: Vec<&Track> = tracks.iter().collect();
    play_list(cx, &refs, shuffle, &p.name);
}

fn play_list(cx: &mut Cx, tracks: &[&Track], shuffle: bool, context: &str) {
    if tracks.is_empty() {
        return;
    }
    // The Shuffle button turns shuffle on; Play keeps it the way the player has it.
    if shuffle && !cx.player.shuffle {
        cx.actions.push(Action::Cmd(Command::SetShuffle(true)));
    }
    let start = if shuffle {
        rand::random_range(0..tracks.len())
    } else {
        widgets::first_song(cx, tracks.len())
    };
    cx.actions.push(Action::Cmd(Command::Play {
        tracks: tracks.iter().map(|t| (*t).clone()).collect(),
        start,
        context: context.to_string(),
    }));
}

// ------------------------------------------------------------------ list pages

/// Case-insensitive filter over title/artist/album.
/// When the list's filter leaves nothing, says so (the list isn't empty) with a way back.
/// True when it did.
fn no_matches(ui: &mut Ui, shown: usize, filter_text: &mut String) -> bool {
    if shown > 0 || filter_text.trim().is_empty() {
        return false;
    }
    panels::empty_state(
        ui,
        icon::MAGNIFYING_GLASS,
        &format!("No songs match “{}”", filter_text.trim()),
    );
    ui.vertical_centered(|ui| {
        if ui.button("Clear filter").clicked() {
            filter_text.clear();
        }
    });
    true
}

fn filter<'a>(tracks: Vec<&'a Track>, needle: &str) -> Vec<&'a Track> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return tracks;
    }
    let terms: Vec<&str> = needle.split_whitespace().collect();
    tracks
        .into_iter()
        .filter(|t| {
            let hay = format!("{} {} {}", t.title, t.artist, t.album).to_lowercase();
            terms.iter().all(|term| hay.contains(term))
        })
        .collect()
}

struct Header<'a> {
    /// "Playlist", "Album", "Artist"…
    kind: &'a str,
    /// Playback started from this list (see [`widgets::plays_from`]).
    playing_from: bool,
    title: &'a str,
    description: &'a str,
    meta: String,
    source: Option<Source>,
    /// Colour of the header card (usually from the cover).
    tint: Color32,
    /// Round artwork (artists).
    round: bool,
}

/// The largest title size (Spotify-style 64 → 24 px) that fits on one line.
pub(super) fn title_size(ui: &Ui, title: &str, width: f32) -> f32 {
    for size in [34.0, 28.0, 24.0] {
        let galley = ui
            .painter()
            .layout_no_wrap(title.to_string(), theme::bold_font(size), TEXT);
        if galley.size().x <= width {
            return size;
        }
    }
    20.0
}

/// Header card (cover glowing on a tinted tile, titles) followed by the action row: Play,
/// Shuffle, then `extra` buttons, with a filter field on the right. Returns (play, shuffle).
fn list_header(
    ui: &mut Ui,
    cx: &mut Cx,
    st: &mut ViewState,
    h: &Header,
    draw_art: impl FnOnce(&Ui, Rect, &mut Cx),
    extra: impl FnOnce(&mut Ui, &mut Cx),
) -> (bool, bool) {
    let w = ui.available_width();
    let art_size = if w > 700.0 { 184.0 } else { 128.0 };
    let pad = 24.0;
    let (card, _) = ui.allocate_exact_size(vec2(w, art_size + 2.0 * pad), Sense::hover());
    ui.painter()
        .rect_filled(card, CornerRadius::same(RADIUS + 4), theme::mix(h.tint, CARD, 0.8));
    let art = Rect::from_min_size(card.min + vec2(pad, pad), vec2(art_size, art_size));
    let art_radius = if h.round { (art_size / 2.0) as u8 } else { 12 };
    // The cover glows in its own colour.
    ui.painter().add(
        egui::epaint::Shadow {
            offset: [0, 6],
            blur: 40,
            spread: 0,
            color: theme::with_alpha(h.tint, 90),
        }
        .as_shape(art, CornerRadius::same(art_radius)),
    );
    draw_art(ui, art, cx);

    let tx = art.right() + 28.0;
    let tw = (card.right() - pad - tx).max(40.0);
    let size = title_size(ui, h.title, tw);
    let desc_h = if h.description.is_empty() { 0.0 } else { 24.0 };
    let block = 18.0 + 8.0 + size * 1.2 + desc_h + 8.0 + 18.0;
    let mut y = card.center().y - block / 2.0;
    let mut kind_x = tx;
    let kind = h.kind.to_uppercase();
    let used = text_trunc(ui, Pos2::new(kind_x, y), &kind, theme::bold_font(11.5), TEXT_DIM, tw);
    kind_x = used.right() + 10.0;
    if let Some(src) = h.source {
        theme::paint_icon(
            ui.painter(),
            Pos2::new(kind_x + 6.0, used.center().y),
            theme::source_icon(src),
            theme::icon_font(13.0),
            source_color(src),
        );
        text_trunc(
            ui,
            Pos2::new(kind_x + 16.0, y),
            src.label(),
            theme::bold_font(11.5),
            source_color(src),
            tw - (kind_x + 16.0 - tx),
        );
    }
    y += 18.0 + 8.0;
    text_trunc(ui, Pos2::new(tx, y), h.title, theme::bold_font(size), TEXT, tw);
    y += size * 1.2;
    if !h.description.is_empty() {
        text_trunc(
            ui,
            Pos2::new(tx, y + 2.0),
            h.description,
            theme::font(13.5),
            TEXT_DIM,
            tw,
        );
        y += desc_h;
    }
    text_trunc(ui, Pos2::new(tx, y + 8.0), &h.meta, theme::font(13.5), TEXT, tw);

    ui.add_space(16.0);
    let mut play = false;
    let mut shuffle = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let here = h.playing_from;
        let playing_here = here && cx.player.status == PlayStatus::Playing;
        let (glyph, label) = if playing_here {
            (egui_phosphor::fill::PAUSE, "Pause")
        } else {
            (egui_phosphor::fill::PLAY, "Play")
        };
        if widgets::action_button(ui, glyph, label, true, cx.accent).clicked() {
            if playing_here {
                cx.actions.push(Action::Cmd(Command::Pause));
            } else if here && cx.player.status == PlayStatus::Paused {
                cx.actions.push(Action::Cmd(Command::Resume));
            } else {
                play = true;
            }
        }
        if widgets::action_button(ui, icon::SHUFFLE, "Shuffle", false, cx.accent).clicked() {
            shuffle = true;
        }
        ui.add_space(4.0);
        extra(ui, cx);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(
                egui::TextEdit::singleline(st.filter_text)
                    .hint_text(theme::ic(icon::MAGNIFYING_GLASS, "Filter"))
                    .desired_width(170.0),
            );
        });
    });
    ui.add_space(12.0);
    (play, shuffle)
}

/// The Spotify or SoundCloud artist named exactly like `query`, once its results are in
/// (Enter in the search box opens it).
pub fn top_artist(feed: &crate::service::Feed, query: &str) -> Option<ArtistHit> {
    let squash = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    };
    let want = squash(query);
    if want.is_empty() || feed.search.query != query.trim() {
        return None;
    }
    feed.search
        .artists
        .iter()
        .find(|a| a.source != Source::Local && squash(&a.name) == want)
        .cloned()
}
