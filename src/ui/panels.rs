//! Window chrome: the sidebar, the player dock and the right panel.

use egui::{vec2, Align, Align2, Color32, CornerRadius, CursorIcon, Id, Layout, Margin, Pos2, Rect, Sense, Stroke, Ui};
use egui_phosphor::regular as icon;

use super::theme::{self, *};
use super::widgets::{self, text_trunc, CONTROL};
use super::{Action, Cx, RightTab, View};
use crate::library::LIKED_ID;
use crate::links;
use crate::model::{PlaylistKind, RepeatMode, Source, Track};
use crate::service::{Command, PlayStatus, Sleep, SleepTimer};

/// Space around the panels.
pub const GAP: f32 = 12.0;

// ------------------------------------------------------------------ sidebar

pub struct SidebarState<'a> {
    pub view: &'a View,
    pub search_text: &'a mut String,
    pub focus_search: &'a mut bool,
    /// Icons only (narrow windows).
    pub collapsed: bool,
}

/// Width of the icons-and-covers sidebar.
pub const SIDEBAR_RAIL: f32 = 72.0;
/// Dragged narrower than this, the sidebar switches to (and snaps to) the rail.
pub const SIDEBAR_COLLAPSE_AT: f32 = 170.0;

/// How the sidebar panel is sized this frame.
pub struct SidebarSize {
    pub id: Id,
    /// Width to start at (the saved width); `None` = fixed rail for narrow windows.
    pub default: Option<f32>,
}

/// Draws the sidebar and returns its width.
pub fn sidebar(ui: &mut Ui, cx: &mut Cx, st: &mut SidebarState, size: SidebarSize) -> f32 {
    let panel = match size.default {
        Some(w) => egui::Panel::left(size.id)
            .resizable(true)
            .default_size(w)
            .size_range(SIDEBAR_RAIL..=420.0),
        None => egui::Panel::left(size.id).exact_size(SIDEBAR_RAIL).resizable(false),
    };
    panel
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(WINDOW_BG).inner_margin(Margin {
            left: GAP as i8,
            right: 0,
            top: GAP as i8,
            bottom: 0,
        }))
        .show(ui, |ui| {
            // Dragged narrow, the sidebar shows just icons and covers.
            st.collapsed |= ui.max_rect().width() + GAP < SIDEBAR_COLLAPSE_AT;
            ui.spacing_mut().item_spacing.y = 2.0;
            brand(ui, cx, st.collapsed);
            ui.add_space(12.0);
            if st.collapsed {
                if nav_item(ui, cx, icon::MAGNIFYING_GLASS, "Search", View::Search, st.view, true).clicked() {
                    *st.focus_search = true;
                }
            } else {
                let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width() - 4.0, 38.0), Sense::hover());
                search_box(ui, cx, st, rect);
                ui.add_space(10.0);
            }
            nav_item(ui, cx, icon::HOUSE, "Home", View::Home, st.view, st.collapsed);
            ui.add_space(10.0);
            section_label(ui, "Library", st.collapsed);
            nav_item(
                ui,
                cx,
                icon::HEART,
                "Liked Songs",
                View::Playlist(LIKED_ID.into()),
                st.view,
                st.collapsed,
            );
            if !cx.lib.local.is_empty() {
                nav_item(ui, cx, icon::FOLDER, "Local Files", View::Songs, st.view, st.collapsed);
            }
            nav_item(
                ui,
                cx,
                icon::VINYL_RECORD,
                "Albums",
                View::Albums,
                st.view,
                st.collapsed,
            );
            nav_item(
                ui,
                cx,
                icon::USERS_THREE,
                "Artists",
                View::Artists,
                st.view,
                st.collapsed,
            );
            // Always there, so it can be found before the first download.
            let left = cx.feed.downloads.iter().filter(|d| d.state.active()).count();
            let label = if left > 0 {
                format!("Downloads · {left}")
            } else {
                "Downloads".to_string()
            };
            nav_item(
                ui,
                cx,
                icon::DOWNLOAD_SIMPLE,
                &label,
                View::Downloads,
                st.view,
                st.collapsed,
            );
            ui.add_space(10.0);
            playlists_header(ui, cx, st.collapsed);
            if let Some((done, total)) = cx.feed.scan {
                if !st.collapsed {
                    ui.label(
                        egui::RichText::new(if total == 0 {
                            "Scanning folders…".to_string()
                        } else {
                            format!("Scanning {done}/{total}…")
                        })
                        .small()
                        .color(TEXT_FAINT),
                    );
                }
            }
            // The playlist list fills the space above Settings (or the Last.fm account card).
            let bottom = match (super::profile::username(cx.feed).is_some(), st.collapsed) {
                (false, _) => 50.0,
                (true, false) => 66.0,
                (true, true) => 92.0,
            };
            let list_h = (ui.available_height() - bottom).max(40.0);
            let view = st.view.clone();
            let collapsed = st.collapsed;
            ui.allocate_ui(vec2(ui.available_width(), list_h), |ui| {
                let playlists: Vec<&crate::model::Playlist> = cx
                    .lib
                    .playlists
                    .iter()
                    .filter(|p| {
                        p.id != LIKED_ID
                            && !p.kind.is_album()
                            && (p.kind == PlaylistKind::Custom || !p.track_ids.is_empty())
                    })
                    .collect();
                if playlists.is_empty() && !collapsed {
                    ui.label(
                        egui::RichText::new(
                            "Playlists you make or import from Spotify, SoundCloud and Apple Music show up here.",
                        )
                        .small()
                        .color(TEXT_FAINT),
                    );
                }
                egui::ScrollArea::vertical()
                    .id_salt("sidebar-playlists")
                    .auto_shrink([false, false])
                    .show_rows(ui, PLAYLIST_ROW, playlists.len(), |ui, range| {
                        for i in range {
                            playlist_row(ui, cx, playlists[i], &view, collapsed);
                        }
                    });
            });
            ui.add_space(6.0);
            if !super::profile::sidebar_account(ui, cx, st.view, st.collapsed) {
                nav_item(ui, cx, icon::GEAR, "Settings", View::Settings, st.view, st.collapsed);
            }
        })
        .response
        .rect
        .width()
}

fn brand(ui: &mut Ui, cx: &mut Cx, collapsed: bool) {
    let w = ui.available_width() - 4.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w, 36.0), Sense::hover());
    let logo = if collapsed {
        Rect::from_center_size(rect.center(), vec2(32.0, 32.0))
    } else {
        Rect::from_min_size(Pos2::new(rect.left() + 6.0, rect.center().y - 16.0), vec2(32.0, 32.0))
    };
    egui::Image::from_texture(egui::load::SizedTexture::new(cx.logo, logo.size())).paint_at(ui, logo);
    let tip = if collapsed {
        "Expand the sidebar"
    } else {
        "Collapse the sidebar"
    };
    let resp = ui
        .interact(logo, Id::new("brand-logo"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(tip);
    if resp.clicked() {
        cx.actions.push(Action::ToggleLibrary);
    }
    if !collapsed {
        ui.painter().text(
            Pos2::new(logo.right() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            "Sumo",
            theme::bold_font(18.0),
            TEXT,
        );
    }
}

fn section_label(ui: &mut Ui, text: &str, collapsed: bool) {
    if collapsed {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width() - 4.0, 12.0), Sense::hover());
        ui.painter().line_segment(
            [
                Pos2::new(r.left() + 14.0, r.center().y),
                Pos2::new(r.right() - 14.0, r.center().y),
            ],
            Stroke::new(1.0, HOVER),
        );
        return;
    }
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width() - 4.0, 24.0), Sense::hover());
    ui.painter().text(
        Pos2::new(r.left() + 10.0, r.center().y),
        Align2::LEFT_CENTER,
        text.to_uppercase(),
        theme::bold_font(11.0),
        TEXT_FAINT,
    );
}

