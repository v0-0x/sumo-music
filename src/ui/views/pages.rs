//! Search, and pages from Spotify and SoundCloud (artists, albums, playlists, links).

use super::*;

pub(super) fn service_name(source: Option<Source>) -> &'static str {
    match source {
        Some(Source::Spotify) => "Spotify",
        Some(Source::SoundCloud) => "SoundCloud",
        Some(Source::AppleMusic) => "Apple Music",
        _ => "the web",
    }
}

/// An artist / album / playlist / song from Spotify, SoundCloud or Apple Music.
pub(super) fn remote_page(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState, key: &str) {
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

pub(super) fn search(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
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
                match library_artist {
                    Some(artist) => {
                        resp.context_menu(|ui| widgets::songs_menu(ui, cx, &artist.track_ids, &artist.name, None))
                    }
                    None => resp.context_menu(|ui| widgets::page_menu(ui, cx, &a.key)),
                };
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

pub(super) fn artist_source_fallback(source: Source) -> (Color32, &'static str) {
    match source {
        Source::Local => widgets::artist_fallback(),
        s => (theme::mix(source_color(s), PANEL, 0.45), icon::USER),
    }
}

/// Small service logo in the corner of an artist picture.
pub(super) fn source_corner(ui: &Ui, art: Rect, source: Source) {
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

pub(super) fn top_card_frame(ui: &mut Ui, width: f32) -> (Rect, egui::Response) {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 230.0), Sense::click());
    ui.painter().rect_filled(rect, CornerRadius::same(RADIUS + 2), CARD);
    widgets::fade_fill(ui, resp.id, rect, RADIUS + 2, resp.hovered(), HOVER);
    (rect, resp.on_hover_cursor(CursorIcon::PointingHand))
}

/// Play button that appears on hover. Returns true when clicked.
pub(super) fn hover_play(ui: &Ui, cx: &Cx, rect: Rect, hovered: bool, id: Id) -> bool {
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

pub(super) fn top_artist_card(ui: &mut Ui, cx: &mut Cx, a: &ArtistHit, width: f32) {
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

pub(super) fn top_track_card(ui: &mut Ui, cx: &mut Cx, t: &Track, width: f32, songs: &[&Track], context: &str) {
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
pub(super) fn type_chip(ui: &Ui, left_center: Pos2, label: &str, source: Source) -> Rect {
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

pub(super) fn link_card(ui: &mut Ui, cx: &mut Cx, link: &links::Link) {
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

pub(super) fn search_error(ui: &mut Ui, error: &str) {
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

pub(super) fn remote_status(ui: &mut Ui, pending: bool) {
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

pub(super) fn browse(ui: &mut Ui, cx: &mut Cx) {
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
