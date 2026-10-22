//! Home: shortcuts, recently and most played, recently added, your artists, playlists and albums.

use super::*;

pub(super) fn home(ui: &mut Ui, cx: &mut Cx) {
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
            crate::ui::profile::home_card(ui, cx);
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

        // Most played (twice or more), one row.
        let top: Vec<&Track> = cx
            .lib
            .most_played
            .iter()
            .filter_map(|id| cx.lib.get(id))
            .take(tiles_per_row(ui, 160.0))
            .collect();
        if top.len() >= 3 {
            widgets::heading(ui, "Most played");
            widgets::grid(ui, top.len(), 160.0, |ui, i, w| {
                let t = top[i];
                let (resp, play) = widgets::tile(ui, w, &t.title, &t.artist, cx.accent, |ui, r| {
                    widgets::cover(ui, cx.art, t.art.as_deref(), r, 6, widgets::track_fallback(t))
                });
                if play || resp.double_clicked() {
                    cx.actions.push(Action::Cmd(Command::Play {
                        tracks: top.iter().map(|t| (*t).clone()).collect(),
                        start: i,
                        context: "Most played".into(),
                    }));
                }
                let track = t.clone();
                resp.context_menu(|ui| widgets::track_menu(ui, cx, &track, None));
            });
            ui.add_space(10.0);
        }

        // Albums from your music folders, newest first, one row.
        let mut fresh: Vec<&Album> = cx.lib.albums.iter().filter(|a| a.added > 0).collect();
        if fresh.len() >= 2 {
            fresh.sort_by_key(|a| std::cmp::Reverse(a.added));
            fresh.truncate(tiles_per_row(ui, 168.0));
            let fresh: Vec<LibAlbum> = fresh.into_iter().map(LibAlbum::Local).collect();
            widgets::heading(ui, "Recently added");
            album_grid(ui, cx, &fresh);
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
                resp.context_menu(|ui| widgets::songs_menu(ui, cx, &a.track_ids, &a.name, None));
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
        crate::ui::profile::home_card(ui, cx);
    });
}

pub(super) fn shortcut_card(ui: &mut Ui, cx: &mut Cx, p: &Playlist, w: f32) {
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

pub(super) fn onboarding(ui: &mut Ui, cx: &mut Cx) {
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