/// A sidebar entry. Returns its response (clicks already navigate).
fn nav_item(
    ui: &mut Ui,
    cx: &mut Cx,
    glyph: &str,
    label: &str,
    target: View,
    current: &View,
    collapsed: bool,
) -> egui::Response {
    // A saved album opens as a library page, but it belongs with the albums.
    let saved_album = |id: &str| cx.lib.playlist(id).is_some_and(|p| p.kind.is_album());
    let active = *current == target
        || (target == View::Albums && matches!(current, View::Album(_)))
        || (target == View::Albums && matches!(current, View::Playlist(id) if saved_album(id)))
        || (target == View::Artists && matches!(current, View::Artist(_)));
    let w = ui.available_width() - 4.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 36.0), Sense::click());
    let hovered = resp.hovered();
    if active {
        ui.painter().rect_filled(rect, CornerRadius::same(10), SELECTED);
    } else {
        widgets::fade_fill(ui, resp.id, rect, 10, hovered, CARD);
    }
    let color = if active || hovered { TEXT } else { TEXT_DIM };
    let font = if active {
        theme::fill_icon_font(18.0)
    } else {
        theme::icon_font(18.0)
    };
    let icon_x = if collapsed { rect.center().x } else { rect.left() + 20.0 };
    theme::paint_icon(ui.painter(), Pos2::new(icon_x, rect.center().y), glyph, font, color);
    if !collapsed {
        ui.painter().text(
            Pos2::new(rect.left() + 40.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            if active {
                theme::bold_font(14.0)
            } else {
                theme::font(14.0)
            },
            color,
        );
    }
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let resp = if collapsed { resp.on_hover_text(label) } else { resp };
    if resp.clicked() {
        cx.actions.push(Action::Go(target));
    }
    resp
}

fn playlists_header(ui: &mut Ui, cx: &mut Cx, collapsed: bool) {
    if collapsed {
        section_label(ui, "", true);
        return;
    }
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width() - 4.0, 26.0), Sense::hover());
    ui.painter().text(
        Pos2::new(r.left() + 10.0, r.center().y),
        Align2::LEFT_CENTER,
        "PLAYLISTS",
        theme::bold_font(11.0),
        TEXT_FAINT,
    );
    let plus = Rect::from_center_size(Pos2::new(r.right() - 14.0, r.center().y), vec2(24.0, 24.0));
    let resp = ui
        .interact(plus, Id::new("new-playlist"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("New playlist");
    if resp.hovered() {
        ui.painter().rect_filled(plus, CornerRadius::same(6), CARD);
    }
    theme::paint_icon(
        ui.painter(),
        plus.center(),
        icon::PLUS,
        theme::icon_font(15.0),
        if resp.hovered() { TEXT } else { TEXT_DIM },
    );
    if resp.clicked() {
        cx.actions.push(Action::NewPlaylist(Vec::new()));
    }
}

const PLAYLIST_ROW: f32 = 44.0;

fn playlist_row(ui: &mut Ui, cx: &mut Cx, p: &crate::model::Playlist, view: &View, collapsed: bool) {
    let w = ui.available_width() - 4.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, PLAYLIST_ROW), Sense::click());
    let active = *view == View::Playlist(p.id.clone());
    let hovered = resp.hovered() || resp.context_menu_opened();
    if active {
        ui.painter().rect_filled(rect, CornerRadius::same(10), SELECTED);
    } else {
        widgets::fade_fill(ui, resp.id, rect, 10, hovered, CARD);
    }
    let art = if collapsed {
        Rect::from_center_size(rect.center(), vec2(34.0, 34.0))
    } else {
        Rect::from_min_size(Pos2::new(rect.left() + 6.0, rect.center().y - 15.0), vec2(30.0, 30.0))
    };
    widgets::playlist_cover(ui, cx.art, p, art, 7);
    let here = widgets::plays_from(cx, &p.name, |id| p.track_ids.iter().any(|t| t == id));
    let playing = here && cx.player.status == PlayStatus::Playing;
    if !collapsed {
        let x = art.right() + 10.0;
        let right_icon_w = 26.0;
        text_trunc(
            ui,
            Pos2::new(x, rect.center().y - 9.0),
            &p.name,
            theme::font(13.5),
            if here {
                cx.accent
            } else if active || hovered {
                TEXT
            } else {
                TEXT_DIM
            },
            rect.right() - x - right_icon_w,
        );
        let marker = Pos2::new(rect.right() - 14.0, rect.center().y);
        if playing {
            theme::paint_icon(
                ui.painter(),
                marker,
                egui_phosphor::fill::SPEAKER_HIGH,
                theme::fill_icon_font(13.0),
                cx.accent,
            );
        } else if let Some(src) = p.kind.source() {
            theme::paint_icon(
                ui.painter(),
                marker,
                theme::source_icon(src),
                theme::icon_font(13.0),
                theme::with_alpha(source_color(src), if hovered { 255 } else { 150 }),
            );
        }
    }
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let resp = if collapsed { resp.on_hover_text(&p.name) } else { resp };
    if resp.clicked() {
        cx.actions.push(Action::Go(View::Playlist(p.id.clone())));
    }
    if resp.double_clicked() {
        play_ids(cx, &p.track_ids, &p.name);
    }
    resp.context_menu(|ui| {
        ui.set_min_width(200.0);
        if ui.button(theme::ic(icon::PLAY, "Play")).clicked() {
            play_ids(cx, &p.track_ids, &p.name);
            ui.close();
        }
        if ui.button(theme::ic(icon::LIST_PLUS, "Add to queue")).clicked() {
            cx.actions
                .push(Action::Cmd(Command::Enqueue(cx.lib.tracks_for(&p.track_ids))));
            ui.close();
        }
        if ui.button(theme::ic(icon::COPY, "Copy songs")).clicked() {
            widgets::copy_songs(ui.ctx(), cx, cx.lib.tracks_for(&p.track_ids));
            ui.close();
        }
        if let Some(copied) = widgets::copied(ui.ctx()).filter(|_| widgets::takes_songs(p)) {
            let label = format!("Paste {}", widgets::songs(copied.tracks.len()));
            if ui.button(theme::ic(icon::CLIPBOARD_TEXT, &label)).clicked() {
                widgets::paste(cx, p, None, Some(copied));
                ui.close();
            }
        }
        ui.separator();
        if p.kind.is_editable() {
            if ui.button(theme::ic(icon::PENCIL_SIMPLE, "Rename")).clicked() {
                cx.actions.push(Action::Rename(p.id.clone(), p.name.clone()));
                ui.close();
            }
            if ui.button(theme::ic(icon::TRASH, "Delete")).clicked() {
                cx.actions.push(Action::Delete(p.id.clone()));
                ui.close();
            }
        } else if ui.button(theme::ic(icon::TRASH, "Remove from Sumo")).clicked() {
            cx.actions.push(Action::Delete(p.id.clone()));
            ui.close();
        }
    });
}

fn play_ids(cx: &mut Cx, ids: &[String], context: &str) {
    let tracks = cx.lib.tracks_for(ids);
    if !tracks.is_empty() {
        cx.actions.push(Action::Cmd(Command::Play {
            start: widgets::first_song(cx, tracks.len()),
            tracks,
            context: context.to_string(),
        }));
    }
}

