//! Main content views.

use egui::{vec2, Align, Align2, Color32, CornerRadius, CursorIcon, Id, Layout, Margin, Pos2, Rect, Sense, Ui};
use egui_phosphor::regular as icon;

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

// ------------------------------------------------------------------ home

fn home(ui: &mut Ui, cx: &mut Cx) {
    page(ui, "home", |ui, _viewport, _origin| {
        ui.label(egui::RichText::new("Home").font(theme::bold_font(26.0)));
        ui.add_space(12.0);

        let playlists: Vec<&Playlist> = cx
            .lib
            .playlists
            .iter()
            .filter(|p| !p.track_ids.is_empty() && !p.kind.is_album())
            .collect();
        let saved_albums = cx.lib.playlists.iter().any(|p| p.kind.is_album());
        if playlists.is_empty() && cx.lib.local.is_empty() && !saved_albums {
            onboarding(ui, cx);
            super::profile::home_card(ui, cx);
            return;
        }

        // Shortcut cards.
        let shortcuts: Vec<&Playlist> = playlists.iter().take(8).copied().collect();
        if !shortcuts.is_empty() {
            let gap = 8.0;
            let cols = if ui.available_width() > 900.0 { 4 } else { 2 };
            let w = ((ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32).floor();
            for row in shortcuts.chunks(cols) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for p in row {
                        shortcut_card(ui, cx, p, w);
                    }
                });
                ui.add_space(gap);
            }
            ui.add_space(24.0);
        }

        // Recently played.
        let recent: Vec<&Track> = cx.lib.recent.iter().filter_map(|id| cx.lib.get(id)).take(12).collect();
        if !recent.is_empty() {
            widgets::heading(ui, "Recently played");
            let tracks: Vec<Track> = recent.iter().map(|t| (*t).clone()).collect();
            let n = recent.len().min(tiles_per_row(ui, 160.0));
            widgets::grid(ui, n, 160.0, |ui, i, w| {
                let t = recent[i];
                let (resp, play) = widgets::tile(ui, w, &t.title, &t.artist, cx.accent, |ui, r| {
                    widgets::cover(ui, cx.art, t.art.as_deref(), r, 6, widgets::track_fallback(t))
                });
                if play || resp.double_clicked() {
                    cx.actions.push(Action::Cmd(Command::Play {
                        tracks: tracks.clone(),
                        start: i,
                        context: "Recently played".into(),
                    }));
                }
                let track = t.clone();
                resp.context_menu(|ui| widgets::track_menu(ui, cx, &track, None));
            });
            ui.add_space(10.0);
        }

        // Your top artists (by songs in the library).
        let artists: Vec<&crate::library::Artist> = cx
            .lib
            .artists
            .iter()
            .filter(|a| a.track_ids.len() >= 2)
            .take(tiles_per_row(ui, 160.0))
            .collect();
        if !artists.is_empty() {
            widgets::heading(ui, "Your artists");
            widgets::grid(ui, artists.len(), 160.0, |ui, i, w| {
                let a = artists[i];
                let (resp, play) = widgets::tile(ui, w, &a.name, "Artist", cx.accent, |ui, r| {
                    widgets::cover_round(ui, cx.art, a.art.as_deref(), r, widgets::artist_fallback())
                });
                if play {
                    cx.actions.push(Action::Cmd(Command::Play {
                        tracks: cx.lib.tracks_for(&a.track_ids),
                        start: widgets::first_song(cx, a.track_ids.len()),
                        context: a.name.clone(),
                    }));
                } else if resp.clicked() {
                    cx.actions.push(Action::Open(format!("local:artist:{}", a.key)));
                }
            });
            ui.add_space(10.0);
        }

        if !playlists.is_empty() {
            widgets::heading(ui, "Your playlists");
            widgets::grid(ui, playlists.len(), 168.0, |ui, i, w| {
                let p = playlists[i];
                let sub = match p.kind.source() {
                    Some(s) => format!("{} · {} songs", s.label(), p.track_ids.len()),
                    None => format!("{} songs", p.track_ids.len()),
                };
                let (resp, play) = widgets::tile(ui, w, &p.name, &sub, cx.accent, |ui, r| {
                    widgets::playlist_cover(ui, cx.art, p, r, 6)
                });
                if play {
                    play_playlist(cx, p, false);
                } else if resp.clicked() {
                    cx.actions.push(Action::Go(View::Playlist(p.id.clone())));
                }
            });
        }

        let albums = library_albums(cx.lib, None);
        if !albums.is_empty() {
            ui.add_space(10.0);
            if heading_with_link(ui, "Albums in your library", "Show all") {
                cx.actions.push(Action::Go(View::Albums));
            }
            let n = albums.len().min(tiles_per_row(ui, 168.0) * 2);
            album_grid(ui, cx, &albums[..n]);
        }

        // You on Last.fm, near the bottom.
        super::profile::home_card(ui, cx);
    });
}

fn tiles_per_row(ui: &Ui, min_w: f32) -> usize {
    (((ui.available_width() + 20.0) / (min_w + 20.0)).floor() as usize).max(1)
}

fn shortcut_card(ui: &mut Ui, cx: &mut Cx, p: &Playlist, w: f32) {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 64.0), Sense::click());
    ui.painter().rect_filled(rect, CornerRadius::same(RADIUS), CARD);
    widgets::fade_fill(ui, resp.id, rect, RADIUS, resp.hovered(), HOVER);
    let art = Rect::from_min_size(rect.min + vec2(6.0, 6.0), vec2(52.0, 52.0));
    widgets::playlist_cover(ui, cx.art, p, art, 10);
    let here = widgets::plays_from(cx, &p.name, |id| p.track_ids.iter().any(|t| t == id));
    let playing = here && cx.player.status == PlayStatus::Playing;
    text_trunc(
        ui,
        Pos2::new(art.right() + 12.0, rect.center().y - 9.0),
        &p.name,
        theme::bold_font(14.0),
        TEXT,
        rect.right() - art.right() - 64.0,
    );
    if resp.hovered() || playing {
        let btn = Rect::from_center_size(Pos2::new(rect.right() - 26.0, rect.center().y), vec2(34.0, 34.0));
        let over = ui.rect_contains_pointer(btn);
        let fill = if over {
            theme::mix(cx.accent, Color32::WHITE, 0.15)
        } else {
            cx.accent
        };
        ui.painter().rect_filled(btn, CornerRadius::same(10), fill);
        let glyph = if playing {
            egui_phosphor::fill::PAUSE
        } else {
            egui_phosphor::fill::PLAY
        };
        theme::paint_icon(
            ui.painter(),
            btn.center() + vec2(if playing { 0.0 } else { 1.0 }, 0.0),
            glyph,
            theme::fill_icon_font(15.0),
            theme::on_color(fill),
        );
        if over && resp.clicked() {
            if playing {
                cx.actions.push(Action::Cmd(Command::Pause));
            } else if here {
                cx.actions.push(Action::Cmd(Command::Resume));
            } else {
                play_playlist(cx, p, false);
            }
            return;
        }
    }
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        cx.actions.push(Action::Go(View::Playlist(p.id.clone())));
    }
}

fn onboarding(ui: &mut Ui, cx: &mut Cx) {
    widgets::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width().min(640.0));
        ui.label(egui::RichText::new("Let's bring in your music").font(theme::bold_font(20.0)));
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Sumo plays your local files, Spotify (Premium) and SoundCloud in one place, \
                 with synced lyrics, Discord status and Last.fm scrobbling.",
            )
            .color(TEXT_DIM),
        );
        ui.add_space(12.0);
        for (glyph, color, text) in [
            (
                icon::FOLDER,
                source_color(Source::Local),
                "Add your music folders (or drop a folder onto this window)",
            ),
            (
                icon::SPOTIFY_LOGO,
                source_color(Source::Spotify),
                "Log in to Spotify to import playlists and Liked Songs",
            ),
            (
                icon::SOUNDCLOUD_LOGO,
                source_color(Source::SoundCloud),
                "Add your SoundCloud profile to import likes and playlists",
            ),
            (
                icon::APPLE_LOGO,
                source_color(Source::AppleMusic),
                "Import your Apple Music library (drop Library.xml here)",
            ),
            (
                icon::LINK,
                TEXT_DIM,
                "Or paste any Spotify, SoundCloud or Apple Music link into the search bar",
            ),
        ] {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(glyph)
                        .family(theme::icons())
                        .size(18.0)
                        .color(color),
                );
                ui.label(text);
            });
        }
        ui.add_space(12.0);
        if widgets::pill(ui, "Open Settings", cx.accent, theme::on_color(cx.accent)).clicked() {
            cx.actions.push(Action::Go(View::Settings));
        }
    });
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

