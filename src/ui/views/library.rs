//! The library's pages: songs, albums, artists, one album, one playlist, one library artist.

use super::*;

pub(super) fn songs(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
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

/// How the Albums page orders its albums.
#[derive(Clone, Copy, Default, PartialEq)]
pub(super) enum AlbumSort {
    #[default]
    Artist,
    Title,
    Added,
}

impl AlbumSort {
    fn label(self) -> &'static str {
        match self {
            AlbumSort::Artist => "Artist",
            AlbumSort::Title => "Title",
            AlbumSort::Added => "Recently added",
        }
    }
}

pub(super) fn albums(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    page(ui, "albums", |ui, _viewport, _origin| {
        let sort_id = Id::new("albums-sort");
        let mut sort: AlbumSort = ui.data(|d| d.get_temp(sort_id)).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Albums").font(theme::bold_font(26.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(st.filter_text)
                        .hint_text(theme::ic(icon::MAGNIFYING_GLASS, "Filter"))
                        .desired_width(200.0),
                );
                ui.menu_button(theme::ic(icon::SORT_ASCENDING, sort.label()), |ui| {
                    ui.set_min_width(170.0);
                    for by in [AlbumSort::Artist, AlbumSort::Title, AlbumSort::Added] {
                        if ui.add(egui::Button::selectable(sort == by, by.label())).clicked() {
                            sort = by;
                            ui.data_mut(|d| d.insert_temp(sort_id, by));
                            ui.close();
                        }
                    }
                });
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
        let mut list: Vec<LibAlbum> = everything
            .into_iter()
            .filter(|a| only.is_none_or(|s| a.source() == s))
            .filter(|a| {
                needle.is_empty()
                    || a.name().to_lowercase().contains(&needle)
                    || a.artist().to_lowercase().contains(&needle)
            })
            .collect();
        match sort {
            AlbumSort::Artist => {}
            AlbumSort::Title => list.sort_by_cached_key(|a| crate::library::sort_key(a.name())),
            AlbumSort::Added => list.sort_by_cached_key(|a| std::cmp::Reverse(a.added(cx.lib))),
        }
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

pub(super) fn artists(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
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
            resp.context_menu(|ui| widgets::songs_menu(ui, cx, &a.track_ids, &a.name, None));
        });
    });
}

pub(super) fn album(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState, key: &str) {
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

pub(super) fn playlist(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState, id: &str) {
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

pub(super) fn playlist_actions(ui: &mut Ui, cx: &mut Cx, p: &Playlist) {
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

pub(super) fn library_artist(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState, key: &str) {
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