fn search_box(ui: &mut Ui, cx: &mut Cx, st: &mut SidebarState, rect: Rect) {
    let bg = ui.interact(rect, Id::new("search-box-bg"), Sense::click());
    let edit_id = Id::new("sidebar-search");
    let focused = ui.memory(|m| m.has_focus(edit_id));
    let hovered = ui.rect_contains_pointer(rect);
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(10),
        if hovered && !focused { HOVER } else { CARD },
    );
    if focused {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(10),
            Stroke::new(1.5, theme::with_alpha(cx.accent, 200)),
            egui::StrokeKind::Inside,
        );
    }
    theme::paint_icon(
        ui.painter(),
        Pos2::new(rect.left() + 18.0, rect.center().y),
        icon::MAGNIFYING_GLASS,
        theme::icon_font(16.0),
        if focused { TEXT } else { TEXT_DIM },
    );
    let clear_w = if st.search_text.is_empty() { 10.0 } else { 30.0 };
    let edit_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 34.0, rect.center().y - 9.0),
        Pos2::new(rect.right() - clear_w, rect.center().y + 9.0),
    );
    let before = st.search_text.len();
    let resp = ui.put(
        edit_rect,
        egui::TextEdit::singleline(st.search_text)
            .id(edit_id)
            .hint_text(egui::RichText::new("Search or paste a link").color(TEXT_FAINT))
            .frame(egui::Frame::NONE)
            .margin(Margin::ZERO)
            .font(theme::font(14.0))
            .desired_width(edit_rect.width()),
    );
    if bg.clicked() || std::mem::take(st.focus_search) {
        resp.request_focus();
    }
    if !st.search_text.is_empty() {
        let x = Rect::from_center_size(Pos2::new(rect.right() - 16.0, rect.center().y), vec2(24.0, 24.0));
        let xr = ui
            .interact(x, Id::new("sidebar-search-clear"), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text("Clear");
        theme::paint_icon(
            ui.painter(),
            x.center(),
            icon::X_CIRCLE,
            theme::fill_icon_font(16.0),
            if xr.hovered() { TEXT } else { TEXT_FAINT },
        );
        if xr.clicked() {
            st.search_text.clear();
            resp.request_focus();
        }
    }
    let text = st.search_text.trim().to_string();
    let link = links::parse(&text);
    if resp.changed() {
        // A pasted link opens right away; typed text goes to the search page.
        match &link {
            Some(l) if st.search_text.len() >= before + 8 => cx.actions.push(Action::Open(links::page_key(l))),
            _ if *st.view != View::Search => cx.actions.push(Action::Go(View::Search)),
            _ => {}
        }
    } else if resp.gained_focus() && *st.view != View::Search && text.is_empty() {
        cx.actions.push(Action::Go(View::Search));
    }
    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !text.is_empty() {
        // Enter on an artist's exact name goes straight to their Spotify / SoundCloud page.
        let artist = super::views::top_artist(cx.feed, &text);
        cx.actions.push(match (&link, artist) {
            (Some(l), _) => Action::Open(links::page_key(l)),
            (None, Some(a)) => Action::Open(a.key),
            (None, None) => Action::Search(text),
        });
    }
}

// ------------------------------------------------------------------ content toolbar

/// Back / forward above the main view.
/// A slim bar about a new version of Sumo, and its download and install.
pub fn update_bar(ui: &mut Ui, cx: &mut Cx, skipped: &mut String, dismissed: &mut String) {
    use crate::service::UpdateStatus;
    let update = &cx.feed.update;
    let Some(release) = update.available.as_ref() else {
        return;
    };
    let busy = matches!(
        update.status,
        UpdateStatus::Downloading(_)
            | UpdateStatus::NeedsPassword { .. }
            | UpdateStatus::Installing
            | UpdateStatus::Installed
            | UpdateStatus::Failed(_)
    );
    if !busy && (release.version == *skipped || release.version == *dismissed) {
        return;
    }
    let version = release.version.clone();
    let page = release.page.clone();
    egui::Frame::new()
        .fill(theme::mix(cx.accent, CARD, 0.82))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(14, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(icon::ARROW_CIRCLE_UP)
                        .family(theme::icons())
                        .size(18.0)
                        .color(TEXT),
                );
                match &update.status {
                    UpdateStatus::Downloading(done) => {
                        ui.label(format!("Downloading Sumo {version}…"));
                        ui.add(egui::ProgressBar::new(*done).desired_width(160.0).show_percentage());
                    }
                    UpdateStatus::NeedsPassword { .. } => {
                        ui.label(format!(
                            "Sumo {version} is downloaded. Enter your password to install it."
                        ));
                    }
                    UpdateStatus::Installing => {
                        ui.spinner();
                        ui.label(format!("Installing Sumo {version}. It starts again in a moment."));
                    }
                    UpdateStatus::Installed => {
                        ui.label(format!("Sumo {version} is installed. Start Sumo again to use it."));
                    }
                    UpdateStatus::Failed(why) => {
                        ui.label(egui::RichText::new(format!("The update didn't work: {why}")).color(DANGER));
                        if ui.button("Try again").clicked() {
                            cx.actions.push(Action::Cmd(Command::InstallUpdate));
                        }
                        if ui.button("Download page").clicked() {
                            cx.actions.push(Action::OpenUrl(page.clone()));
                        }
                    }
                    _ => {
                        ui.label(egui::RichText::new(format!("Sumo {version} is available")).strong());
                        if crate::updater::can_install() {
                            if widgets::pill(ui, "Update now", cx.accent, theme::on_color(cx.accent)).clicked() {
                                cx.actions.push(Action::Cmd(Command::InstallUpdate));
                            }
                        } else {
                            ui.label(
                                egui::RichText::new("Update: git pull && makepkg -sif")
                                    .small()
                                    .color(TEXT_DIM),
                            );
                        }
                        if ui.button("What's new").clicked() {
                            cx.actions.push(Action::OpenUrl(page.clone()));
                        }
                        if ui.button("Skip this version").clicked() {
                            *skipped = version.clone();
                        }
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !matches!(
                        update.status,
                        UpdateStatus::Downloading(_) | UpdateStatus::NeedsPassword { .. } | UpdateStatus::Installing
                    ) && widgets::icon_button(ui, icon::X, 13.0, TEXT_DIM, "Later").clicked()
                    {
                        *dismissed = version.clone();
                    }
                });
            });
        });
    ui.add_space(8.0);
}

pub fn toolbar(ui: &mut Ui, cx: &mut Cx, can_back: bool, can_forward: bool, search: Option<&mut SidebarState>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::hover());
    if let Some(st) = search {
        let left = rect.left() + 16.0 + 2.0 * 36.0 + 8.0;
        let w = (rect.right() - 16.0 - left).min(320.0);
        if w > 80.0 {
            let field = Rect::from_min_size(Pos2::new(left, rect.top() + 9.0), vec2(w, 36.0));
            search_box(ui, cx, st, field);
        }
    }
    let mut x = rect.left() + 16.0;
    for (glyph, enabled, tip, action) in [
        (icon::CARET_LEFT, can_back, "Back (Alt+←)", Action::Back),
        (icon::CARET_RIGHT, can_forward, "Forward (Alt+→)", Action::Forward),
    ] {
        let r = Rect::from_min_size(Pos2::new(x, rect.top() + 12.0), vec2(30.0, 30.0));
        let resp = ui.interact(
            r,
            Id::new(("toolbar", tip)),
            if enabled { Sense::click() } else { Sense::hover() },
        );
        let hovered = enabled && resp.hovered();
        ui.painter().rect_filled(r, CornerRadius::same(10), CARD);
        widgets::fade_fill(ui, resp.id, r, 10, hovered, HOVER);
        let color = match (enabled, hovered) {
            (false, _) => TEXT_FAINT,
            (true, true) => TEXT,
            (true, false) => TEXT_DIM,
        };
        theme::paint_icon(ui.painter(), r.center(), glyph, theme::icon_font(15.0), color);
        if enabled
            && resp
                .on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_text(tip)
                .clicked()
        {
            cx.actions.push(action);
        }
        x += 36.0;
    }
}

// ------------------------------------------------------------------ player dock