fn songs(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    let tint = source_color(Source::Local);
    page(ui, "songs", |ui, viewport, origin| {
        let all: Vec<&Track> = cx.lib.local.iter().filter_map(|id| cx.lib.get(id)).collect();
        let total_ms: u64 = all.iter().map(|t| t.duration_ms).sum();
        let header = Header {
            kind: "Folder",
            playing_from: widgets::plays_from(cx, "Local Files", |id| all.iter().any(|t| t.id == id)),
            title: "Local Files",
            description: "Every song in your music folders",
            meta: format!("{} songs, {}", all.len(), theme::fmt_total(total_ms)),
            source: None,
            tint,
            round: false,
        };
        let (play, shuffle) = list_header(
            ui,
            cx,
            st,
            &header,
            |ui, r, _| widgets::placeholder(ui, r, theme::mix(tint, PANEL, 0.3), icon::FOLDER, 12),
            |ui, cx| {
                if widgets::icon_button(ui, icon::ARROWS_CLOCKWISE, 22.0, TEXT_DIM, "Rescan folders").clicked() {
                    cx.actions.push(Action::Cmd(Command::Rescan));
                }
            },
        );
        let tracks = filter(all, st.filter_text);
        if play || shuffle {
            play_list(cx, &tracks, shuffle, "Local Files");
        }
        if no_matches(ui, tracks.len(), st.filter_text) {
            return;
        }
        if tracks.is_empty() {
            panels::empty_state(ui, icon::FOLDER, "No local songs yet — add a music folder in Settings");
            return;
        }
        let opts = TableOpts {
            id: "songs",
            context: "Local Files",
            playlist: None,
            show_album: true,
            show_header: true,
        };
        widgets::track_table(ui, cx, &tracks, &opts, viewport, origin);
    });
}

fn albums(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    page(ui, "albums", |ui, _viewport, _origin| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Albums").font(theme::bold_font(26.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(st.filter_text)
                        .hint_text(theme::ic(icon::MAGNIFYING_GLASS, "Filter"))
                        .desired_width(200.0),
                );
            });
        });
        ui.add_space(12.0);
        // Tabs for where the albums come from, when there's more than one place.
        let everything = library_albums(cx.lib, None);
        let tab_id = Id::new("albums-source");
        let mut only: Option<Source> = ui.data(|d| d.get_temp(tab_id)).unwrap_or(None);
        let tabs: Vec<(Option<Source>, &str)> = [
            (None, "All"),
            (Some(Source::Local), "Local files"),
            (Some(Source::Spotify), "Spotify"),
            (Some(Source::SoundCloud), "SoundCloud"),
        ]
        .into_iter()
        .filter(|(s, _)| s.is_none_or(|s| everything.iter().any(|a| a.source() == s)))
        .collect();
        if tabs.len() > 2 {
            if let Some(pick) = widgets::segmented(ui, &tabs, only) {
                only = pick;
                ui.data_mut(|d| d.insert_temp(tab_id, only));
            }
            ui.add_space(14.0);
        } else {
            only = None;
        }
        let needle = st.filter_text.trim().to_lowercase();
        let list: Vec<LibAlbum> = everything
            .into_iter()
            .filter(|a| only.is_none_or(|s| a.source() == s))
            .filter(|a| {
                needle.is_empty()
                    || a.name().to_lowercase().contains(&needle)
                    || a.artist().to_lowercase().contains(&needle)
            })
            .collect();
        if list.is_empty() {
            panels::empty_state(
                ui,
                icon::VINYL_RECORD,
                "No albums yet. Albums from your music folders show up here, and so do albums you save \
                 from Spotify or SoundCloud (search for one, open it and click Save album).",
            );
            return;
        }
        album_grid(ui, cx, &list);
    });
}

fn artists(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    page(ui, "artists", |ui, _viewport, _origin| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Artists").font(theme::bold_font(26.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(st.filter_text)
                        .hint_text(theme::ic(icon::MAGNIFYING_GLASS, "Filter"))
                        .desired_width(200.0),
                );
            });
        });
        ui.add_space(16.0);
        let needle = st.filter_text.trim().to_lowercase();
        let list: Vec<&crate::library::Artist> = cx
            .lib
            .artists
            .iter()
            .filter(|a| needle.is_empty() || a.key.contains(&needle))
            .collect();
        if list.is_empty() {
            panels::empty_state(
                ui,
                icon::USERS_THREE,
                "Artists of your songs and playlists show up here",
            );
            return;
        }
        widgets::grid(ui, list.len(), 150.0, |ui, i, w| {
            let a = list[i];
            let sub = format!("{} songs", a.track_ids.len());
            let (resp, play) = widgets::tile(ui, w, &a.name, &sub, cx.accent, |ui, r| {
                widgets::cover_round(ui, cx.art, a.art.as_deref(), r, widgets::artist_fallback())
            });
            if play {
                cx.actions.push(Action::Cmd(Command::Play {
                    tracks: cx.lib.tracks_for(&a.track_ids),
                    start: widgets::first_song(cx, a.track_ids.len()),
                    context: a.name.clone(),
                }));
            } else if resp.clicked() {
                cx.actions.push(Action::Open(format!("local:artist:{}", a.key)));
            }
        });
    });
}

fn album(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState, key: &str) {
    let Some(a) = cx.lib.albums.iter().find(|a| a.key == key) else {
        panels::empty_state(ui, icon::VINYL_RECORD, "Album not found");
        return;
    };
    let tint = a.art.as_deref().and_then(|src| cx.art.accent(src)).unwrap_or(cx.tint);
    page(ui, "album", |ui, viewport, origin| {
        let all: Vec<&Track> = a.track_ids.iter().filter_map(|id| cx.lib.get(id)).collect();
        let total_ms: u64 = all.iter().map(|t| t.duration_ms).sum();
        let header = Header {
            kind: "Album",
            playing_from: widgets::plays_from(cx, &a.name, |id| all.iter().any(|t| t.id == id)),
            title: &a.name,
            description: "",
            meta: format!("{} • {} songs, {}", a.artist, all.len(), theme::fmt_total(total_ms)),
            source: None,
            tint,
            round: false,
        };
        let art = a.art.clone();
        let artist = a.artist.clone();
        let (play, shuffle) = list_header(
            ui,
            cx,
            st,
            &header,
            |ui, r, cx| {
                widgets::cover(
                    ui,
                    cx.art,
                    art.as_deref(),
                    r,
                    12,
                    (theme::mix(source_color(Source::Local), PANEL, 0.4), icon::VINYL_RECORD),
                )
            },
            |ui, cx| {
                if widgets::icon_button(ui, icon::USER, 22.0, TEXT_DIM, &format!("Go to {artist}")).clicked() {
                    cx.actions.push(widgets::artist_action(cx.lib, &artist));
                }
            },
        );
        let tracks = filter(all, st.filter_text);
        if play || shuffle {
            play_list(cx, &tracks, shuffle, &a.name);
        }
        if no_matches(ui, tracks.len(), st.filter_text) {
            return;
        }
        let opts = TableOpts {
            id: "album",
            context: &a.name,
            playlist: None,
            show_album: false,
            show_header: true,
        };
        widgets::track_table(ui, cx, &tracks, &opts, viewport, origin);
    });
}

fn playlist(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState, id: &str) {
    let Some(p) = cx.lib.playlist(id) else {
        panels::empty_state(ui, icon::PLAYLIST, "Playlist not found");
        return;
    };
    let tint = if matches!(
        p.kind,
        PlaylistKind::Liked | PlaylistKind::SpotifyLiked | PlaylistKind::SoundCloudLikes
    ) {
        widgets::playlist_fallback(p).0
    } else {
        p.art.as_deref().and_then(|a| cx.art.accent(a)).unwrap_or(cx.tint)
    };
    page(ui, "playlist", |ui, viewport, origin| {
        let all: Vec<&Track> = p.track_ids.iter().filter_map(|id| cx.lib.get(id)).collect();
        let total_ms: u64 = all.iter().map(|t| t.duration_ms).sum();
        let header = Header {
            kind: if p.kind.is_album() { "Album" } else { "Playlist" },
            playing_from: widgets::plays_from(cx, &p.name, |id| all.iter().any(|t| t.id == id)),
            title: &p.name,
            description: &p.description,
            meta: format!("{} songs, {}", all.len(), theme::fmt_total(total_ms)),
            source: if p.kind == PlaylistKind::M3u {
                Some(Source::Local)
            } else {
                p.kind.source()
            },
            tint,
            round: false,
        };
        let (play, shuffle) = list_header(
            ui,
            cx,
            st,
            &header,
            |ui, r, cx| widgets::playlist_cover(ui, cx.art, p, r, 12),
            |ui, cx| {
                playlist_actions(ui, cx, p);
                widgets::download_button(ui, cx, &all, 22.0);
            },
        );
        let tracks = filter(all, st.filter_text);
        if play || shuffle {
            play_list(cx, &tracks, shuffle, &p.name);
        }
        widgets::paste_shortcut(ui, cx, p);
        if p.kind == PlaylistKind::AppleMusic {
            ui.label(
                egui::RichText::new("Apple Music songs play from your local files, Spotify or SoundCloud")
                    .small()
                    .color(TEXT_FAINT),
            );
            ui.add_space(4.0);
        }
        if no_matches(ui, tracks.len(), st.filter_text) {
            return;
        }
        if tracks.is_empty() {
            let msg = if p.kind == PlaylistKind::Liked {
                "Songs you like (♥) from any source show up here"
            } else if widgets::takes_songs(p) {
                "This playlist is empty. Copy songs anywhere (Ctrl+A, Ctrl+C) and press Ctrl+V here"
            } else {
                "This playlist is empty"
            };
            panels::empty_state(ui, icon::MUSIC_NOTES, msg);
            return;
        }
        let opts = TableOpts {
            id: "playlist",
            context: &p.name,
            playlist: Some(p),
            show_album: !p.kind.is_album(),
            show_header: true,
        };
        widgets::track_table(ui, cx, &tracks, &opts, viewport, origin);
    });
}

fn playlist_actions(ui: &mut Ui, cx: &mut Cx, p: &Playlist) {
    match p.kind {
        PlaylistKind::Spotify | PlaylistKind::SpotifyLiked => {
            if widgets::icon_button(ui, icon::ARROWS_CLOCKWISE, 22.0, TEXT_DIM, "Sync with Spotify").clicked() {
                cx.actions.push(Action::Cmd(Command::SyncSpotify));
            }
        }
        PlaylistKind::SoundCloud | PlaylistKind::SoundCloudLikes => {
            if widgets::icon_button(ui, icon::ARROWS_CLOCKWISE, 22.0, TEXT_DIM, "Sync with SoundCloud").clicked() {
                cx.actions.push(Action::Cmd(Command::SyncSoundCloud));
            }
        }
        PlaylistKind::SpotifyAlbum | PlaylistKind::SoundCloudAlbum => {
            if widgets::icon_button(ui, icon::TRASH, 22.0, TEXT_DIM, "Remove from your albums").clicked() {
                cx.actions.push(Action::Delete(p.id.clone()));
            }
            let link = match (p.kind, p.remote_id.as_deref()) {
                (PlaylistKind::SpotifyAlbum, Some(id)) => Some(format!("https://open.spotify.com/album/{id}")),
                (_, Some(url)) if url.starts_with("https://") => Some(url.to_string()),
                _ => None,
            };
            if let Some(url) = link {
                let tip = format!("Open on {}", service_name(p.kind.source()));
                if widgets::icon_button(ui, icon::ARROW_SQUARE_OUT, 22.0, TEXT_DIM, &tip).clicked() {
                    cx.actions.push(Action::OpenUrl(url));
                }
            }
        }
        PlaylistKind::Custom | PlaylistKind::M3u => {
            if let Some(copied) = widgets::copied(ui.ctx()) {
                let tip = format!("Paste {} (Ctrl+V)", widgets::songs(copied.tracks.len()));
                if widgets::icon_button(ui, icon::CLIPBOARD_TEXT, 22.0, TEXT_DIM, &tip).clicked() {
                    widgets::paste(cx, p, None, Some(copied));
                }
            }
            if widgets::icon_button(ui, icon::PENCIL_SIMPLE, 22.0, TEXT_DIM, "Rename").clicked() {
                cx.actions.push(Action::Rename(p.id.clone(), p.name.clone()));
            }
            if widgets::icon_button(ui, icon::TRASH, 22.0, TEXT_DIM, "Delete").clicked() {
                cx.actions.push(Action::Delete(p.id.clone()));
            }
        }
        _ => {}
    }
    if p.track_ids.iter().any(|id| id.starts_with("local:"))
        && widgets::icon_button(ui, icon::EXPORT, 22.0, TEXT_DIM, "Export local songs as .m3u").clicked()
    {
        let dir = directories::UserDirs::new()
            .and_then(|u| u.audio_dir().map(|d| d.to_path_buf()))
            .unwrap_or_else(std::env::temp_dir);
        let safe: String = p
            .name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == ' ' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        cx.actions.push(Action::Cmd(Command::ExportM3u {
            playlist_id: p.id.clone(),
            path: dir.join(format!("{safe}.m3u")),
        }));
    }
}

// ------------------------------------------------------------------ artists and pages

fn library_artist(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState, key: &str) {
    let Some(a) = cx.lib.artist(key) else {
        let name = key.to_string();
        panels::empty_state(ui, icon::USER, "This artist isn't in your library anymore");
        ui.vertical_centered(|ui| {
            if widgets::pill(ui, "Search for them", cx.accent, theme::on_color(cx.accent)).clicked() {
                cx.actions.push(Action::Search(name));
            }
        });
        return;
    };
    let tint = a.art.as_deref().and_then(|src| cx.art.accent(src)).unwrap_or(cx.tint);
    page(ui, "artist", |ui, viewport, origin| {
        let all: Vec<&Track> = a.track_ids.iter().filter_map(|id| cx.lib.get(id)).collect();
        let total_ms: u64 = all.iter().map(|t| t.duration_ms).sum();
        let header = Header {
            kind: "Artist",
            playing_from: widgets::plays_from(cx, &a.name, |id| all.iter().any(|t| t.id == id)),
            title: &a.name,
            description: "",
            meta: format!("{} songs in your library, {}", all.len(), theme::fmt_total(total_ms)),
            source: None,
            tint,
            round: true,
        };
        let art = a.art.clone();
        let name = a.name.clone();
        let (play, shuffle) = list_header(
            ui,
            cx,
            st,
            &header,
            |ui, r, cx| widgets::cover_round(ui, cx.art, art.as_deref(), r, widgets::artist_fallback()),
            |ui, cx| {
                if widgets::action_button(
                    ui,
                    icon::MAGNIFYING_GLASS,
                    "Find on Spotify & SoundCloud",
                    false,
                    cx.accent,
                )
                .clicked()
                {
                    cx.actions.push(Action::Search(name.clone()));
                }
            },
        );
        let tracks = filter(all, st.filter_text);
        if play || shuffle {
            play_list(cx, &tracks, shuffle, &a.name);
        }
        let opts = TableOpts {
            id: "artist",
            context: &a.name,
            playlist: None,
            show_album: true,
            show_header: true,
        };
        // Their songs elsewhere (below) may still match the filter.
        if !no_matches(ui, tracks.len(), st.filter_text) {
            widgets::track_table(ui, cx, &tracks, &opts, viewport, origin);
        }

        // Their songs on Spotify and SoundCloud that aren't in the library (asked for by the
        // app as the page `artist:<name>`).
        let online = &cx.feed.page;
        if online.key != format!("artist:{}", a.name) || online.error.is_some() {
            return;
        }
        ui.add_space(18.0);
        if online.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    egui::RichText::new(format!("Looking for {} on Spotify and SoundCloud…", a.name)).color(TEXT_DIM),
                );
            });
            return;
        }
        let mut songs: Vec<Track> = a.track_ids.iter().filter_map(|id| cx.lib.get(id)).cloned().collect();
        let in_library = songs.len();
        crate::service::add_songs(&mut songs, online.tracks.clone());
        let more = songs.split_off(in_library);
        if more.is_empty() {
            return;
        }
        widgets::heading(ui, "More on Spotify & SoundCloud");
        let more: Vec<&Track> = more.iter().collect();
        let more = filter(more, st.filter_text);
        let opts = TableOpts {
            id: "artist-more",
            context: &a.name,
            playlist: None,
            show_album: true,
            show_header: false,
        };
        widgets::track_table(ui, cx, &more, &opts, viewport, origin);
    });
}