/// The player: a floating dock with the transport on the left, the song and its progress in
/// the middle and the toggles on the right.
pub fn player_dock(ui: &mut Ui, cx: &mut Cx, right: Option<RightTab>) {
    egui::Panel::bottom("player")
        .exact_size(76.0 + 2.0 * GAP)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(WINDOW_BG).inner_margin(Margin::same(GAP as i8)))
        .show(ui, |ui| {
            let dock = ui.max_rect();
            ui.painter().rect(
                dock,
                CornerRadius::same(18),
                CARD,
                Stroke::new(1.0, Color32::from_rgb(0x2a, 0x2a, 0x30)),
                egui::StrokeKind::Inside,
            );
            let compact = dock.width() < 900.0;
            let cy = dock.center().y;

            // Left: previous, play, next — equal slots, evenly spaced.
            let mut x = dock.left() + 14.0;
            let slot = |x: f32, size: f32| Rect::from_min_size(Pos2::new(x, cy - size / 2.0), vec2(size, size));
            if dock_button(
                ui,
                slot(x, 36.0),
                egui_phosphor::fill::SKIP_BACK,
                Look::Filled,
                cx.accent,
                "Previous",
            )
            .clicked()
            {
                cx.actions.push(Action::Cmd(Command::Previous));
            }
            x += 36.0 + 6.0;
            let playing = cx.player.status == PlayStatus::Playing;
            let play_rect = slot(x, 44.0);
            if play_button(ui, play_rect, cx.accent, playing).clicked() {
                cx.actions.push(Action::Cmd(Command::TogglePause));
            }
            x += 44.0 + 6.0;
            if dock_button(
                ui,
                slot(x, 36.0),
                egui_phosphor::fill::SKIP_FORWARD,
                Look::Filled,
                cx.accent,
                "Next",
            )
            .clicked()
            {
                cx.actions.push(Action::Cmd(Command::Next));
            }
            x += 36.0 + 16.0;

            // Right: toggles and volume, laid out from the right edge.
            let mut rx = dock.right() - 14.0;
            let vol_w = if compact { 0.0 } else { 84.0 };
            if vol_w > 0.0 {
                let bar = Rect::from_min_max(Pos2::new(rx - vol_w, cy - 8.0), Pos2::new(rx, cy + 8.0));
                let vol = cx.player.volume;
                let (_, changed) = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
                        widgets::bar(ui, Id::new("volume"), vol_w, vol / 100.0, cx.accent)
                    })
                    .inner;
                if let Some(f) = changed {
                    cx.actions.push(Action::Cmd(Command::SetVolume(f * 100.0)));
                }
                rx -= vol_w + 4.0;
            }
            let vol = cx.player.volume;
            let vol_icon = if vol <= 0.5 {
                icon::SPEAKER_X
            } else if vol < 50.0 {
                icon::SPEAKER_LOW
            } else {
                icon::SPEAKER_HIGH
            };
            let vr = take_slot(&mut rx, cy, CONTROL);
            if dock_button(
                ui,
                vr,
                vol_icon,
                Look::Plain,
                cx.accent,
                if vol <= 0.5 { "Unmute" } else { "Mute" },
            )
            .clicked()
            {
                cx.actions.push(Action::Cmd(mute_command(ui.ctx(), vol)));
            }
            // The mouse wheel over the volume turns it up or down.
            let volume_area = vr.union(Rect::from_min_max(
                Pos2::new(vr.right(), cy - 10.0),
                Pos2::new(vr.right() + vol_w + 4.0, cy + 10.0),
            ));
            if ui.rect_contains_pointer(volume_area) {
                // A wheel notch (a line) is 5%; touchpads scroll by points.
                let scroll: f32 = ui.input(|i| {
                    i.events
                        .iter()
                        .map(|e| match e {
                            egui::Event::MouseWheel { unit, delta, .. } => match unit {
                                egui::MouseWheelUnit::Line => delta.y * 5.0,
                                egui::MouseWheelUnit::Point => delta.y * 0.1,
                                egui::MouseWheelUnit::Page => delta.y * 20.0,
                            },
                            _ => 0.0,
                        })
                        .sum()
                });
                if scroll != 0.0 {
                    let v = (vol + scroll).clamp(0.0, 100.0);
                    cx.actions.push(Action::Cmd(Command::SetVolume(v)));
                }
            }
            // At every width: a window too narrow for the panel shows it over the page.
            let tabs: &[(RightTab, &str, &str)] = &[
                (RightTab::Queue, icon::QUEUE, "Queue"),
                (RightTab::Lyrics, icon::MICROPHONE_STAGE, "Lyrics"),
            ];
            for &(tab, glyph, tip) in tabs {
                if dock_button(
                    ui,
                    take_slot(&mut rx, cy, CONTROL),
                    glyph,
                    Look::Toggle(right == Some(tab)),
                    cx.accent,
                    tip,
                )
                .clicked()
                {
                    cx.actions.push(Action::RightTab(tab));
                }
            }
            if dock_button(
                ui,
                take_slot(&mut rx, cy, CONTROL),
                icon::CORNERS_OUT,
                Look::Plain,
                cx.accent,
                "Full screen lyrics (L)",
            )
            .clicked()
            {
                cx.actions.push(Action::Go(View::NowPlaying));
            }
            if !compact || cx.player.sleep.is_some() {
                sleep_button(ui, cx, take_slot(&mut rx, cy, CONTROL));
            }
            rx -= 8.0;
            let (rep_icon, rep_on, rep_tip) = match cx.player.repeat {
                RepeatMode::Off => (icon::REPEAT, false, "Repeat"),
                RepeatMode::All => (icon::REPEAT, true, "Repeat one"),
                RepeatMode::One => (icon::REPEAT_ONCE, true, "Repeat off"),
            };
            if dock_button(
                ui,
                take_slot(&mut rx, cy, CONTROL),
                rep_icon,
                Look::Toggle(rep_on),
                cx.accent,
                rep_tip,
            )
            .clicked()
            {
                cx.actions.push(Action::Cmd(Command::CycleRepeat));
            }
            let shuffle = cx.player.shuffle;
            if dock_button(
                ui,
                take_slot(&mut rx, cy, CONTROL),
                icon::SHUFFLE,
                Look::Toggle(shuffle),
                cx.accent,
                "Shuffle",
            )
            .clicked()
            {
                cx.actions.push(Action::Cmd(Command::SetShuffle(!shuffle)));
            }
            if let Some(t) = cx.player.current.clone() {
                let liked = cx.lib.is_liked(&t.id);
                let tip = if liked {
                    "Remove from Liked Songs"
                } else {
                    "Save to Liked Songs"
                };
                let slot = take_slot(&mut rx, cy, CONTROL);
                if dock_button(ui, slot, icon::HEART, Look::Like(liked), cx.accent, tip).clicked() {
                    cx.actions.push(Action::Cmd(Command::ToggleLike(t)));
                }
            }
            let middle = Rect::from_min_max(
                Pos2::new(x, dock.top() + 10.0),
                Pos2::new(rx - 12.0, dock.bottom() - 10.0),
            );
            now_playing(ui, cx, middle);
        });
}