fn service_name(source: Option<Source>) -> &'static str {
    match source {
        Some(Source::Spotify) => "Spotify",
        Some(Source::SoundCloud) => "SoundCloud",
        Some(Source::AppleMusic) => "Apple Music",
        _ => "the web",
    }
}

/// An artist / album / playlist / song from Spotify, SoundCloud or Apple Music.
fn remote_page(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState, key: &str) {
    let p = &cx.feed.page;
    if p.key != key || p.loading {
        ui.add_space(80.0);
        ui.vertical_centered(|ui| {
            ui.spinner();
            ui.add_space(8.0);
            let what = match links::target(key) {
                Some(links::Target::Spotify(..)) => "Loading from Spotify…",
                Some(links::Target::SoundCloudUser(_) | links::Target::SoundCloudUrl(_)) => "Loading from SoundCloud…",
                Some(links::Target::AppleMusic { .. }) => "Loading from Apple Music…",
                Some(links::Target::ArtistName(_)) => "Looking on Spotify and SoundCloud…",
                _ => "Opening link…",
            };
            ui.label(egui::RichText::new(what).color(TEXT_DIM));
        });
        return;
    }
    if let Some(err) = &p.error {
        let err = err.clone();
        let url = p.external_url.clone().or_else(|| links::web_url(key));
        let needs_login = err.contains("Log in to Spotify");
        panels::empty_state(ui, icon::WARNING_CIRCLE, "Couldn't open this page");
        ui.vertical_centered(|ui| {
            ui.set_max_width(520.0);
            ui.label(egui::RichText::new(err).color(TEXT_FAINT));
            ui.add_space(12.0);
            let primary = if needs_login { "Open Settings" } else { "Try again" };
            ui.horizontal(|ui| {
                let spacing = ui.spacing().item_spacing.x;
                let w = widgets::pill_width(ui, primary)
                    + url
                        .as_ref()
                        .map_or(0.0, |_| spacing + widgets::pill_width(ui, "Open in browser"));
                ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                if widgets::pill(ui, primary, cx.accent, theme::on_color(cx.accent)).clicked() {
                    cx.actions.push(if needs_login {
                        Action::Go(View::Settings)
                    } else {
                        Action::Open(key.to_string())
                    });
                }
                if let Some(url) = url {
                    if widgets::pill(ui, "Open in browser", HOVER, TEXT).clicked() {
                        cx.actions.push(Action::OpenUrl(url));
                    }
                }
            });
        });
        return;
    }
    let source_tint = p.source.map(source_color).unwrap_or(cx.tint);
    let tint = p
        .image
        .as_deref()
        .and_then(|src| cx.art.accent(src))
        .unwrap_or(source_tint);
    // Clone what the closures need; `cx` is borrowed mutably inside.
    let page_state = p.clone();
    page(ui, "remote-page", |ui, viewport, origin| {
        let p = &page_state;
        let total_ms: u64 = p.tracks.iter().map(|t| t.duration_ms).sum();
        let mut meta = p.subtitle.clone();
        if total_ms > 0 && p.tracks.len() > 1 {
            meta.push_str(&format!(", {}", theme::fmt_total(total_ms)));
        }
        let header = Header {
            kind: &p.kind,
            playing_from: widgets::plays_from(cx, &p.title, |id| p.tracks.iter().any(|t| t.id == id)),
            title: &p.title,
            description: "",
            meta,
            source: p.source,
            tint,
            round: p.round,
        };
        let fallback = match p.source {
            Some(s) => (
                theme::mix(source_color(s), PANEL, 0.4),
                if p.round { icon::USER } else { icon::MUSIC_NOTES },
            ),
            None => widgets::artist_fallback(),
        };
        let (play, shuffle) = list_header(
            ui,
            cx,
            st,
            &header,
            |ui, r, cx| {
                if p.round {
                    widgets::cover_round(ui, cx.art, p.image.as_deref(), r, fallback)
                } else {
                    widgets::cover(ui, cx.art, p.image.as_deref(), r, 12, fallback)
                }
            },
            |ui, cx| {
                let saveable = p.kind == "Album"
                    && matches!(p.source, Some(Source::Spotify | Source::SoundCloud))
                    && !p.tracks.is_empty();
                if saveable {
                    match saved_album(cx.lib, &p.key, p.source) {
                        Some(saved) => {
                            let tip = if p.source == Some(Source::Spotify) {
                                "Remove from your albums (and your Spotify library)"
                            } else {
                                "Remove from your albums"
                            };
                            if widgets::action_button(ui, icon::CHECK, "Saved", false, cx.accent)
                                .on_hover_text(tip)
                                .clicked()
                            {
                                cx.actions.push(Action::Delete(saved.id.clone()));
                            }
                        }
                        None => {
                            let tip = if p.source == Some(Source::Spotify) {
                                "Add it to your albums (and your Spotify library)"
                            } else {
                                "Add it to your albums"
                            };
                            if widgets::action_button(ui, icon::PLUS, "Save album", false, cx.accent)
                                .on_hover_text(tip)
                                .clicked()
                            {
                                cx.actions.push(Action::Cmd(Command::SaveAlbum(p.key.clone())));
                            }
                        }
                    }
                }
                if !p.tracks.is_empty()
                    && widgets::icon_button(ui, icon::PLUS_CIRCLE, 28.0, TEXT_DIM, "Save as a playlist in Sumo")
                        .clicked()
                {
                    cx.actions.push(Action::Cmd(Command::CreatePlaylist {
                        name: p.title.clone(),
                        tracks: p.tracks.clone(),
                    }));
                }
                widgets::download_button(ui, cx, &p.tracks.iter().collect::<Vec<_>>(), 24.0);
                if let Some(url) = &p.external_url {
                    let tip = format!("Open on {}", service_name(p.source));
                    if widgets::icon_button(ui, icon::ARROW_SQUARE_OUT, 24.0, TEXT_DIM, &tip).clicked() {
                        cx.actions.push(Action::OpenUrl(url.clone()));
                    }
                }
            },
        );
        let all: Vec<&Track> = p.tracks.iter().collect();
        let tracks = filter(all, st.filter_text);
        if play || shuffle {
            play_list(cx, &tracks, shuffle, &p.title);
        }
        if p.source == Some(Source::AppleMusic) {
            ui.label(
                egui::RichText::new("Apple Music songs play from your local files, Spotify or SoundCloud")
                    .small()
                    .color(TEXT_FAINT),
            );
            ui.add_space(4.0);
        }
        if no_matches(ui, tracks.len(), st.filter_text) {
            return;
        }
        if tracks.is_empty() {
            panels::empty_state(ui, icon::MUSIC_NOTES, "No playable songs here");
            return;
        }
        if p.kind == "Artist" {
            widgets::heading(ui, "Songs");
        }
        let opts = TableOpts {
            id: "remote-page",
            context: &p.title,
            playlist: None,
            show_album: p.kind != "Album",
            show_header: p.kind != "Artist",
        };
        widgets::track_table(ui, cx, &tracks, &opts, viewport, origin);
        if let Some(other) = p.merging {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    egui::RichText::new(format!("Adding {}'s songs from {}…", p.title, other.label())).color(TEXT_DIM),
                );
            });
        }
    });
}

// ------------------------------------------------------------------ search

fn search(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    let q = st.search_text.trim().to_string();
    page(ui, "search", |ui, viewport, origin| {
        if q.is_empty() {
            browse(ui, cx);
            return;
        }
        if let Some(link) = links::parse(&q) {
            link_card(ui, cx, &link);
            return;
        }

        // Library results (cached until the library changes).
        if st.search_cache.0 != q || st.search_cache.1 != cx.lib.version {
            let hits = cx.lib.search(&q, 50);
            *st.search_cache = (q.clone(), cx.lib.version, hits.into_iter().map(|t| t.id).collect());
        }
        let local: Vec<&Track> = st.search_cache.2.iter().filter_map(|id| cx.lib.get(id)).collect();
        let search = cx.feed.search.clone();
        let fresh = search.query == q;
        let empty: Vec<Track> = Vec::new();
        let spotify = if fresh { &search.spotify } else { &empty };
        let soundcloud = if fresh { &search.soundcloud } else { &empty };
        let spotify_pending = !fresh || search.spotify_pending;
        let soundcloud_pending = !fresh || search.soundcloud_pending;

        // Artists: Spotify and SoundCloud first (their pages have everything), then the
        // library's own.
        let mut artists: Vec<ArtistHit> = if fresh { search.artists.clone() } else { Vec::new() };
        artists.extend(cx.lib.search_artists(&q, 4).into_iter().map(|a| ArtistHit {
            key: format!("local:artist:{}", a.key),
            name: a.name.clone(),
            image: a.art.clone(),
            source: Source::Local,
            subtitle: format!("{} songs in your library", a.track_ids.len()),
        }));

        // Songs: the catalogue results first, like Spotify.
        let songs: Vec<&Track> = spotify
            .iter()
            .chain(soundcloud.iter())
            .chain(local.iter().copied())
            .take(4)
            .collect();
        // An artist named like the query is the top result: exact match first (Spotify and
        // SoundCloud before the library), then prefix.
        let q_lower = q.to_lowercase();
        let exact_artist = top_artist(cx.feed, &q).or_else(|| {
            artists
                .iter()
                .find(|a| a.name.to_lowercase() == q_lower)
                .or_else(|| artists.iter().find(|a| a.name.to_lowercase().starts_with(&q_lower)))
                .cloned()
        });
        let ctx_name = format!("Search: {q}");

        let any = !songs.is_empty() || !artists.is_empty();
        if any {
            let wide = ui.available_width() > 620.0;
            let top_w = if wide {
                (ui.available_width() * 0.4).clamp(260.0, 460.0)
            } else {
                ui.available_width()
            };
            let show_top = |ui: &mut Ui, cx: &mut Cx| {
                widgets::heading(ui, "Top result");
                match (&exact_artist, songs.first(), artists.first()) {
                    (Some(a), _, _) => top_artist_card(ui, cx, a, top_w),
                    (None, Some(t), _) => top_track_card(ui, cx, t, top_w, &songs, &ctx_name),
                    (None, None, Some(a)) => top_artist_card(ui, cx, a, top_w),
                    _ => {}
                }
            };
            let show_songs = |ui: &mut Ui, cx: &mut Cx| {
                widgets::heading(ui, "Songs");
                if songs.is_empty() {
                    remote_status(ui, spotify_pending || soundcloud_pending);
                } else {
                    let opts = TableOpts {
                        id: "s-top",
                        context: &ctx_name,
                        playlist: None,
                        show_album: false,
                        show_header: false,
                    };
                    widgets::track_table(ui, cx, &songs, &opts, viewport, origin);
                }
            };
            if wide {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 24.0;
                    ui.vertical(|ui| {
                        ui.set_width(top_w);
                        show_top(ui, cx);
                    });
                    ui.vertical(|ui| show_songs(ui, cx));
                });
            } else {
                show_top(ui, cx);
                ui.add_space(12.0);
                show_songs(ui, cx);
            }
            ui.add_space(20.0);
        }

        if !artists.is_empty() {
            widgets::heading(ui, "Artists");
            let n = artists.len().min(tiles_per_row(ui, 150.0));
            widgets::grid(ui, n, 150.0, |ui, i, w| {
                let a = &artists[i];
                let (resp, play) = widgets::tile(ui, w, &a.name, &a.subtitle, cx.accent, |ui, r| {
                    widgets::cover_round(ui, cx.art, a.image.as_deref(), r, artist_source_fallback(a.source));
                    source_corner(ui, r, a.source);
                });
                // Library artists play right away; the others open their page first.
                let library_artist = a.key.strip_prefix("local:artist:").and_then(|k| cx.lib.artist(k));
                match (play, library_artist) {
                    (true, Some(artist)) => cx.actions.push(Action::Cmd(Command::Play {
                        tracks: cx.lib.tracks_for(&artist.track_ids),
                        start: widgets::first_song(cx, artist.track_ids.len()),
                        context: artist.name.clone(),
                    })),
                    _ if resp.clicked() => cx.actions.push(Action::Open(a.key.clone())),
                    _ => {}
                }
            });
            ui.add_space(8.0);
        }

        let albums: &[CollectionHit] = if fresh { &search.albums } else { &[] };
        let playlists: &[CollectionHit] = if fresh { &search.playlists } else { &[] };
        collection_row(ui, cx, "Albums", albums);
        collection_row(ui, cx, "Playlists", playlists);

        // Library songs not already shown under "Songs".
        let local: Vec<&Track> = local
            .into_iter()
            .filter(|t| !songs.iter().any(|s| s.id == t.id))
            .collect();
        if !local.is_empty() {
            widgets::heading_icon(ui, icon::BOOKS, TEXT_DIM, "More in your library");
            let opts = TableOpts {
                id: "s-local",
                context: &ctx_name,
                playlist: None,
                show_album: true,
                show_header: false,
            };
            widgets::track_table(ui, cx, &local, &opts, viewport, origin);
            ui.add_space(16.0);
        }
        let error_for = |prefix: &str| {
            search
                .errors
                .iter()
                .find_map(|e| e.strip_prefix(prefix))
                .filter(|_| fresh)
                .map(str::to_string)
        };
        let spotify_error = error_for("Spotify: ");
        let soundcloud_error = error_for("SoundCloud: ");
        if cx.feed.spotify_logged_in {
            widgets::heading_icon(ui, icon::SPOTIFY_LOGO, source_color(Source::Spotify), "Spotify");
            if let (true, Some(e)) = (spotify.is_empty(), &spotify_error) {
                search_error(ui, e);
                if e.contains("rate limited")
                    && widgets::action_button(ui, icon::GEAR, "Add your own Spotify app", false, cx.accent).clicked()
                {
                    cx.actions.push(Action::Go(View::Settings));
                }
            } else if spotify.is_empty() {
                remote_status(ui, spotify_pending);
            } else {
                let refs: Vec<&Track> = spotify.iter().collect();
                let opts = TableOpts {
                    id: "s-spotify",
                    context: &ctx_name,
                    playlist: None,
                    show_album: true,
                    show_header: false,
                };
                widgets::track_table(ui, cx, &refs, &opts, viewport, origin);
            }
            ui.add_space(16.0);
        }
        if !soundcloud.is_empty() || soundcloud_pending {
            widgets::heading_icon(
                ui,
                icon::SOUNDCLOUD_LOGO,
                source_color(Source::SoundCloud),
                "SoundCloud",
            );
            if let (true, Some(e)) = (soundcloud.is_empty(), &soundcloud_error) {
                search_error(ui, e);
            } else if soundcloud.is_empty() {
                remote_status(ui, soundcloud_pending);
            } else {
                let refs: Vec<&Track> = soundcloud.iter().collect();
                let opts = TableOpts {
                    id: "s-sc",
                    context: &ctx_name,
                    playlist: None,
                    show_album: false,
                    show_header: false,
                };
                widgets::track_table(ui, cx, &refs, &opts, viewport, origin);
            }
        }
        if fresh {
            // Errors not shown in a section above (e.g. while results are still listed).
            for e in &search.errors {
                let shown = (e.starts_with("Spotify: ") && spotify.is_empty() && cx.feed.spotify_logged_in)
                    || (e.starts_with("SoundCloud: ") && soundcloud.is_empty());
                if !shown {
                    ui.label(egui::RichText::new(e).small().color(DANGER));
                }
            }
        }
        let pending = spotify_pending || soundcloud_pending;
        if !any
            && local.is_empty()
            && spotify.is_empty()
            && soundcloud.is_empty()
            && albums.is_empty()
            && playlists.is_empty()
            && !pending
        {
            panels::empty_state(ui, icon::MAGNIFYING_GLASS, &format!("No results for “{q}”"));
        }
    });
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