/// The sleep timer: a moon that lights up while it runs, with a menu of times.
fn sleep_button(ui: &mut Ui, cx: &mut Cx, rect: Rect) {
    let on = cx.player.sleep.is_some();
    let resp = dock_button(ui, rect, icon::MOON, Look::Toggle(on), cx.accent, "Sleep timer");
    egui::Popup::menu(&resp).show(|ui| {
        ui.set_min_width(190.0);
        let status = match &cx.player.sleep {
            Some(Sleep::At(at)) => {
                let left = at.saturating_duration_since(std::time::Instant::now()).as_secs();
                format!("Pauses in {}:{:02}", left / 60, left % 60)
            }
            Some(Sleep::AfterSong(_)) => "Pauses after this song".to_string(),
            None => "Pause playback…".to_string(),
        };
        ui.label(egui::RichText::new(status).small().color(TEXT_FAINT));
        let mut pick = |ui: &mut Ui, label: &str, timer: Option<SleepTimer>| {
            if ui.button(label).clicked() {
                cx.actions.push(Action::Cmd(Command::SetSleep(timer)));
                ui.close();
            }
        };
        for m in [15, 30, 45, 60, 90] {
            let label = if m < 60 {
                format!("In {m} minutes")
            } else if m == 60 {
                "In 1 hour".to_string()
            } else {
                "In 1½ hours".to_string()
            };
            pick(ui, &label, Some(SleepTimer::Minutes(m)));
        }
        pick(ui, "After this song", Some(SleepTimer::EndOfSong));
        if on {
            ui.separator();
            pick(ui, "Turn off", None);
        }
    });
    // While it counts down, the menu's time and the dot stay current.
    if matches!(cx.player.sleep, Some(Sleep::At(_)))
        && egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp))
    {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
}

/// The next control slot of width `w`, going left from `rx`.
fn take_slot(rx: &mut f32, cy: f32, w: f32) -> Rect {
    let r = Rect::from_min_max(Pos2::new(*rx - w, cy - w / 2.0), Pos2::new(*rx, cy + w / 2.0));
    *rx -= w + 2.0;
    r
}

/// How a dock control is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    /// Outline icon.
    Plain,
    /// Filled icon (previous / next).
    Filled,
    /// On/off toggle: accent with a dot underneath when on.
    Toggle(bool),
    /// Heart: filled in the accent when liked.
    Like(bool),
}

/// A 32 px dock control with its icon optically centred.
fn dock_button(ui: &mut Ui, rect: Rect, glyph: &str, look: Look, accent: Color32, tooltip: &str) -> egui::Response {
    let resp = ui.interact(rect, Id::new(("dock", tooltip)), Sense::click());
    let hovered = resp.hovered();
    widgets::fade_fill(ui, resp.id, rect, 10, hovered, HOVER);
    let on = matches!(look, Look::Toggle(true) | Look::Like(true));
    let color = match (on, hovered) {
        (true, _) => accent,
        (false, true) => TEXT,
        (false, false) => TEXT_DIM,
    };
    // Phosphor's regular and fill variants share code points; the font picks the style.
    let font = if matches!(look, Look::Filled | Look::Like(true)) {
        theme::fill_icon_font(17.0)
    } else {
        theme::icon_font(17.0)
    };
    theme::paint_icon(ui.painter(), rect.center(), glyph, font, color);
    if look == Look::Toggle(true) {
        ui.painter()
            .circle_filled(Pos2::new(rect.center().x, rect.bottom() - 3.0), 1.8, color);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tooltip)
}

fn play_button(ui: &mut Ui, rect: Rect, accent: Color32, playing: bool) -> egui::Response {
    let resp = ui.interact(rect, Id::new("dock-play"), Sense::click());
    let fill = if resp.hovered() {
        theme::mix(accent, Color32::WHITE, 0.2)
    } else {
        accent
    };
    ui.painter().rect_filled(rect, CornerRadius::same(14), fill);
    let glyph = if playing {
        egui_phosphor::fill::PAUSE
    } else {
        egui_phosphor::fill::PLAY
    };
    let nudge = if playing { 0.0 } else { 1.0 };
    theme::paint_icon(
        ui.painter(),
        rect.center() + vec2(nudge, 0.0),
        glyph,
        theme::fill_icon_font(19.0),
        theme::on_color(fill),
    );
    resp.on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(if playing { "Pause" } else { "Play" })
}

/// Cover, title, artist / source and the seek bar.
fn now_playing(ui: &mut Ui, cx: &mut Cx, rect: Rect) {
    let Some(t) = cx.player.current.clone() else {
        ui.painter().text(
            Pos2::new(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            "Nothing playing",
            theme::font(13.0),
            TEXT_FAINT,
        );
        return;
    };
    let art = Rect::from_min_size(Pos2::new(rect.left(), rect.center().y - 26.0), vec2(52.0, 52.0));
    let art_src = t
        .art
        .clone()
        .or_else(|| cx.player.via.as_ref().and_then(|v| v.art.clone()));
    widgets::cover(ui, cx.art, art_src.as_deref(), art, 10, widgets::track_fallback(&t));
    if ui
        .interact(art, Id::new("np-art"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Full screen lyrics (L)")
        .clicked()
    {
        cx.actions.push(Action::Go(View::NowPlaying));
    }
    let x = art.right() + 14.0;
    let w = (rect.right() - x).max(60.0);

    // Title, then "artist · source · format".
    let title = widgets::link_text(
        ui,
        Id::new("np-title"),
        Pos2::new(x, rect.top() + 1.0),
        &t.title,
        theme::bold_font(14.0),
        TEXT,
        w,
    );
    if title.clicked() {
        cx.actions.push(Action::Go(View::NowPlaying));
    }
    let line_y = rect.top() + 21.0;
    let artist = widgets::link_text(
        ui,
        Id::new("np-artist"),
        Pos2::new(x, line_y),
        &t.artist,
        theme::font(12.5),
        TEXT_DIM,
        (w * 0.5).max(60.0),
    );
    if artist.clicked() {
        cx.actions.push(widgets::artist_action(cx.lib, &t.artist));
    }
    let (src, prefix) = match &cx.player.via {
        Some(v) => (v.source, "via "),
        None => (t.source, ""),
    };
    let src_color = theme::with_alpha(source_color(src), 220);
    let mut sx = artist.rect.right() + 10.0;
    let mid_y = artist.rect.center().y;
    if sx + 40.0 < rect.right() {
        theme::paint_icon(
            ui.painter(),
            Pos2::new(sx + 6.0, mid_y),
            theme::source_icon(src),
            theme::icon_font(12.0),
            src_color,
        );
        sx += 16.0;
        let mut line = format!("{prefix}{}", src.label());
        if let Some(q) = &cx.player.quality {
            line.push_str(" · ");
            line.push_str(&q.label());
        }
        let badge_w = if cx.player.quality.as_ref().is_some_and(|q| q.lossless) {
            66.0
        } else {
            0.0
        };
        let used = text_trunc(
            ui,
            Pos2::new(sx, mid_y - 7.0),
            &line,
            theme::font(11.5),
            src_color,
            rect.right() - sx - badge_w,
        );
        if let Some(q) = &cx.player.quality {
            widgets::quality_badge(ui, Pos2::new(used.right() + 6.0, used.center().y), q);
        }
    }

    // Seek bar with the times on either side.
    let p = cx.player;
    let dur = p.duration;
    let id = Id::new("seek");
    let drag: Option<f32> = ui.data(|d| d.get_temp(id));
    let shown = drag.map(|f| f as f64 * dur).unwrap_or_else(|| p.position_now());
    let y = rect.bottom() - 7.0;
    let time_w = 38.0;
    ui.painter().text(
        Pos2::new(x, y),
        Align2::LEFT_CENTER,
        theme::fmt_time(shown),
        theme::font(11.0),
        TEXT_DIM,
    );
    ui.painter().text(
        Pos2::new(rect.right(), y),
        Align2::RIGHT_CENTER,
        if dur > 0.0 { theme::fmt_time(dur) } else { "-:--".into() },
        theme::font(11.0),
        TEXT_DIM,
    );
    let bar = Rect::from_min_max(
        Pos2::new(x + time_w, y - 8.0),
        Pos2::new(rect.right() - time_w, y + 8.0),
    );
    if bar.width() > 20.0 {
        let frac = if dur > 0.0 { (shown / dur) as f32 } else { 0.0 };
        let (resp, changed) = ui
            .scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
                widgets::bar(ui, id, bar.width(), frac, cx.accent)
            })
            .inner;
        if let Some(f) = changed {
            ui.data_mut(|d| d.insert_temp(id, f));
        }
        // Hovering shows the time under the pointer.
        if let (Some(pos), true) = (resp.hover_pos(), dur > 0.0) {
            let f = ((pos.x - bar.left()) / bar.width()).clamp(0.0, 1.0);
            let text = theme::fmt_time(f as f64 * dur);
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Tooltip, id.with("hover")));
            let galley = painter.layout_no_wrap(text, theme::font(11.5), TEXT);
            let at = Rect::from_center_size(Pos2::new(pos.x, bar.top() - 14.0), galley.size() + vec2(12.0, 6.0));
            painter.rect_filled(at, CornerRadius::same(6), CARD);
            painter.galley(at.center() - galley.size() / 2.0, galley, TEXT);
        }
        if resp.drag_stopped() || resp.clicked() {
            if let Some(f) = ui.data(|d| d.get_temp::<f32>(id)) {
                if dur > 0.0 {
                    cx.actions.push(Action::Cmd(Command::Seek(f as f64 * dur)));
                }
            }
            ui.data_mut(|d| d.remove::<f32>(id));
        }
    }
}