fn artist_source_fallback(source: Source) -> (Color32, &'static str) {
    match source {
        Source::Local => widgets::artist_fallback(),
        s => (theme::mix(source_color(s), PANEL, 0.45), icon::USER),
    }
}

/// Small service logo in the corner of an artist picture.
fn source_corner(ui: &Ui, art: Rect, source: Source) {
    let c = art.right_bottom() - vec2(art.width() * 0.15, art.height() * 0.15);
    ui.painter().circle_filled(c, 13.0, PANEL);
    let glyph = if source == Source::Local {
        icon::BOOKS
    } else {
        theme::source_icon(source)
    };
    let color = if source == Source::Local {
        TEXT_DIM
    } else {
        source_color(source)
    };
    theme::paint_icon(ui.painter(), c, glyph, theme::icon_font(15.0), color);
}

fn top_card_frame(ui: &mut Ui, width: f32) -> (Rect, egui::Response) {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 230.0), Sense::click());
    ui.painter().rect_filled(rect, CornerRadius::same(RADIUS + 2), CARD);
    widgets::fade_fill(ui, resp.id, rect, RADIUS + 2, resp.hovered(), HOVER);
    (rect, resp.on_hover_cursor(CursorIcon::PointingHand))
}

/// Play button that appears on hover. Returns true when clicked.
fn hover_play(ui: &Ui, cx: &Cx, rect: Rect, hovered: bool, id: Id) -> bool {
    let t = ui.ctx().animate_bool_with_time(id, hovered, 0.15);
    if t <= 0.0 {
        return false;
    }
    let c = rect.right_bottom() - vec2(42.0, 42.0 - 6.0 * (1.0 - t));
    let btn = Rect::from_center_size(c, vec2(46.0, 46.0));
    let over = ui.rect_contains_pointer(btn);
    let fill = if over {
        theme::mix(cx.accent, Color32::WHITE, 0.15)
    } else {
        cx.accent
    };
    ui.painter()
        .rect_filled(btn, CornerRadius::same(14), theme::with_alpha(fill, (255.0 * t) as u8));
    theme::paint_icon(
        ui.painter(),
        c + vec2(1.5, 0.0),
        egui_phosphor::fill::PLAY,
        theme::fill_icon_font(20.0),
        theme::with_alpha(theme::on_color(cx.accent), (255.0 * t) as u8),
    );
    over && ui.input(|i| i.pointer.primary_clicked())
}

fn top_artist_card(ui: &mut Ui, cx: &mut Cx, a: &ArtistHit, width: f32) {
    let (rect, resp) = top_card_frame(ui, width);
    let art = Rect::from_min_size(rect.min + vec2(20.0, 20.0), vec2(96.0, 96.0));
    widgets::cover_round(ui, cx.art, a.image.as_deref(), art, artist_source_fallback(a.source));
    text_trunc(
        ui,
        Pos2::new(rect.left() + 20.0, art.bottom() + 18.0),
        &a.name,
        theme::bold_font(30.0),
        TEXT,
        width - 40.0,
    );
    let y = art.bottom() + 66.0;
    let label = match a.source {
        Source::Local => "Artist • in your library".to_string(),
        s => format!("Artist • {}", s.label()),
    };
    type_chip(ui, Pos2::new(rect.left() + 20.0, y), &label, a.source);
    let play = hover_play(ui, cx, rect, resp.hovered(), Id::new(("top-play", &a.key)));
    if play {
        // Library artists can play straight away; others open their page first.
        if let Some(artist) = a.key.strip_prefix("local:artist:").and_then(|k| cx.lib.artist(k)) {
            cx.actions.push(Action::Cmd(Command::Play {
                tracks: cx.lib.tracks_for(&artist.track_ids),
                start: widgets::first_song(cx, artist.track_ids.len()),
                context: artist.name.clone(),
            }));
            return;
        }
    }
    if resp.clicked() {
        cx.actions.push(Action::Open(a.key.clone()));
    }
}

fn top_track_card(ui: &mut Ui, cx: &mut Cx, t: &Track, width: f32, songs: &[&Track], context: &str) {
    let (rect, resp) = top_card_frame(ui, width);
    let art = Rect::from_min_size(rect.min + vec2(20.0, 20.0), vec2(96.0, 96.0));
    theme::art_shadow(ui, art, 12);
    widgets::cover(ui, cx.art, t.art.as_deref(), art, 12, widgets::track_fallback(t));
    text_trunc(
        ui,
        Pos2::new(rect.left() + 20.0, art.bottom() + 18.0),
        &t.title,
        theme::bold_font(30.0),
        TEXT,
        width - 40.0,
    );
    let y = art.bottom() + 66.0;
    let used = type_chip(ui, Pos2::new(rect.left() + 20.0, y), "Song", t.source);
    let artist = widgets::link_text(
        ui,
        Id::new(("top-artist", &t.id)),
        Pos2::new(used.right() + 10.0, y - 8.0),
        &t.artist,
        theme::font(14.0),
        TEXT_DIM,
        rect.right() - used.right() - 90.0,
    );
    if artist.clicked() {
        cx.actions.push(widgets::artist_action(cx.lib, &t.artist));
        return;
    }
    let play = hover_play(ui, cx, rect, resp.hovered(), Id::new(("top-play", &t.id)));
    if play || resp.double_clicked() {
        cx.actions.push(Action::Cmd(Command::Play {
            tracks: songs.iter().map(|t| (*t).clone()).collect(),
            start: 0,
            context: context.to_string(),
        }));
    }
    let track = t.clone();
    resp.context_menu(|ui| widgets::track_menu(ui, cx, &track, None));
}

/// "Artist" / "Song" chip with the service logo. Returns its rect.
fn type_chip(ui: &Ui, left_center: Pos2, label: &str, source: Source) -> Rect {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), theme::bold_font(13.0), TEXT);
    let w = galley.size().x + 24.0 + 18.0;
    let rect = Rect::from_min_size(left_center - vec2(0.0, 14.0), vec2(w, 28.0));
    ui.painter()
        .rect_filled(rect, CornerRadius::same(14), Color32::from_black_alpha(90));
    let glyph = if source == Source::Local {
        icon::BOOKS
    } else {
        theme::source_icon(source)
    };
    let color = if source == Source::Local {
        TEXT_DIM
    } else {
        source_color(source)
    };
    theme::paint_icon(
        ui.painter(),
        Pos2::new(rect.left() + 18.0, rect.center().y),
        glyph,
        theme::icon_font(14.0),
        color,
    );
    ui.painter().galley(
        Pos2::new(rect.left() + 30.0, rect.center().y - galley.size().y / 2.0),
        galley,
        TEXT,
    );
    rect
}

fn link_card(ui: &mut Ui, cx: &mut Cx, link: &links::Link) {
    let (glyph, service) = match link {
        links::Link::Spotify { .. } => (icon::SPOTIFY_LOGO, Some(Source::Spotify)),
        links::Link::SoundCloud { .. } => (icon::SOUNDCLOUD_LOGO, Some(Source::SoundCloud)),
        links::Link::AppleMusic { .. } => (icon::APPLE_LOGO, Some(Source::AppleMusic)),
        links::Link::Short { .. } => (icon::LINK, None),
    };
    let what = match link {
        links::Link::Spotify { kind, .. } | links::Link::AppleMusic { kind, .. } => {
            links::kind_label(*kind).to_lowercase()
        }
        links::Link::SoundCloud { .. } => "profile, track or playlist".into(),
        links::Link::Short { .. } => "short link".into(),
    };
    widgets::heading(ui, "Open link");
    widgets::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width().min(560.0));
        ui.horizontal(|ui| {
            let color = service.map(source_color).unwrap_or(TEXT_DIM);
            ui.label(
                egui::RichText::new(glyph)
                    .family(theme::icons())
                    .size(30.0)
                    .color(color),
            );
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(format!("{} {what}", service_name(service).replace("the web", "Music")))
                        .font(theme::bold_font(17.0)),
                );
                ui.label(egui::RichText::new("Press Enter or click Open to load it").color(TEXT_DIM));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::pill(ui, "Open", cx.accent, theme::on_color(cx.accent)).clicked() {
                    cx.actions.push(Action::Open(links::page_key(link)));
                }
            });
        });
    });
}