/// Mutes, or brings back the volume from before muting.
pub fn mute_command(ctx: &egui::Context, volume: f32) -> Command {
    let id = Id::new("pre-mute");
    if volume > 0.5 {
        ctx.data_mut(|d| d.insert_temp(id, volume));
        Command::SetVolume(0.0)
    } else {
        Command::SetVolume(ctx.data(|d| d.get_temp(id)).unwrap_or(70.0))
    }
}

// ------------------------------------------------------------------ right panel

/// Size limits of the lyrics / queue panel (including the gap next to it).
pub const RIGHT_MIN: f32 = 280.0 + GAP;
pub const RIGHT_MAX: f32 = 600.0 + GAP;

/// Draws the lyrics / queue panel at the saved `width` (draggable) and returns its width.
pub fn right_panel(ui: &mut Ui, cx: &mut Cx, tab: RightTab, width: f32) -> f32 {
    egui::Panel::right("right")
        .resizable(true)
        .default_size(width)
        .size_range(RIGHT_MIN..=RIGHT_MAX)
        .show_separator_line(false)
        .frame(egui::Frame::new().inner_margin(Margin {
            left: 0,
            right: GAP as i8,
            top: GAP as i8,
            bottom: 0,
        }))
        .show(ui, |ui| right_panel_body(ui, cx, tab))
        .response
        .rect
        .width()
}

/// The right panel floating over the page, for windows too narrow to show it beside it.
/// `area` is where it may go (right of the sidebar, above the player).
pub fn right_overlay(ctx: &egui::Context, cx: &mut Cx, tab: RightTab, area: Rect) {
    let width = RIGHT_MIN.max(area.width().min(360.0));
    let rect = Rect::from_min_max(Pos2::new(area.right() - width, area.top()), area.max);
    egui::Area::new(egui::Id::new("right-overlay"))
        .order(egui::Order::Foreground)
        .fixed_pos(rect.min)
        .show(ctx, |ui| {
            ui.set_min_size(rect.size());
            ui.set_max_size(rect.size());
            ui.painter().add(
                egui::Shadow {
                    offset: [0, 6],
                    blur: 28,
                    spread: 0,
                    color: Color32::from_black_alpha(140),
                }
                .as_shape(rect, CornerRadius::same(RADIUS)),
            );
            right_panel_body(ui, cx, tab);
        });
}

/// The tabs (Playing, Lyrics, Queue) and what the chosen one shows.
fn right_panel_body(ui: &mut Ui, cx: &mut Cx, tab: RightTab) {
    egui::Frame::new()
        .fill(PANEL)
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(ui.available_height());
            ui.horizontal(|ui| {
                let options = [
                    (RightTab::NowPlaying, "Playing"),
                    (RightTab::Lyrics, "Lyrics"),
                    (RightTab::Queue, "Queue"),
                ];
                if let Some(t) = widgets::segmented(ui, &options, tab) {
                    if t != tab {
                        cx.actions.push(Action::RightTab(t));
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_button(ui, icon::X, 15.0, TEXT_DIM, "Close").clicked() {
                        cx.actions.push(Action::ToggleRightPanel);
                    }
                });
            });
            ui.add_space(14.0);
            match tab {
                RightTab::NowPlaying => now_playing_panel(ui, cx),
                RightTab::Lyrics => lyrics(ui, cx, false),
                RightTab::Queue => queue(ui, cx),
            }
        });
}

fn now_playing_panel(ui: &mut Ui, cx: &mut Cx) {
    let Some(t) = cx.player.current.clone() else {
        empty_state(ui, icon::MUSIC_NOTES, "Play something to see it here");
        return;
    };
    egui::ScrollArea::vertical()
        .id_salt("np-panel")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let w = ui.available_width();
            let (art, _) = ui.allocate_exact_size(vec2(w, w), Sense::hover());
            let src = t
                .art
                .clone()
                .or_else(|| cx.player.via.as_ref().and_then(|v| v.art.clone()));
            widgets::cover(ui, cx.art, src.as_deref(), art, 8, widgets::track_fallback(&t));
            ui.add_space(14.0);
            let (row, _) = ui.allocate_exact_size(vec2(w, 54.0), Sense::hover());
            let downloadable = t.source != Source::Local;
            let text_w = w - if downloadable { 76.0 } else { 40.0 };
            text_trunc(ui, row.min, &t.title, theme::bold_font(22.0), TEXT, text_w);
            let artist = widgets::link_text(
                ui,
                Id::new("np-panel-artist"),
                row.min + vec2(0.0, 32.0),
                &t.artist,
                theme::font(15.0),
                TEXT_DIM,
                text_w,
            );
            if artist.clicked() {
                cx.actions.push(widgets::artist_action(cx.lib, &t.artist));
            }
            let liked = cx.lib.is_liked(&t.id);
            let heart = Rect::from_center_size(Pos2::new(row.right() - 16.0, row.center().y), vec2(CONTROL, CONTROL));
            let hr = ui.interact(heart, Id::new("np-panel-like"), Sense::click());
            let (glyph, font, color) = if liked {
                (egui_phosphor::fill::HEART, theme::fill_icon_font(20.0), cx.accent)
            } else {
                (
                    icon::HEART,
                    theme::icon_font(20.0),
                    if hr.hovered() { TEXT } else { TEXT_DIM },
                )
            };
            theme::paint_icon(ui.painter(), heart.center(), glyph, font, color);
            if hr.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                cx.actions.push(Action::Cmd(Command::ToggleLike(t.clone())));
            }
            if downloadable {
                let r = heart.translate(vec2(-(CONTROL + 4.0), 0.0));
                download_control(ui, cx, &t, r);
            }
            ui.add_space(6.0);
            let source = cx.player.via.as_ref().map(|v| v.source).unwrap_or(t.source);
            ui.horizontal(|ui| {
                widgets::source_badge(ui, source);
                if let Some(q) = &cx.player.quality {
                    ui.label(egui::RichText::new(q.label()).size(12.0).color(TEXT_DIM));
                }
            });
            if let Some(q) = &cx.player.quality {
                let (r, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
                widgets::quality_badge(ui, r.left_center(), q);
            }
            ui.add_space(14.0);

            // Next in queue.
            if let Some(next) = cx.player.upcoming.first().cloned() {
                widgets::card_frame().inner_margin(Margin::same(12)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Next in queue").font(theme::bold_font(15.0)));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Label::new(egui::RichText::new("Open queue").size(13.0).color(TEXT_DIM))
                                        .sense(Sense::click()),
                                )
                                .on_hover_cursor(CursorIcon::PointingHand)
                                .clicked()
                            {
                                cx.actions.push(Action::RightTab(RightTab::Queue));
                            }
                        });
                    });
                    ui.add_space(4.0);
                    queue_row(ui, cx, &next, Some(0), false);
                });
                ui.add_space(12.0);
            }

            // Lyrics preview.
            if let Some(lyrics) = cx
                .feed
                .lyrics
                .lyrics
                .as_ref()
                .filter(|_| cx.feed.lyrics.track_id == t.id)
                .filter(|l| !l.instrumental && !l.synced.is_empty())
            {
                let pos_ms = (cx.player.position_now() * 1000.0) as u64;
                let current = lyrics.line_at(pos_ms).unwrap_or(0);
                let from = current.saturating_sub(1);
                let fill = theme::mix(cx.tint, PANEL, 0.35);
                // Spotify-style: the current line in white, the others darker than the card.
                let dim = if theme::on_color(fill) == Color32::WHITE {
                    theme::with_alpha(Color32::WHITE, 110)
                } else {
                    theme::with_alpha(Color32::BLACK, 170)
                };
                let resp = egui::Frame::new()
                    .fill(fill)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new("Lyrics").font(theme::bold_font(15.0)));
                        ui.add_space(6.0);
                        for (i, line) in lyrics.synced.iter().enumerate().skip(from).take(4) {
                            let text = if line.text.trim().is_empty() {
                                "♪"
                            } else {
                                line.text.as_str()
                            };
                            let color = if i == current { Color32::WHITE } else { dim };
                            ui.add(
                                egui::Label::new(egui::RichText::new(text).font(theme::bold_font(17.0)).color(color))
                                    .wrap(),
                            );
                        }
                    })
                    .response;
                if ui
                    .interact(resp.rect, Id::new("np-lyrics-card"), Sense::click())
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .on_hover_text("Show lyrics")
                    .clicked()
                {
                    cx.actions.push(Action::RightTab(RightTab::Lyrics));
                }
            }
        });
}

/// Synced lyrics. `big` is used by the now playing view.
pub fn lyrics(ui: &mut Ui, cx: &mut Cx, big: bool) {
    let Some(t) = cx.player.current.as_ref() else {
        empty_state(ui, icon::MICROPHONE_STAGE, "Play something to see its lyrics");
        return;
    };
    let state = &cx.feed.lyrics;
    if state.track_id == t.id && state.off {
        empty_state(ui, icon::MICROPHONE_STAGE, "Lyrics are turned off in Settings");
        return;
    }
    if state.track_id != t.id || state.loading {
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(egui::RichText::new("Looking for lyrics…").color(TEXT_DIM));
        });
        return;
    }
    let Some(lyrics) = state.lyrics.as_ref() else {
        empty_state(ui, icon::MICROPHONE_STAGE, "No lyrics found for this song");
        return;
    };
    if lyrics.instrumental {
        empty_state(ui, icon::MUSIC_NOTES, "Instrumental");
        return;
    }
    let size = if big { 30.0 } else { 19.0 };
    let pos_ms = (cx.player.position_now() * 1000.0) as u64;
    let id = Id::new(("lyrics-scroll", big));
    if lyrics.synced.is_empty() {
        egui::ScrollArea::vertical()
            .id_salt(id)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Unsynced lyrics").small().color(TEXT_FAINT));
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(&lyrics.plain)
                        .font(theme::bold_font(size * 0.8))
                        .color(TEXT_DIM),
                );
                provider(ui, &lyrics.provider);
            });
        return;
    }
    let current = lyrics.line_at(pos_ms);
    let last_key = id.with("last");
    let last: Option<Option<usize>> = ui.data(|d| d.get_temp(last_key));
    let changed = last != Some(current);
    // Pause auto-scroll for a few seconds after the user scrolls manually.
    let user_scrolled = ui.rect_contains_pointer(ui.max_rect()) && ui.input(|i| i.smooth_scroll_delta.y != 0.0);
    let hold_key = id.with("hold");
    let now = ui.input(|i| i.time);
    if user_scrolled {
        ui.data_mut(|d| d.insert_temp(hold_key, now));
    }
    let held = ui.data(|d| d.get_temp::<f64>(hold_key)).is_some_and(|t| now - t < 4.0);

    egui::ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(if big { ui.available_height() * 0.3 } else { 8.0 });
            for (i, line) in lyrics.synced.iter().enumerate() {
                let state = match current {
                    Some(c) if i == c => 0,
                    Some(c) if i < c => -1,
                    _ => 1,
                };
                let color = match state {
                    0 => Color32::WHITE,
                    -1 => theme::with_alpha(TEXT_DIM, if big { 150 } else { 170 }),
                    _ => theme::with_alpha(TEXT_FAINT, if big { 200 } else { 255 }),
                };
                let text = if line.text.trim().is_empty() {
                    "♪"
                } else {
                    line.text.as_str()
                };
                let resp = ui.add(
                    egui::Label::new(egui::RichText::new(text).font(theme::bold_font(size)).color(color))
                        .wrap()
                        .sense(Sense::click()),
                );
                if resp.hovered() && state != 0 {
                    ui.painter().text(
                        resp.rect.left_top() - vec2(0.0, 2.0),
                        Align2::LEFT_BOTTOM,
                        theme::fmt_time(line.time_ms as f64 / 1000.0),
                        theme::font(10.0),
                        TEXT_FAINT,
                    );
                }
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    cx.actions
                        .push(Action::Cmd(Command::Seek(line.time_ms as f64 / 1000.0)));
                }
                if state == 0 && changed && !held {
                    resp.scroll_to_me(Some(Align::Center));
                }
                ui.add_space(if big { 14.0 } else { 8.0 });
            }
            provider(ui, &lyrics.provider);
            ui.add_space(if big { ui.available_height() * 0.5 } else { 40.0 });
        });
    if changed && !held {
        ui.data_mut(|d| d.insert_temp(last_key, current));
    }
}

fn provider(ui: &mut Ui, name: &str) {
    if !name.is_empty() {
        ui.add_space(16.0);
        ui.label(
            egui::RichText::new(format!("Lyrics from {name}"))
                .size(11.0)
                .color(TEXT_FAINT),
        );
    }
}

/// Download button for one song: a ring fills while it downloads, and once saved it shows the
/// file in its folder.
fn download_control(ui: &mut Ui, cx: &mut Cx, t: &Track, rect: Rect) {
    let resp = ui.interact(rect, Id::new(("download", &t.id)), Sense::click());
    let state = widgets::downloaded(cx.feed, &t.id);
    let hovered = resp.hovered();
    let center = rect.center();
    let (glyph, color, tip) = match state {
        widgets::Downloaded::Yes => (
            icon::ARROW_CIRCLE_DOWN,
            cx.accent,
            "Downloaded · show in folder".to_string(),
        ),
        widgets::Downloaded::Busy(p) => {
            // Progress ring around a small arrow.
            let radius = 10.0;
            let painter = ui.painter();
            painter.circle_stroke(center, radius, Stroke::new(2.0, HOVER));
            if p > 0.0 {
                let n = 40;
                let points: Vec<Pos2> = (0..=n)
                    .map(|k| {
                        let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * p * k as f32 / n as f32;
                        center + radius * vec2(a.cos(), a.sin())
                    })
                    .collect();
                painter.add(egui::Shape::line(points, Stroke::new(2.0, cx.accent)));
            }
            let tip = if p > 0.0 {
                format!("Downloading… {:.0}%", p * 100.0)
            } else {
                "Waiting to download…".to_string()
            };
            (icon::ARROW_DOWN, TEXT_DIM, tip)
        }
        widgets::Downloaded::No => (
            icon::DOWNLOAD_SIMPLE,
            if hovered { TEXT } else { TEXT_DIM },
            "Download".to_string(),
        ),
    };
    let size = if matches!(state, widgets::Downloaded::Busy(_)) {
        11.0
    } else {
        20.0
    };
    theme::paint_icon(ui.painter(), center, glyph, theme::icon_font(size), color);
    if resp
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(tip)
        .clicked()
    {
        match state {
            widgets::Downloaded::Yes => {
                if let Some(dir) = cx.feed.downloaded.get(&t.id).and_then(|f| f.parent()) {
                    cx.actions.push(Action::OpenUrl(dir.to_string_lossy().to_string()));
                }
            }
            widgets::Downloaded::Busy(_) => cx.actions.push(Action::Go(View::Downloads)),
            widgets::Downloaded::No => cx.actions.push(Action::Cmd(Command::Download(vec![t.clone()]))),
        }
    }
}