fn search_error(ui: &mut Ui, error: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(icon::WARNING_CIRCLE)
                .family(theme::icons())
                .color(DANGER),
        );
        ui.label(egui::RichText::new(error).color(TEXT_DIM));
    });
    ui.add_space(6.0);
}

fn remote_status(ui: &mut Ui, pending: bool) {
    ui.horizontal(|ui| {
        if pending {
            ui.spinner();
            ui.label(egui::RichText::new("Searching…").color(TEXT_DIM));
        } else {
            ui.label(egui::RichText::new("No results").color(TEXT_FAINT));
        }
    });
    ui.add_space(4.0);
}

fn browse(ui: &mut Ui, cx: &mut Cx) {
    widgets::heading(ui, "Search everywhere");
    let sources = [
        (Source::Local, "Your library", "Local files and every imported playlist"),
        (
            Source::Spotify,
            "Spotify",
            "Songs, artists, albums and playlists from the whole catalogue (log in under Settings)",
        ),
        (
            Source::SoundCloud,
            "SoundCloud",
            "Tracks, artists, albums and playlists, no account needed",
        ),
        (
            Source::AppleMusic,
            "Apple Music",
            "Paste an Apple Music link to open an artist, album or playlist",
        ),
    ];
    widgets::grid(ui, sources.len(), 200.0, |ui, i, w| {
        let (s, title, sub) = sources[i];
        let (rect, _) = ui.allocate_exact_size(vec2(w, 120.0), Sense::hover());
        let base = source_color(s);
        ui.painter()
            .rect_filled(rect, CornerRadius::same(8), theme::mix(base, PANEL, 0.5));
        ui.painter().text(
            rect.left_top() + vec2(16.0, 16.0),
            Align2::LEFT_TOP,
            title,
            theme::bold_font(20.0),
            Color32::WHITE,
        );
        let galley = ui.painter().layout(
            sub.to_string(),
            theme::font(12.5),
            theme::with_alpha(Color32::WHITE, 210),
            w - 80.0,
        );
        ui.painter()
            .galley(rect.left_top() + vec2(16.0, 48.0), galley, Color32::WHITE);
        theme::paint_icon(
            ui.painter(),
            rect.right_bottom() - vec2(34.0, 34.0),
            theme::source_icon(s),
            theme::icon_font(40.0),
            theme::with_alpha(Color32::WHITE, 170),
        );
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(icon::LINK).family(theme::icons()).color(TEXT_DIM));
        ui.label(
            egui::RichText::new(
                "Tip: paste a Spotify, SoundCloud or Apple Music link (artist, album, playlist or song) \
                 into the search bar to open it.",
            )
            .color(TEXT_DIM),
        );
    });
    let _ = cx;
}

// ------------------------------------------------------------------ now playing

// ------------------------------------------------------------------ downloads

/// One line of the Downloads page.
struct DownloadRow {
    track: Track,
    state: DownloadState,
}

/// SoundCloud downloads: what's going on now, then everything saved earlier.
fn downloads(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    // This session's downloads: running, waiting, failed, then the newest saved first.
    let mut rows: Vec<DownloadRow> = Vec::new();
    let rank = |s: &DownloadState| match s {
        DownloadState::Running(_) => 0,
        DownloadState::Queued => 1,
        DownloadState::Failed(_) => 2,
        DownloadState::Done { .. } => 3,
    };
    for group in 0..4 {
        let items = cx.feed.downloads.iter().filter(|d| rank(&d.state) == group);
        let items: Vec<_> = if group == 3 {
            items.rev().collect()
        } else {
            items.collect()
        };
        rows.extend(items.into_iter().map(|d| DownloadRow {
            track: d.track.clone(),
            state: d.state.clone(),
        }));
    }
    // Earlier downloads, as far as the library still knows the song.
    let session: std::collections::HashSet<&str> = cx.feed.downloads.iter().map(|d| d.track.id.as_str()).collect();
    let mut earlier: Vec<DownloadRow> = cx
        .feed
        .downloaded
        .iter()
        .filter(|(id, _)| !session.contains(id.as_str()))
        .filter_map(|(id, path)| {
            let track = cx
                .lib
                .get(id)
                .or_else(|| cx.lib.get(&Track::local_id(&path.to_string_lossy())))?;
            Some(DownloadRow {
                track: track.clone(),
                state: DownloadState::Done {
                    path: path.clone(),
                    from: String::new(),
                },
            })
        })
        .collect();
    earlier.sort_by_cached_key(|r| (r.track.artist.to_lowercase(), r.track.title.to_lowercase()));
    let active = rows.iter().filter(|r| r.state.active()).count();
    let finished = rows.len() - active;

    page(ui, "downloads", |ui, _viewport, _origin| {
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Downloads").font(theme::bold_font(30.0)));
        ui.add_space(4.0);
        let dir = st.download_dir.to_string_lossy().to_string();
        let home = directories::BaseDirs::new()
            .map(|b| b.home_dir().to_string_lossy().to_string())
            .unwrap_or_default();
        let shown = match dir.strip_prefix(&home) {
            Some(rest) if !home.is_empty() => format!("~{rest}"),
            _ => dir.clone(),
        };
        let saved_to = if st.download_custom {
            format!("Songs are saved to {shown}")
        } else {
            format!("Songs are saved in {shown}, in a folder for each service")
        };
        ui.label(egui::RichText::new(saved_to).color(TEXT_DIM));
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if widgets::pill(ui, "Open folder", HOVER, TEXT).clicked() {
                let _ = std::fs::create_dir_all(st.download_dir);
                cx.actions.push(Action::OpenUrl(dir.clone()));
            }
            if active > 0 && widgets::pill(ui, "Cancel all", HOVER, TEXT).clicked() {
                cx.actions.push(Action::Cmd(Command::CancelDownloads));
            }
            if finished > 0 && widgets::pill(ui, "Clear list", HOVER, TEXT).clicked() {
                cx.actions.push(Action::Cmd(Command::ClearDownloads));
            }
            if widgets::pill(ui, "Change folder", HOVER, TEXT).clicked() {
                cx.actions.push(Action::Go(View::Settings));
            }
        });
        ui.add_space(18.0);

        if rows.is_empty() && earlier.is_empty() {
            panels::empty_state(ui, icon::DOWNLOAD_SIMPLE, "Songs you download show up here");
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(
                        "Right-click a song and choose Download, or use the download button on a playlist, \
                         album or profile.",
                    )
                    .size(12.5)
                    .color(TEXT_FAINT),
                );
            });
        }
        if !rows.is_empty() {
            let title = if active > 0 {
                format!("Downloading · {active} left")
            } else {
                "This session".to_string()
            };
            ui.label(egui::RichText::new(title).font(theme::bold_font(18.0)));
            ui.add_space(6.0);
            let tracks: Vec<Track> = rows.iter().map(|r| r.track.clone()).collect();
            for (i, row) in rows.iter().enumerate() {
                download_row(ui, cx, row, &tracks, i);
            }
            ui.add_space(18.0);
        }
        if !earlier.is_empty() {
            ui.label(egui::RichText::new("Downloaded earlier").font(theme::bold_font(18.0)));
            ui.add_space(6.0);
            let tracks: Vec<Track> = earlier.iter().map(|r| r.track.clone()).collect();
            for (i, row) in earlier.iter().enumerate() {
                download_row(ui, cx, row, &tracks, i);
            }
            ui.add_space(18.0);
        }
        let missing;
        let note = match st.ytdlp.map(|program| widgets::ytdlp_status(ui, cx, program)) {
            Some(Some(Err(_))) => {
                missing = format!(
                    "yt-dlp isn't installed, so Spotify and Apple Music songs are only looked for on SoundCloud. \
                     To find them on YouTube too: {}",
                    crate::tools::install_hint("yt-dlp")
                );
                missing.as_str()
            }
            Some(_) => {
                "Spotify and Apple Music audio is DRM-protected, so those songs are saved from the same \
                 recording on YouTube or SoundCloud, then tagged with all their details."
            }
            None => {
                "Spotify and Apple Music audio is DRM-protected, so those songs are saved from the same \
                 recording on SoundCloud (YouTube is off in Settings), then tagged with all their details."
            }
        };
        ui.label(egui::RichText::new(note).size(12.0).color(TEXT_FAINT));
    });
}

fn download_row(ui: &mut Ui, cx: &mut Cx, row: &DownloadRow, tracks: &[Track], i: usize) {
    const H: f32 = 56.0;
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, H), Sense::click());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let t = &row.track;
    let id = Id::new(("download-row", &t.id));
    let hovered = ui.rect_contains_pointer(rect) || resp.context_menu_opened();
    widgets::fade_fill(ui, id, rect, 10, hovered, HOVER);
    let art = Rect::from_min_size(rect.min + vec2(8.0, (H - 40.0) / 2.0), vec2(40.0, 40.0));
    widgets::cover(ui, cx.art, t.art.as_deref(), art, 6, widgets::track_fallback(t));

    // The state on the right decides how much room the titles get.
    let right_w = match row.state {
        DownloadState::Failed(_) => (w * 0.5).clamp(160.0, 420.0),
        _ => 220.0_f32.min(w * 0.4),
    };
    let tx = art.right() + 12.0;
    let tw = (rect.right() - right_w - tx - 12.0).max(40.0);
    text_trunc(
        ui,
        Pos2::new(tx, rect.top() + 9.0),
        &t.title,
        theme::font(14.5),
        TEXT,
        tw,
    );
    text_trunc(
        ui,
        Pos2::new(tx, rect.top() + 30.0),
        &t.artist,
        theme::font(12.5),
        TEXT_DIM,
        tw,
    );

    let right = Rect::from_min_max(
        Pos2::new(rect.right() - right_w, rect.top()),
        rect.right_bottom() - vec2(8.0, 0.0),
    );
    let painter = ui.painter();
    let button: Option<(&str, &str)> = match &row.state {
        DownloadState::Queued => {
            painter.text(
                right.right_center() - vec2(40.0, 0.0),
                Align2::RIGHT_CENTER,
                "Waiting…",
                theme::font(13.0),
                TEXT_FAINT,
            );
            Some((icon::X, "Cancel"))
        }
        DownloadState::Running(p) => {
            let bar = Rect::from_min_size(
                Pos2::new(right.left(), right.center().y - 2.0),
                vec2((right.width() - 88.0).max(20.0), 4.0),
            );
            painter.rect_filled(bar, CornerRadius::same(2), SELECTED);
            let filled = Rect::from_min_size(bar.min, vec2(bar.width() * p.clamp(0.0, 1.0), bar.height()));
            painter.rect_filled(filled, CornerRadius::same(2), cx.accent);
            painter.text(
                right.right_center() - vec2(40.0, 0.0),
                Align2::RIGHT_CENTER,
                format!("{:.0}%", p * 100.0),
                theme::font(13.0),
                TEXT_DIM,
            );
            Some((icon::X, "Cancel"))
        }
        DownloadState::Done { from, .. } => {
            let label = match from.as_str() {
                "" => "Saved".to_string(),
                "original file" => "Saved · original file".to_string(),
                from => format!("Saved · from {from}"),
            };
            painter.text(
                right.right_center() - vec2(40.0, 0.0),
                Align2::RIGHT_CENTER,
                label,
                theme::font(13.0),
                TEXT_DIM,
            );
            Some((icon::FOLDER_OPEN, "Show in folder"))
        }
        DownloadState::Failed(e) => {
            let used = text_trunc(
                ui,
                Pos2::new(right.left(), right.center().y - 8.0),
                e,
                theme::font(13.0),
                DANGER,
                right.width() - 44.0,
            );
            let _ = ui.interact(used, id.with("error"), Sense::hover()).on_hover_text(e);
            Some((icon::ARROW_CLOCKWISE, "Try again"))
        }
    };
    let mut button_clicked = false;
    if let Some((glyph, tip)) = button {
        let r = Rect::from_center_size(Pos2::new(right.right() - 14.0, right.center().y), vec2(30.0, 30.0));
        let b = ui.interact(r, id.with("button"), Sense::click());
        let color = if b.hovered() { TEXT } else { TEXT_DIM };
        theme::paint_icon(ui.painter(), r.center(), glyph, theme::icon_font(18.0), color);
        if b.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip).clicked() {
            button_clicked = true;
            match &row.state {
                DownloadState::Done { path, .. } => {
                    if let Some(dir) = path.parent() {
                        cx.actions.push(Action::OpenUrl(dir.to_string_lossy().to_string()));
                    }
                }
                DownloadState::Queued | DownloadState::Running(_) => {
                    cx.actions.push(Action::Cmd(Command::CancelDownload(t.id.clone())));
                }
                DownloadState::Failed(_) => cx.actions.push(Action::Cmd(Command::Download(vec![t.clone()]))),
            }
        }
    }
    if resp.double_clicked() && !button_clicked {
        cx.actions.push(Action::Cmd(Command::Play {
            tracks: tracks.to_vec(),
            start: i,
            context: "Downloads".into(),
        }));
    }
    resp.context_menu(|ui| widgets::track_menu(ui, cx, t, None));
}

fn now_playing(ui: &mut Ui, cx: &mut Cx) {
    let full = ui.max_rect();
    let tint = cx.tint;
    rounded_top_gradient(
        ui,
        Rect::from_min_size(full.min, vec2(full.width(), full.height())),
        theme::with_alpha(tint, 150),
        theme::with_alpha(tint, 10),
    );
    let Some(t) = cx.player.current.clone() else {
        panels::empty_state(ui, icon::VINYL_RECORD, "Nothing is playing");
        return;
    };
    egui::Frame::new().inner_margin(Margin::same(28)).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.horizontal(|ui| {
            if widgets::icon_button(ui, icon::CORNERS_IN, 18.0, TEXT, "Close (Esc)").clicked() {
                cx.actions.push(Action::Back);
            }
            ui.label(
                egui::RichText::new(format!("PLAYING FROM {}", cx.player.context.to_uppercase()))
                    .size(12.0)
                    .color(TEXT_DIM),
            );
        });
        ui.add_space(12.0);
        let avail = ui.available_size();
        let wide = avail.x > 860.0;
        let art_src = t
            .art
            .clone()
            .or_else(|| cx.player.via.as_ref().and_then(|v| v.art.clone()));
        if wide {
            let art_size = (avail.y - 120.0).min(avail.x * 0.42).clamp(200.0, 560.0);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(art_size);
                    let (r, _) = ui.allocate_exact_size(vec2(art_size, art_size), Sense::hover());
                    theme::art_shadow(ui, r, 12);
                    widgets::cover(ui, cx.art, art_src.as_deref(), r, 12, widgets::track_fallback(&t));
                    ui.add_space(18.0);
                    track_titles(ui, cx, &t, art_size);
                });
                ui.add_space(40.0);
                ui.vertical(|ui| {
                    ui.set_height(avail.y);
                    panels::lyrics(ui, cx, true);
                });
            });
        } else {
            let art_size = (avail.x * 0.5).clamp(140.0, 320.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(art_size, art_size), Sense::hover());
                widgets::cover(ui, cx.art, art_src.as_deref(), r, 12, widgets::track_fallback(&t));
                ui.add_space(16.0);
                ui.vertical(|ui| track_titles(ui, cx, &t, avail.x - art_size - 20.0));
            });
            ui.add_space(16.0);
            panels::lyrics(ui, cx, true);
        }
    });
}

fn track_titles(ui: &mut Ui, cx: &mut Cx, t: &Track, w: f32) {
    let (r, _) = ui.allocate_exact_size(vec2(w, 36.0), Sense::hover());
    text_trunc(ui, r.min, &t.title, theme::bold_font(28.0), TEXT, w);
    let (r, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
    text_trunc(ui, r.min, &t.artist, theme::font(17.0), TEXT_DIM, w);
    if !t.album.is_empty() {
        let (r, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
        text_trunc(ui, r.min, &t.album, theme::font(13.5), TEXT_FAINT, w);
    }
    ui.add_space(4.0);
    let source = cx.player.via.as_ref().map(|v| v.source).unwrap_or(t.source);
    widgets::source_badge(ui, source);
    if let Some(q) = &cx.player.quality {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(w.min(320.0), 20.0), Sense::hover());
            let used = text_trunc(
                ui,
                r.left_top() + vec2(0.0, 2.0),
                &q.label(),
                theme::font(12.5),
                TEXT_DIM,
                r.width() - 70.0,
            );
            widgets::quality_badge(ui, Pos2::new(used.right() + 8.0, used.center().y), q);
        });
    }
}