pub fn empty_state(ui: &mut Ui, glyph: &str, text: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(glyph)
                .family(theme::icons())
                .size(42.0)
                .color(TEXT_FAINT),
        );
        ui.add_space(8.0);
        ui.label(egui::RichText::new(text).color(TEXT_DIM));
    });
}

fn queue(ui: &mut Ui, cx: &mut Cx) {
    if let Some(t) = cx.player.current.clone() {
        ui.label(
            egui::RichText::new("Now playing")
                .font(theme::bold_font(13.0))
                .color(TEXT_DIM),
        );
        ui.add_space(4.0);
        queue_row(ui, cx, &t, None, true);
        ui.add_space(10.0);
    }
    let upcoming = &cx.player.upcoming;
    if upcoming.is_empty() {
        empty_state(ui, icon::QUEUE, "Nothing queued");
        return;
    }
    let n = upcoming.len();
    let queued = cx.player.up_next_len.min(n);
    let drag_id = Id::new("queue-drag");
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // Songs you queued play first, then the rest of the playlist; each part has its own
        // Clear, and songs can be dragged within their part.
        let parts = [
            (0..queued, "Queued by you".to_string(), Command::ClearQueued),
            (
                queued..n,
                format!("Next from: {}", cx.player.context),
                Command::ClearUpcoming,
            ),
        ];
        let mut rows: Vec<(usize, Rect)> = Vec::with_capacity(n);
        for (range, title, clear) in parts {
            if range.is_empty() {
                continue;
            }
            let count = range.len();
            queue_part_header(ui, cx, &title, count, clear);
            for i in range {
                let t = cx.player.upcoming[i].clone();
                let rect = queue_row(ui, cx, &t, Some(i), false);
                rows.push((i, rect));
            }
            ui.add_space(10.0);
        }
        queue_drag(ui, cx, drag_id, &rows, queued);
    });
}

/// "Queued by you" / "Next from …" with a Clear that asks first.
fn queue_part_header(ui: &mut Ui, cx: &mut Cx, title: &str, count: usize, clear: Command) {
    let confirm_id = Id::new(("queue-clear", title.starts_with("Queued")));
    let asking: bool = ui.data(|d| d.get_temp(confirm_id)).unwrap_or(false);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).font(theme::bold_font(13.0)).color(TEXT_DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let link = |ui: &mut Ui, text: &str, color: Color32| {
                ui.add(egui::Label::new(egui::RichText::new(text).small().color(color)).sense(Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
            };
            if asking {
                if link(ui, "Keep", TEXT_DIM) {
                    ui.data_mut(|d| d.insert_temp(confirm_id, false));
                }
                if link(ui, &format!("Remove {count}?"), theme::DANGER) {
                    ui.data_mut(|d| d.insert_temp(confirm_id, false));
                    cx.actions.push(Action::Cmd(clear));
                }
            } else if link(ui, "Clear", TEXT_FAINT) {
                ui.data_mut(|d| d.insert_temp(confirm_id, true));
            }
        });
    });
    ui.add_space(4.0);
}

/// Dragging a queue row moves it within its part (shown with a line where it will go).
fn queue_drag(ui: &mut Ui, cx: &mut Cx, drag_id: Id, rows: &[(usize, Rect)], queued: usize) {
    let Some(from) = ui.data(|d| d.get_temp::<usize>(drag_id)) else {
        return;
    };
    // A drag that ended elsewhere (the panel was closed meanwhile) is over.
    if !ui.input(|i| i.pointer.any_down() || i.pointer.any_released()) {
        ui.data_mut(|d| d.remove::<usize>(drag_id));
        return;
    }
    let Some(pointer) = ui.ctx().pointer_interact_pos() else {
        return;
    };
    let same_part = |i: usize| (i < queued) == (from < queued);
    // The row under the pointer in the same part decides the place.
    let target = rows
        .iter()
        .filter(|(i, _)| same_part(*i))
        .min_by(|(_, a), (_, b)| {
            (a.center().y - pointer.y)
                .abs()
                .total_cmp(&(b.center().y - pointer.y).abs())
        })
        .copied();
    let Some((to, rect)) = target else { return };
    let y = if to > from { rect.bottom() } else { rect.top() };
    ui.painter().hline(rect.x_range(), y, Stroke::new(2.0, cx.accent));
    if ui.input(|i| i.pointer.any_released()) {
        ui.data_mut(|d| d.remove::<usize>(drag_id));
        if to != from {
            if let Some(t) = cx.player.upcoming.get(from) {
                cx.actions.push(Action::Cmd(Command::MoveUpcoming {
                    from,
                    to,
                    id: t.id.clone(),
                }));
            }
        }
    }
}

fn queue_row(ui: &mut Ui, cx: &mut Cx, t: &crate::model::Track, index: Option<usize>, current: bool) -> Rect {
    let sense = if index.is_some() {
        Sense::click_and_drag()
    } else {
        Sense::click()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), sense);
    if let (Some(i), true) = (index, resp.drag_started()) {
        ui.data_mut(|d| d.insert_temp(Id::new("queue-drag"), i));
    }
    if resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    }
    widgets::fade_fill(ui, resp.id, rect, 10, resp.hovered(), HOVER);
    let art = Rect::from_min_size(rect.min + vec2(4.0, 6.0), vec2(40.0, 40.0));
    let src = t.art.clone().or_else(|| {
        if current {
            cx.player.via.as_ref().and_then(|v| v.art.clone())
        } else {
            None
        }
    });
    widgets::cover(ui, cx.art, src.as_deref(), art, 5, widgets::track_fallback(t));
    let x = art.right() + 10.0;
    let w = rect.right() - x - 34.0;
    text_trunc(
        ui,
        Pos2::new(x, rect.top() + 8.0),
        &t.title,
        theme::font(14.0),
        if current { cx.accent } else { TEXT },
        w,
    );
    text_trunc(
        ui,
        Pos2::new(x, rect.top() + 28.0),
        &t.artist,
        theme::font(12.0),
        TEXT_DIM,
        w,
    );
    if let Some(i) = index {
        let x_rect = Rect::from_center_size(Pos2::new(rect.right() - 16.0, rect.center().y), vec2(24.0, 24.0));
        let xr = ui.interact(x_rect, Id::new(("q-remove", i)), Sense::click());
        if resp.hovered() || xr.hovered() {
            ui.painter().text(
                x_rect.center(),
                Align2::CENTER_CENTER,
                icon::X,
                theme::icon_font(14.0),
                if xr.hovered() { TEXT } else { TEXT_DIM },
            );
        }
        if xr.clicked() {
            cx.actions.push(Action::Cmd(Command::RemoveUpcoming(i, t.id.clone())));
        } else if resp.double_clicked() || resp.clicked() {
            cx.actions.push(Action::Cmd(Command::JumpTo(i, t.id.clone())));
        }
    }
    let track = t.clone();
    resp.context_menu(|ui| widgets::track_menu(ui, cx, &track, None));
    rect
}
