//! Reusable painted widgets: covers, buttons, seek bar, tiles and the virtualized track table.

use egui::text::{LayoutJob, TextWrapping};
use egui::{
    vec2, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Mesh, Pos2, Rect, Response, Sense, Shape, Stroke, Ui,
};
use egui_phosphor::regular as icon;

use super::art::{ArtCache, MEDIUM, THUMB};
use super::theme::{self, *};
use super::{Action, Cx, View};
use crate::model::{Playlist, PlaylistKind, Source, Track};
use crate::service::{Command, DownloadState, Feed, PlayStatus};

/// Draws `text` on one line, cut with an ellipsis at `max_w`. Returns the used rect.
pub fn text_trunc(ui: &Ui, pos: Pos2, text: &str, font: FontId, color: Color32, max_w: f32) -> Rect {
    let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = TextWrapping::truncate_at_width(max_w.max(8.0));
    let galley = ui.painter().layout_job(job);
    let rect = Rect::from_min_size(pos, galley.size());
    ui.painter().galley(pos, galley, color);
    rect
}

/// Paints `color` behind `rect`, fading it in and out as `on` changes (hover highlights).
pub fn fade_fill(ui: &Ui, id: Id, rect: Rect, radius: u8, on: bool, color: Color32) {
    let t = ui.ctx().animate_bool_with_time(id.with("fade"), on, 0.14);
    if t > 0.0 {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(radius), color.gamma_multiply(t));
    }
}

/// Vertical gradient, `top` colour fading to `bottom`.
pub fn gradient(ui: &Ui, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    ui.painter().add(Shape::mesh(mesh));
}

/// Vertical gradient inside a rounded rectangle.
pub fn rounded_gradient(ui: &Ui, rect: Rect, top: Color32, bottom: Color32, radius: u8) {
    let r = (radius as f32).min(rect.height() / 2.0);
    let p = ui.painter();
    p.rect_filled(
        Rect::from_min_size(rect.min, vec2(rect.width(), r)),
        CornerRadius {
            nw: radius,
            ne: radius,
            sw: 0,
            se: 0,
        },
        top,
    );
    p.rect_filled(
        Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - r), rect.max),
        CornerRadius {
            nw: 0,
            ne: 0,
            sw: radius,
            se: radius,
        },
        bottom,
    );
    gradient(
        ui,
        Rect::from_min_max(rect.min + vec2(0.0, r), rect.max - vec2(0.0, r)),
        top,
        bottom,
    );
}

/// Placeholder artwork: a soft gradient with an icon.
pub fn placeholder(ui: &Ui, rect: Rect, base: Color32, glyph: &str, radius: u8) {
    let top = theme::mix(base, Color32::BLACK, 0.35);
    let bottom = theme::mix(base, Color32::BLACK, 0.7);
    rounded_gradient(ui, rect, top, bottom, radius);
    theme::paint_icon(
        ui.painter(),
        rect.center(),
        glyph,
        theme::icon_font(rect.height() * 0.38),
        theme::with_alpha(Color32::WHITE, 200),
    );
}

/// Cover art (or a placeholder) in `rect`.
pub fn cover(ui: &Ui, art: &mut ArtCache, src: Option<&str>, rect: Rect, radius: u8, fallback: (Color32, &str)) {
    let size = if rect.width() <= 64.0 {
        THUMB
    } else if rect.width() <= 260.0 {
        MEDIUM
    } else {
        super::art::LARGE
    };
    match art.get(src, size) {
        Some(tex) => {
            egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), rect.size()))
                .corner_radius(CornerRadius::same(radius))
                .paint_at(ui, rect);
        }
        None => placeholder(ui, rect, fallback.0, fallback.1, radius),
    }
}

/// Round artwork (artists).
pub fn cover_round(ui: &Ui, art: &mut ArtCache, src: Option<&str>, rect: Rect, fallback: (Color32, &str)) {
    let size = if rect.width() <= 64.0 { THUMB } else { MEDIUM };
    let radius = (rect.width() / 2.0).min(255.0) as u8;
    match art.get(src, size) {
        Some(tex) => {
            egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), rect.size()))
                .corner_radius(CornerRadius::same(radius))
                .paint_at(ui, rect);
        }
        None => {
            ui.painter()
                .circle_filled(rect.center(), rect.width() / 2.0, fallback.0);
            theme::paint_icon(
                ui.painter(),
                rect.center(),
                fallback.1,
                theme::icon_font(rect.height() * 0.38),
                theme::with_alpha(Color32::WHITE, 200),
            );
        }
    }
}

pub fn artist_fallback() -> (Color32, &'static str) {
    (Color32::from_rgb(0x33, 0x33, 0x33), icon::USER)
}

pub fn track_fallback(t: &Track) -> (Color32, &'static str) {
    (theme::mix(source_color(t.source), PANEL, 0.4), icon::MUSIC_NOTE)
}

pub fn playlist_fallback(p: &Playlist) -> (Color32, &'static str) {
    match p.kind {
        PlaylistKind::Liked => (Color32::from_rgb(0x50, 0x38, 0xa0), icon::HEART),
        PlaylistKind::SpotifyLiked => (source_color(Source::Spotify), icon::HEART),
        PlaylistKind::SoundCloudLikes => (source_color(Source::SoundCloud), icon::HEART),
        k => (
            k.source()
                .map(source_color)
                .unwrap_or(Color32::from_rgb(0x55, 0x5a, 0x78)),
            icon::MUSIC_NOTES,
        ),
    }
}

/// Art shown for a playlist: Liked playlists get a gradient heart tile.
pub fn playlist_cover(ui: &Ui, art: &mut ArtCache, p: &Playlist, rect: Rect, radius: u8) {
    if matches!(
        p.kind,
        PlaylistKind::Liked | PlaylistKind::SpotifyLiked | PlaylistKind::SoundCloudLikes
    ) {
        let (base, glyph) = playlist_fallback(p);
        let top = theme::mix(base, Color32::WHITE, 0.12);
        let bottom = theme::mix(base, Color32::BLACK, 0.45);
        rounded_gradient(ui, rect, top, bottom, radius);
        theme::paint_icon(
            ui.painter(),
            rect.center(),
            glyph,
            theme::fill_icon_font(rect.height() * 0.42),
            Color32::WHITE,
        );
        return;
    }
    cover(ui, art, p.art.as_deref(), rect, radius, playlist_fallback(p));
}

fn with_tooltip(resp: Response, tooltip: &str) -> Response {
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if tooltip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tooltip)
    }
}

/// A flat icon button (a `size + 12` square) whose icon brightens on hover.
pub fn icon_button(ui: &mut Ui, glyph: &str, size: f32, color: Color32, tooltip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size + 12.0, size + 12.0), Sense::click());
    let c = if resp.hovered() {
        theme::mix(color, Color32::WHITE, 0.6)
    } else {
        color
    };
    theme::paint_icon(ui.painter(), rect.center(), glyph, theme::icon_font(size), c);
    with_tooltip(resp, tooltip)
}

/// Size of the player dock controls.
pub const CONTROL: f32 = 32.0;

/// A row of options in one rounded box (tabs); returns the clicked option.
pub fn segmented<T: Copy + PartialEq>(ui: &mut Ui, options: &[(T, &str)], selected: T) -> Option<T> {
    let font = theme::font(13.0);
    let widths: Vec<f32> = options
        .iter()
        .map(|(_, label)| {
            ui.painter()
                .layout_no_wrap(label.to_string(), font.clone(), TEXT)
                .size()
                .x
                + 22.0
        })
        .collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0;
    let (rect, _) = ui.allocate_exact_size(vec2(total, 34.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(10), CARD);
    let mut x = rect.left() + 3.0;
    let mut clicked = None;
    for ((value, label), w) in options.iter().zip(widths) {
        let seg = Rect::from_min_size(Pos2::new(x, rect.top() + 3.0), vec2(w, 28.0));
        let resp = ui
            .interact(seg, Id::new(("segmented", *label)), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        let active = *value == selected;
        if active {
            ui.painter().rect_filled(seg, CornerRadius::same(8), SELECTED);
        } else {
            fade_fill(ui, resp.id, seg, 8, resp.hovered(), HOVER);
        }
        ui.painter().text(
            seg.center(),
            Align2::CENTER_CENTER,
            *label,
            font.clone(),
            if active || resp.hovered() { TEXT } else { TEXT_DIM },
        );
        if resp.clicked() {
            clicked = Some(*value);
        }
        x += w;
    }
    clicked
}

/// Labelled button with an icon ("▶ Play"). `primary` uses the accent.
pub fn action_button(ui: &mut Ui, glyph: &str, label: &str, primary: bool, accent: Color32) -> Response {
    let fg = if primary { theme::on_color(accent) } else { TEXT };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), theme::bold_font(14.0), fg);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 58.0, 42.0), Sense::click());
    let hovered = resp.hovered();
    let fill = match (primary, hovered) {
        (true, false) => accent,
        (true, true) => theme::mix(accent, Color32::WHITE, 0.15),
        (false, false) => CARD,
        (false, true) => HOVER,
    };
    ui.painter().rect_filled(rect, CornerRadius::same(12), fill);
    let font = if primary {
        theme::fill_icon_font(17.0)
    } else {
        theme::icon_font(17.0)
    };
    theme::paint_icon(
        ui.painter(),
        Pos2::new(rect.left() + 26.0, rect.center().y),
        glyph,
        font,
        fg,
    );
    ui.painter().galley(
        Pos2::new(rect.left() + 42.0, rect.center().y - galley.size().y / 2.0),
        galley,
        fg,
    );
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Where a SoundCloud song stands with downloads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Downloaded {
    No,
    /// Queued (0) or downloading.
    Busy(f32),
    Yes,
}

pub fn downloaded(feed: &Feed, track_id: &str) -> Downloaded {
    if feed.downloaded.contains_key(track_id) {
        return Downloaded::Yes;
    }
    match feed.downloads.iter().find(|d| d.track.id == track_id).map(|d| &d.state) {
        Some(DownloadState::Queued) => Downloaded::Busy(0.0),
        Some(DownloadState::Running(p)) => Downloaded::Busy(*p),
        _ => Downloaded::No,
    }
}

/// Download button for the songs among `tracks` that aren't local files; nothing when all are.
pub fn download_button(ui: &mut Ui, cx: &mut Cx, tracks: &[&Track], size: f32) {
    let remote: Vec<&Track> = tracks.iter().copied().filter(|t| t.source != Source::Local).collect();
    if remote.is_empty() {
        return;
    }
    let active: std::collections::HashSet<&str> = cx
        .feed
        .downloads
        .iter()
        .filter(|d| d.state.active())
        .map(|d| d.track.id.as_str())
        .collect();
    let total = remote.len();
    let done = remote.iter().filter(|t| cx.feed.downloaded.contains_key(&t.id)).count();
    let busy = remote.iter().filter(|t| active.contains(t.id.as_str())).count();
    let (glyph, color, tip) = if done == total {
        let tip = if total == 1 {
            "Downloaded".to_string()
        } else {
            format!("All {total} songs are downloaded")
        };
        (icon::CHECK_CIRCLE, cx.accent, tip)
    } else if busy > 0 {
        (
            icon::DOWNLOAD_SIMPLE,
            cx.accent,
            format!("Downloading… {done} of {total} saved"),
        )
    } else {
        let tip = match (total, done) {
            (1, _) => "Download".to_string(),
            (n, 0) => format!("Download {n} songs"),
            (n, d) => format!("Download the other {} songs ({d} of {n} saved)", n - d),
        };
        (icon::DOWNLOAD_SIMPLE, TEXT_DIM, tip)
    };
    if icon_button(ui, glyph, size, color, &tip).clicked() {
        if done == total || busy > 0 {
            cx.actions.push(Action::Go(View::Downloads));
        } else {
            let todo = remote
                .into_iter()
                .filter(|t| !cx.feed.downloaded.contains_key(&t.id))
                .cloned()
                .collect();
            cx.actions.push(Action::Cmd(Command::Download(todo)));
        }
    }
}

/// Whether the yt-dlp `program` runs: its version, or why not. Asks the service the first time
/// (and whenever the program changes); `None` until it has answered.
pub fn ytdlp_status(ui: &Ui, cx: &mut Cx, program: &str) -> Option<Result<String, String>> {
    let program = program.trim().to_string();
    if let Some((checked, result)) = &cx.feed.ytdlp {
        if *checked == program {
            return Some(result.clone());
        }
    }
    let id = Id::new("ytdlp-check");
    let asked: Option<String> = ui.data(|d| d.get_temp(id));
    if asked.as_deref() != Some(program.as_str()) {
        ui.data_mut(|d| d.insert_temp(id, program.clone()));
        cx.actions.push(Action::Cmd(Command::CheckYtDlp(program)));
    }
    None
}

/// Width [`pill`] will take for `text`.
pub fn pill_width(ui: &Ui, text: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_string(), theme::bold_font(14.0), TEXT)
        .size()
        .x
        + 28.0
}

/// Rounded "pill" button.
pub fn pill(ui: &mut Ui, text: &str, fill: Color32, fg: Color32) -> Response {
    let font = theme::bold_font(14.0);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, fg);
    let size = vec2(galley.size().x + 28.0, 34.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let fill = if resp.hovered() {
        theme::mix(fill, Color32::WHITE, 0.12)
    } else {
        fill
    };
    ui.painter().rect_filled(rect, CornerRadius::same(17), fill);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, fg);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Thin seek/volume bar. Returns the new fraction while dragging or on click.
pub fn bar(ui: &mut Ui, id: Id, width: f32, fraction: f32, accent: Color32) -> (Response, Option<f32>) {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 16.0), Sense::click_and_drag());
    let _ = id;
    let active = resp.hovered() || resp.dragged();
    let track = Rect::from_center_size(rect.center(), vec2(rect.width(), 4.0));
    let mut f = fraction.clamp(0.0, 1.0);
    let mut changed = None;
    if let Some(p) = resp.interact_pointer_pos() {
        if resp.dragged() || resp.clicked() {
            f = ((p.x - track.left()) / track.width()).clamp(0.0, 1.0);
            changed = Some(f);
        }
    }
    let painter = ui.painter();
    painter.rect_filled(track, CornerRadius::same(2), Color32::from_rgb(0x4d, 0x4d, 0x4d));
    let filled = Rect::from_min_max(track.min, Pos2::new(track.left() + track.width() * f, track.max.y));
    painter.rect_filled(filled, CornerRadius::same(2), if active { accent } else { TEXT });
    if active {
        painter.circle_filled(Pos2::new(filled.right(), track.center().y), 6.0, Color32::WHITE);
    }
    (resp.on_hover_cursor(CursorIcon::PointingHand), changed)
}

/// Section heading with a leading icon.
pub fn heading_icon(ui: &mut Ui, glyph: &str, color: Color32, text: &str) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(glyph)
                .family(theme::icons())
                .size(20.0)
                .color(color),
        );
        ui.label(egui::RichText::new(text).font(theme::bold_font(18.0)).color(TEXT));
    });
    ui.add_space(6.0);
}

/// Section heading.
pub fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(text).font(theme::bold_font(18.0)).color(TEXT));
    ui.add_space(6.0);
}

/// Square tile with art, title and subtitle (playlists, albums). Returns (clicked, play clicked).
pub fn tile(
    ui: &mut Ui,
    width: f32,
    title: &str,
    subtitle: &str,
    accent: Color32,
    draw_art: impl FnOnce(&Ui, Rect),
) -> (Response, bool) {
    let height = width + 58.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    let hovered = resp.hovered();
    fade_fill(ui, resp.id, rect.expand(8.0), RADIUS + 2, hovered, CARD);
    let art_rect = Rect::from_min_size(rect.min, vec2(width, width));
    draw_art(ui, art_rect);
    text_trunc(
        ui,
        art_rect.left_bottom() + vec2(0.0, 8.0),
        title,
        theme::bold_font(14.0),
        TEXT,
        width,
    );
    text_trunc(
        ui,
        art_rect.left_bottom() + vec2(0.0, 28.0),
        subtitle,
        theme::font(12.5),
        TEXT_DIM,
        width,
    );

    let mut play = false;
    let t = ui.ctx().animate_bool_with_time(resp.id.with("play"), hovered, 0.15);
    if t > 0.0 {
        let btn = Rect::from_min_size(
            art_rect.right_bottom() - vec2(52.0, 52.0 - 6.0 * (1.0 - t)),
            vec2(40.0, 40.0),
        );
        let over = ui.rect_contains_pointer(btn);
        let alpha = (255.0 * t) as u8;
        ui.painter().add(
            egui::epaint::Shadow {
                offset: [0, 4],
                blur: 14,
                spread: 0,
                color: Color32::from_black_alpha((110.0 * t) as u8),
            }
            .as_shape(btn, CornerRadius::same(12)),
        );
        let fill = if over {
            theme::mix(accent, Color32::WHITE, 0.15)
        } else {
            accent
        };
        ui.painter()
            .rect_filled(btn, CornerRadius::same(12), theme::with_alpha(fill, alpha));
        theme::paint_icon(
            ui.painter(),
            btn.center() + vec2(1.0, 0.0),
            egui_phosphor::fill::PLAY,
            theme::fill_icon_font(17.0),
            theme::with_alpha(theme::on_color(accent), alpha),
        );
        if over && resp.clicked() {
            play = true;
        }
    }
    (resp.on_hover_cursor(CursorIcon::PointingHand), play)
}

/// Lays out tiles in a responsive grid. `draw(ui, index, tile_width)`.
pub fn grid(ui: &mut Ui, count: usize, min_w: f32, mut draw: impl FnMut(&mut Ui, usize, f32)) {
    let gap = 20.0;
    let avail = ui.available_width();
    let cols = (((avail + gap) / (min_w + gap)).floor() as usize).max(1);
    let w = ((avail - gap * (cols as f32 - 1.0)) / cols as f32).floor();
    let mut i = 0;
    // Rows are the same height: once one is drawn, rows that are scrolled out of view only take
    // their space. Drawing them would load every cover in a big library each frame, more than
    // the art cache holds, so covers would be dropped and loaded again forever.
    let mut row_h: Option<f32> = None;
    while i < count {
        if let Some(h) = row_h {
            let row = Rect::from_min_size(ui.cursor().min, vec2(avail, h));
            if !ui.is_rect_visible(row) {
                ui.add_space(h + ui.spacing().item_spacing.y);
                i += cols;
                ui.add_space(14.0);
                continue;
            }
        }
        let drawn = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for _ in 0..cols {
                if i >= count {
                    break;
                }
                draw(ui, i, w);
                i += 1;
            }
        });
        row_h.get_or_insert(drawn.response.rect.height());
        ui.add_space(14.0);
    }
}

/// Whether playback started from this list (`name`; `has` tells whether a song is in it).
/// Lists can share a name, so the playing song (or, while a song you queued plays, the first
/// song of the list it started from) must be in it too.
pub fn plays_from(cx: &Cx, name: &str, has: impl Fn(&str) -> bool) -> bool {
    let Some(current) = &cx.player.current else {
        return false;
    };
    !cx.player.context.is_empty()
        && cx.player.context == name
        && (has(&current.id) || cx.player.context_first.as_deref().is_some_and(&has))
}

/// Options for [`track_table`].
pub struct TableOpts<'a> {
    pub id: &'a str,
    pub context: &'a str,
    /// The playlist being shown, enables "Remove from playlist".
    pub playlist: Option<&'a Playlist>,
    pub show_album: bool,
    pub show_header: bool,
}

pub const ROW_H: f32 = 56.0;

/// A virtualized track table. Must be called inside a `ScrollArea::show_viewport`,
/// `viewport` being the visible content rect and `origin_y` the screen y of the content top.
pub fn track_table(ui: &mut Ui, cx: &mut Cx, tracks: &[&Track], opts: &TableOpts, viewport: Rect, origin_y: f32) {
    let width = ui.available_width();
    let show_album = opts.show_album && width > 720.0;
    let num_w = 40.0;
    let dur_w = 64.0;
    let icons_w = 64.0;
    let flex = width - num_w - dur_w - icons_w - 16.0;
    let title_w = if show_album { flex * 0.58 } else { flex };
    let album_x = num_w + title_w + 16.0;

    if opts.show_header {
        let list = list_key(opts.context, tracks);
        let picked = ui.data_mut(|d| {
            let sel = d.get_temp_mut_or_default::<Selection>(Id::new(("table-sel", opts.id)));
            if sel.list == list {
                sel.picked.iter().filter(|p| **p).count()
            } else {
                0
            }
        });
        let (hrect, _) = ui.allocate_exact_size(vec2(width, 32.0), Sense::hover());
        let f = theme::font(12.0);
        let p = ui.painter();
        p.text(
            hrect.left_center() + vec2(num_w * 0.5, 0.0),
            Align2::CENTER_CENTER,
            "#",
            f.clone(),
            TEXT_FAINT,
        );
        p.text(
            hrect.left_center() + vec2(num_w + 8.0, 0.0),
            Align2::LEFT_CENTER,
            "Title",
            f.clone(),
            TEXT_FAINT,
        );
        if show_album {
            p.text(
                hrect.left_center() + vec2(album_x, 0.0),
                Align2::LEFT_CENTER,
                "Album",
                f.clone(),
                TEXT_FAINT,
            );
        }
        if picked > 1 {
            p.text(
                hrect.right_center() - vec2(dur_w + icons_w, 0.0),
                Align2::RIGHT_CENTER,
                format!("{picked} selected · Ctrl+C to copy"),
                f.clone(),
                TEXT_DIM,
            );
        }
        p.text(
            hrect.right_center() - vec2(20.0, 0.0),
            Align2::RIGHT_CENTER,
            icon::CLOCK,
            theme::icon_font(13.0),
            TEXT_FAINT,
        );
        p.line_segment(
            [
                hrect.left_bottom() + vec2(0.0, -1.0),
                hrect.right_bottom() + vec2(0.0, -1.0),
            ],
            Stroke::new(1.0, theme::with_alpha(Color32::WHITE, 18)),
        );
        ui.add_space(6.0);
    }

    let n = tracks.len();
    let (rect, _) = ui.allocate_exact_size(vec2(width, n as f32 * ROW_H), Sense::hover());
    let offset = rect.top() - origin_y;
    let first = (((viewport.min.y - offset) / ROW_H).floor().max(0.0)) as usize;
    let last = ((((viewport.max.y - offset) / ROW_H).ceil()).max(0.0) as usize).min(n);

    let current_id = cx.player.current.as_ref().map(|t| t.id.as_str());
    let playing = cx.player.status == PlayStatus::Playing;
    let sel_id = Id::new(("table-sel", opts.id));
    let list = list_key(opts.context, tracks);
    let selected: Vec<bool> = ui.data_mut(|d| {
        let sel = d.get_temp_mut_or_default::<Selection>(sel_id);
        if sel.list != list {
            *sel = Selection {
                list,
                ..Default::default()
            };
        }
        (first..last).map(|i| sel.has(i)).collect()
    });
    if is_active_table(ui, opts.id) {
        table_keys(ui, cx, tracks, opts, sel_id);
    }

    for i in first..last {
        let t = tracks[i];
        let row = Rect::from_min_size(rect.min + vec2(0.0, i as f32 * ROW_H), vec2(width, ROW_H));
        let resp = ui.interact(row, Id::new((opts.id, i)), Sense::click());
        let is_current = current_id == Some(t.id.as_str());
        let hovered = ui.rect_contains_pointer(row) || resp.context_menu_opened();
        if selected[i - first] {
            ui.painter().rect_filled(row, CornerRadius::same(10), SELECTED);
        } else {
            if is_current {
                ui.painter()
                    .rect_filled(row, CornerRadius::same(10), theme::with_alpha(cx.accent, 14));
            }
            fade_fill(ui, resp.id, row, 10, hovered, HOVER);
        }

        // Number / play icon / equalizer.
        let num_center = row.left_center() + vec2(num_w * 0.5, 0.0);
        if hovered {
            ui.painter().text(
                num_center,
                Align2::CENTER_CENTER,
                if is_current && playing {
                    egui_phosphor::fill::PAUSE
                } else {
                    egui_phosphor::fill::PLAY
                },
                theme::fill_icon_font(15.0),
                TEXT,
            );
        } else if is_current {
            draw_eq(ui, num_center, cx.accent, playing);
        } else {
            ui.painter().text(
                num_center,
                Align2::CENTER_CENTER,
                (i + 1).to_string(),
                theme::font(13.0),
                TEXT_DIM,
            );
        }

        // Art + title + artist.
        let art_rect = Rect::from_min_size(row.min + vec2(num_w + 8.0, (ROW_H - 40.0) / 2.0), vec2(40.0, 40.0));
        cover(ui, cx.art, t.art.as_deref(), art_rect, 6, track_fallback(t));
        let tx = art_rect.right() + 12.0;
        let tw = title_w - 60.0;
        let title_color = if is_current { cx.accent } else { TEXT };
        text_trunc(
            ui,
            Pos2::new(tx, row.top() + 9.0),
            &t.title,
            theme::font(14.5),
            title_color,
            tw,
        );
        // Downloaded (or downloading) songs get a small arrow before the artist.
        let mut artist_x = tx;
        if t.source != Source::Local {
            let mark = match downloaded(cx.feed, &t.id) {
                Downloaded::Yes => Some(cx.accent),
                Downloaded::Busy(_) => Some(TEXT_FAINT),
                Downloaded::No => None,
            };
            if let Some(color) = mark {
                theme::paint_icon(
                    ui.painter(),
                    Pos2::new(tx + 6.0, row.top() + 38.0),
                    icon::ARROW_CIRCLE_DOWN,
                    theme::icon_font(13.0),
                    color,
                );
                artist_x += 17.0;
            }
        }
        let artist = link_text(
            ui,
            Id::new((opts.id, i, "artist")),
            Pos2::new(artist_x, row.top() + 30.0),
            &t.artist,
            theme::font(12.5),
            if hovered { TEXT } else { TEXT_DIM },
            tw - (artist_x - tx),
        );
        if artist.clicked() {
            cx.actions.push(artist_action(cx.lib, &t.artist));
        }
        if show_album {
            text_trunc(
                ui,
                Pos2::new(row.left() + album_x, row.center().y - 8.0),
                &t.album,
                theme::font(13.0),
                TEXT_DIM,
                width - album_x - dur_w - icons_w - 8.0,
            );
        }

        // Source badge, like button, duration.
        let src_pos = Pos2::new(row.right() - dur_w - icons_w + 14.0, row.center().y);
        ui.painter().text(
            src_pos,
            Align2::CENTER_CENTER,
            theme::source_icon(t.source),
            theme::icon_font(15.0),
            theme::with_alpha(source_color(t.source), if hovered { 255 } else { 170 }),
        );
        let liked = cx.lib.is_liked(&t.id);
        let heart_rect =
            Rect::from_center_size(Pos2::new(row.right() - dur_w - 18.0, row.center().y), vec2(26.0, 26.0));
        let heart = ui.interact(heart_rect, Id::new((opts.id, i, "like")), Sense::click());
        if liked || hovered {
            let (glyph, f, c) = if liked {
                (egui_phosphor::fill::HEART, theme::fill_icon_font(16.0), cx.accent)
            } else {
                (
                    icon::HEART,
                    theme::icon_font(16.0),
                    if heart.hovered() { TEXT } else { TEXT_DIM },
                )
            };
            ui.painter()
                .text(heart_rect.center(), Align2::CENTER_CENTER, glyph, f, c);
        }
        if heart.clicked() {
            cx.actions.push(Action::Cmd(Command::ToggleLike(t.clone())));
        }
        let dur = if t.duration_ms > 0 {
            theme::fmt_time(t.duration_secs())
        } else {
            "–".into()
        };
        ui.painter().text(
            Pos2::new(row.right() - 18.0, row.center().y),
            Align2::RIGHT_CENTER,
            dur,
            theme::font(13.0),
            TEXT_DIM,
        );

        // Interactions.
        if resp.clicked() && !heart.clicked() && !artist.clicked() {
            let num_rect = Rect::from_min_size(row.min, vec2(num_w, ROW_H));
            if ui.rect_contains_pointer(num_rect) {
                if is_current {
                    cx.actions.push(Action::Cmd(Command::TogglePause));
                } else {
                    play_from(cx, tracks, i, opts.context);
                }
            } else {
                let m = ui.input(|i| i.modifiers);
                ui.data_mut(|d| {
                    d.get_temp_mut_or_default::<Selection>(sel_id)
                        .click(i, n, m.command, m.shift)
                });
                make_active_table(ui, opts.id);
            }
        }
        if resp.double_clicked() {
            play_from(cx, tracks, i, opts.context);
        }
        // Right-clicking a row outside the selection picks just that row.
        if resp.secondary_clicked() && !selected[i - first] {
            ui.data_mut(|d| d.get_temp_mut_or_default::<Selection>(sel_id).click(i, n, false, false));
            make_active_table(ui, opts.id);
        }
        resp.context_menu(|ui| {
            let rows = ui.data(|d| d.get_temp::<Selection>(sel_id)).unwrap_or_default().rows(n);
            if rows.len() > 1 && rows.contains(&i) {
                selection_menu(ui, cx, tracks, &rows, opts.playlist);
            } else {
                track_menu(ui, cx, t, Some((opts.playlist, i)));
            }
        });
    }
}

// ------------------------------------------------------------------ selecting and copying songs

/// Rows picked in a track table: click, Ctrl-click, Shift-click or Ctrl+A.
#[derive(Clone, Default)]
struct Selection {
    /// Picked rows; empty until something is picked.
    picked: Vec<bool>,
    anchor: usize,
    /// The list the rows belong to: another (or a changed) list drops them.
    list: u64,
}

impl Selection {
    fn has(&self, i: usize) -> bool {
        self.picked.get(i).copied().unwrap_or(false)
    }

    /// The picked rows, in order.
    fn rows(&self, n: usize) -> Vec<usize> {
        (0..n.min(self.picked.len())).filter(|i| self.picked[*i]).collect()
    }

    /// A click on row `i` of `n`: picks it alone, or with `toggle` (Ctrl) adds or drops it, or
    /// with `range` (Shift) picks everything from the last clicked row to it.
    fn click(&mut self, i: usize, n: usize, toggle: bool, range: bool) {
        self.picked.resize(n, false);
        if i >= n {
            return;
        }
        if range {
            let (a, b) = (self.anchor.min(i), self.anchor.max(i).min(n - 1));
            if !toggle {
                self.picked.fill(false);
            }
            self.picked[a..=b].fill(true);
            return;
        }
        if toggle {
            self.picked[i] = !self.picked[i];
        } else {
            self.picked.fill(false);
            self.picked[i] = true;
        }
        self.anchor = i;
    }

    fn all(&mut self, n: usize) {
        self.picked = vec![true; n];
    }
}

/// Tells lists apart cheaply: what they are and their first and last songs.
fn list_key(context: &str, tracks: &[&Track]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    context.hash(&mut h);
    tracks.len().hash(&mut h);
    tracks.first().map(|t| &t.id).hash(&mut h);
    tracks.last().map(|t| &t.id).hash(&mut h);
    h.finish()
}

const ACTIVE_TABLE: &str = "active-track-table";

/// Whether keyboard shortcuts go to this table: the one last clicked, else the first one on
/// the screen.
fn is_active_table(ui: &Ui, table: &str) -> bool {
    let me = Id::new(("track-table", table));
    let pass = ui.ctx().cumulative_pass_nr();
    ui.data_mut(|d| {
        let active = d.get_temp_mut_or_insert_with(Id::new(ACTIVE_TABLE), || (me, pass));
        // The active table wasn't drawn last time: it is gone from the screen.
        if active.0 != me && active.1 + 1 < pass {
            *active = (me, pass);
        }
        if active.0 == me {
            active.1 = pass;
        }
        active.0 == me
    })
}

fn make_active_table(ui: &Ui, table: &str) {
    let pass = ui.ctx().cumulative_pass_nr();
    ui.data_mut(|d| d.insert_temp(Id::new(ACTIVE_TABLE), (Id::new(("track-table", table)), pass)));
}

/// Ctrl+A picks every song, Ctrl+C copies the picked ones, Escape lets go of them. In your
/// own playlists Ctrl+X cuts and Delete removes them.
fn table_keys(ui: &Ui, cx: &mut Cx, tracks: &[&Track], opts: &TableOpts, sel_id: Id) {
    if ui.ctx().text_edit_focused() {
        return;
    }
    let (all, copy, cut, delete, escape) = ui.input(|i| {
        let event = |want: &egui::Event| i.events.iter().any(|e| e == want);
        (
            i.modifiers.command && i.key_pressed(egui::Key::A),
            event(&egui::Event::Copy),
            event(&egui::Event::Cut),
            i.key_pressed(egui::Key::Delete),
            i.key_pressed(egui::Key::Escape),
        )
    });
    // Escape closing a menu keeps the songs picked.
    let escape = escape && !egui::Popup::is_any_open(ui.ctx());
    if !(all || copy || cut || delete || escape) {
        return;
    }
    let n = tracks.len();
    let rows = ui.data_mut(|d| {
        let sel = d.get_temp_mut_or_default::<Selection>(sel_id);
        if all {
            sel.all(n);
        } else if escape {
            sel.picked.clear();
        }
        sel.rows(n)
    });
    if rows.is_empty() {
        return;
    }
    let own = opts.playlist.filter(|p| takes_songs(p));
    if copy || (cut && own.is_some()) {
        copy_songs(ui.ctx(), cx, rows.iter().map(|i| tracks[*i].clone()).collect());
    }
    if let Some(p) = own.filter(|_| cut || delete) {
        remove_rows(cx, p, tracks, &rows);
        ui.data_mut(|d| d.get_temp_mut_or_default::<Selection>(sel_id).picked.clear());
    }
}

/// Your own playlists, which songs can be pasted into.
pub fn takes_songs(p: &Playlist) -> bool {
    matches!(p.kind, PlaylistKind::Custom | PlaylistKind::M3u)
}

fn remove_rows(cx: &mut Cx, p: &Playlist, tracks: &[&Track], rows: &[usize]) {
    cx.actions.push(Action::Cmd(Command::RemoveFromPlaylist {
        playlist_id: p.id.clone(),
        rows: rows.iter().map(|i| (*i, tracks[*i].id.clone())).collect(),
    }));
}

/// Songs copied with Ctrl+C, ready to paste into a playlist. They go on the system clipboard
/// too, as links (`text`), so a paste can tell they came from here.
#[derive(Default)]
pub struct Copied {
    pub tracks: Vec<Track>,
    pub text: String,
}

const COPIED: &str = "copied-songs";

/// The songs last copied.
pub fn copied(ctx: &egui::Context) -> Option<std::sync::Arc<Copied>> {
    ctx.data(|d| d.get_temp(Id::new(COPIED)))
}

pub fn copy_songs(ctx: &egui::Context, cx: &mut Cx, tracks: Vec<Track>) {
    if tracks.is_empty() {
        return;
    }
    let text = tracks.iter().map(song_link).collect::<Vec<_>>().join("\n");
    ctx.copy_text(text.clone());
    let count = songs(tracks.len());
    ctx.data_mut(|d| d.insert_temp(Id::new(COPIED), std::sync::Arc::new(Copied { tracks, text })));
    cx.actions.push(Action::Cmd(Command::Notify(format!(
        "Copied {count}. Open one of your playlists and press Ctrl+V to add them"
    ))));
}

/// "1 song" / "12 songs".
pub fn songs(n: usize) -> String {
    if n == 1 {
        "1 song".into()
    } else {
        format!("{n} songs")
    }
}

/// A song as a line of text: its web link, or a local file's path.
pub fn song_link(t: &Track) -> String {
    let link = match t.source {
        Source::Spotify => {
            t.id.strip_prefix("spotify:track:")
                .map(|id| format!("https://open.spotify.com/track/{id}"))
        }
        Source::SoundCloud => t.uri.starts_with("http").then(|| t.uri.clone()),
        Source::Local => Some(t.uri.clone()),
        Source::AppleMusic => None,
    };
    link.unwrap_or_else(|| format!("{} - {}", t.artist, t.title))
}

/// Ctrl+V on one of your playlists adds the songs copied here, or the songs behind pasted
/// Spotify / SoundCloud links or music files.
pub fn paste_shortcut(ui: &Ui, cx: &mut Cx, p: &Playlist) {
    if !takes_songs(p) || ui.ctx().text_edit_focused() {
        return;
    }
    let pasted = ui.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Paste(text) => Some(text.clone()),
            _ => None,
        })
    });
    if let Some(text) = pasted {
        paste(cx, p, Some(&text), copied(ui.ctx()));
    }
}

/// Adds `text` (pasted) or else the copied songs to `p`.
pub fn paste(cx: &mut Cx, p: &Playlist, text: Option<&str>, copied: Option<std::sync::Arc<Copied>>) {
    let ours = copied.filter(|c| text.is_none_or(|t| t.trim() == c.text.trim()));
    if let Some(c) = ours {
        cx.actions.push(Action::Cmd(Command::AddToPlaylist {
            playlist_id: p.id.clone(),
            tracks: c.tracks.clone(),
        }));
        return;
    }
    let links: Vec<String> = text
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| crate::service::local_path(l).is_some() || crate::links::target(l).is_some())
        .map(String::from)
        .collect();
    if !links.is_empty() {
        cx.actions.push(Action::Cmd(Command::AddLinksToPlaylist {
            playlist_id: p.id.clone(),
            links,
        }));
    }
}

/// The right-click menu for several picked songs.
fn selection_menu(ui: &mut Ui, cx: &mut Cx, tracks: &[&Track], rows: &[usize], playlist: Option<&Playlist>) {
    ui.set_min_width(220.0);
    let picked = || rows.iter().map(|i| tracks[*i].clone()).collect::<Vec<Track>>();
    let count = songs(rows.len());
    ui.label(
        egui::RichText::new(format!("{count} selected"))
            .small()
            .color(TEXT_FAINT),
    );
    if ui.button(theme::ic(icon::QUEUE, "Play next")).clicked() {
        cx.actions.push(Action::Cmd(Command::PlayNext(picked())));
        ui.close();
    }
    if ui.button(theme::ic(icon::LIST_PLUS, "Add to queue")).clicked() {
        cx.actions.push(Action::Cmd(Command::Enqueue(picked())));
        ui.close();
    }
    ui.menu_button(theme::ic(icon::PLUS, "Add to playlist"), |ui| {
        ui.set_min_width(200.0);
        if ui.button(theme::ic(icon::PLUS, "New playlist…")).clicked() {
            cx.actions.push(Action::NewPlaylist(picked()));
            ui.close();
        }
        ui.separator();
        for p in cx.lib.playlists.iter().filter(|p| takes_songs(p)) {
            if ui.button(&p.name).clicked() {
                cx.actions.push(Action::Cmd(Command::AddToPlaylist {
                    playlist_id: p.id.clone(),
                    tracks: picked(),
                }));
                ui.close();
            }
        }
    });
    if ui.button(theme::ic(icon::COPY, "Copy  (Ctrl+C)")).clicked() {
        copy_songs(ui.ctx(), cx, picked());
        ui.close();
    }
    let local = rows.iter().filter(|i| tracks[**i].source == Source::Local).count();
    if local > 0 {
        let label = format!("Edit details of {}…", songs(local));
        if ui.button(theme::ic(icon::PENCIL_SIMPLE, label)).clicked() {
            cx.actions.push(Action::EditTags(picked()));
            ui.close();
        }
    }
    if let Some(p) = playlist.filter(|p| takes_songs(p)) {
        if ui
            .button(theme::ic(icon::TRASH, "Remove from this playlist  (Delete)"))
            .clicked()
        {
            remove_rows(cx, p, tracks, rows);
            ui.close();
        }
    }
    let online: Vec<Track> = rows
        .iter()
        .map(|i| tracks[*i])
        .filter(|t| t.source != Source::Local && !cx.feed.downloaded.contains_key(&t.id))
        .cloned()
        .collect();
    if !online.is_empty() && ui.button(theme::ic(icon::DOWNLOAD_SIMPLE, "Download")).clicked() {
        cx.actions.push(Action::Cmd(Command::Download(online)));
        ui.close();
    }
}

/// One line of text that underlines on hover and can be clicked (artist names).
pub fn link_text(ui: &Ui, id: Id, pos: Pos2, text: &str, font: FontId, color: Color32, max_w: f32) -> Response {
    let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = TextWrapping::truncate_at_width(max_w.max(8.0));
    let galley = ui.painter().layout_job(job);
    let rect = Rect::from_min_size(pos, galley.size());
    let resp = ui.interact(rect, id, Sense::click());
    let color = if resp.hovered() { TEXT } else { color };
    ui.painter().galley_with_override_text_color(pos, galley, color);
    if resp.hovered() {
        let y = rect.bottom() - 1.5;
        ui.painter().line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(1.0, color),
        );
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Where clicking an artist name leads: their page in the library, else a search for them.
pub fn artist_action(lib: &crate::library::Library, artist: &str) -> Action {
    let main = crate::library::credited_artists(artist, &Default::default())
        .into_iter()
        .next()
        .unwrap_or(artist.trim());
    let first = main.split(", ").next().unwrap_or(main);
    for name in [main, first] {
        if let Some(a) = lib.artist(name) {
            return Action::Open(format!("local:artist:{}", a.key));
        }
    }
    Action::Search(first.to_string())
}

/// Where playing a whole list starts: a random song while shuffle is on (it stays on from
/// list to list), else the first.
pub fn first_song(cx: &Cx, len: usize) -> usize {
    if cx.player.shuffle && len > 0 {
        rand::random_range(0..len)
    } else {
        0
    }
}

fn play_from(cx: &mut Cx, tracks: &[&Track], i: usize, context: &str) {
    cx.actions.push(Action::Cmd(Command::Play {
        tracks: tracks.iter().map(|t| (*t).clone()).collect(),
        start: i,
        context: context.to_string(),
    }));
}

/// Three little animated bars marking the playing track.
fn draw_eq(ui: &Ui, center: Pos2, color: Color32, animate: bool) {
    let t = ui.input(|i| i.time) as f32;
    for k in 0..3 {
        let h = if animate {
            4.0 + 8.0 * (0.5 + 0.5 * (t * (5.0 + k as f32 * 1.7) + k as f32).sin())
        } else {
            4.0 + k as f32 * 2.0
        };
        let x = center.x - 5.0 + k as f32 * 5.0;
        let r = Rect::from_min_max(
            Pos2::new(x - 1.5, center.y + 6.0 - h),
            Pos2::new(x + 1.5, center.y + 6.0),
        );
        ui.painter().rect_filled(r, CornerRadius::same(1), color);
    }
    if animate {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(80));
    }
}

/// Right-click menu for a track. `in_playlist` = (playlist being viewed, row index).
pub fn track_menu(ui: &mut Ui, cx: &mut Cx, t: &Track, in_playlist: Option<(Option<&Playlist>, usize)>) {
    ui.set_min_width(220.0);
    if ui.button(theme::ic(icon::QUEUE, "Play next")).clicked() {
        cx.actions.push(Action::Cmd(Command::PlayNext(vec![t.clone()])));
        ui.close();
    }
    if ui.button(theme::ic(icon::LIST_PLUS, "Add to queue")).clicked() {
        cx.actions.push(Action::Cmd(Command::Enqueue(vec![t.clone()])));
        ui.close();
    }
    let liked = cx.lib.is_liked(&t.id);
    let like_label = if liked {
        "Remove from Liked Songs"
    } else {
        "Save to Liked Songs"
    };
    if ui.button(theme::ic(icon::HEART, like_label)).clicked() {
        cx.actions.push(Action::Cmd(Command::ToggleLike(t.clone())));
        ui.close();
    }
    ui.menu_button(theme::ic(icon::PLUS, "Add to playlist"), |ui| {
        ui.set_min_width(200.0);
        if ui.button(theme::ic(icon::PLUS, "New playlist…")).clicked() {
            cx.actions.push(Action::NewPlaylist(vec![t.clone()]));
            ui.close();
        }
        ui.separator();
        for p in cx.lib.playlists.iter().filter(|p| takes_songs(p)) {
            if ui.button(&p.name).clicked() {
                cx.actions.push(Action::Cmd(Command::AddToPlaylist {
                    playlist_id: p.id.clone(),
                    tracks: vec![t.clone()],
                }));
                ui.close();
            }
        }
    });
    if ui.button(theme::ic(icon::COPY, "Copy  (Ctrl+C)")).clicked() {
        copy_songs(ui.ctx(), cx, vec![t.clone()]);
        ui.close();
    }
    if t.source == Source::Local && ui.button(theme::ic(icon::PENCIL_SIMPLE, "Edit details…")).clicked() {
        cx.actions.push(Action::EditTags(vec![t.clone()]));
        ui.close();
    }
    if let Some((Some(p), idx)) = in_playlist {
        if takes_songs(p) && ui.button(theme::ic(icon::TRASH, "Remove from this playlist")).clicked() {
            cx.actions.push(Action::Cmd(Command::RemoveFromPlaylist {
                playlist_id: p.id.clone(),
                rows: vec![(idx, t.id.clone())],
            }));
            ui.close();
        }
    }
    if t.source != Source::Local {
        if let Some(file) = cx.feed.downloaded.get(&t.id) {
            if ui
                .button(theme::ic(icon::FOLDER_OPEN, "Show download in folder"))
                .clicked()
            {
                if let Some(dir) = file.parent() {
                    cx.actions.push(Action::OpenUrl(dir.to_string_lossy().to_string()));
                }
                ui.close();
            }
        } else {
            let busy = cx.feed.downloads.iter().any(|d| d.track.id == t.id && d.state.active());
            let label = if busy { "Downloading…" } else { "Download" };
            if ui
                .add_enabled(!busy, egui::Button::new(theme::ic(icon::DOWNLOAD_SIMPLE, label)))
                .clicked()
            {
                cx.actions.push(Action::Cmd(Command::Download(vec![t.clone()])));
                ui.close();
            }
        }
    }
    ui.separator();
    match t.source {
        Source::Spotify => {
            if let Some(id) = t.id.strip_prefix("spotify:track:") {
                if ui
                    .button(theme::ic(icon::ARROW_SQUARE_OUT, "Open in Spotify"))
                    .clicked()
                {
                    cx.actions
                        .push(Action::OpenUrl(format!("https://open.spotify.com/track/{id}")));
                    ui.close();
                }
            }
        }
        Source::SoundCloud => {
            if t.uri.starts_with("http")
                && ui
                    .button(theme::ic(icon::ARROW_SQUARE_OUT, "Open on SoundCloud"))
                    .clicked()
            {
                cx.actions.push(Action::OpenUrl(t.uri.clone()));
                ui.close();
            }
        }
        Source::Local => {
            if ui.button(theme::ic(icon::FOLDER_OPEN, "Show in folder")).clicked() {
                if let Some(dir) = std::path::Path::new(&t.uri).parent() {
                    cx.actions.push(Action::OpenUrl(dir.to_string_lossy().to_string()));
                }
                ui.close();
            }
        }
        Source::AppleMusic => {
            ui.label(
                egui::RichText::new("Plays via a local / Spotify / SoundCloud match")
                    .small()
                    .color(TEXT_FAINT),
            );
        }
    }
}

/// "HI-RES" / "LOSSLESS" pill with its left edge centred on `left`. Returns its rect.
pub fn quality_badge(ui: &Ui, left: Pos2, q: &crate::model::AudioQuality) -> Option<Rect> {
    if !q.lossless {
        return None;
    }
    let (text, color) = if q.hi_res() {
        ("HI-RES", Color32::from_rgb(0xf5, 0xc4, 0x51))
    } else {
        ("LOSSLESS", Color32::from_rgb(0x4f, 0xd1, 0xc5))
    };
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), theme::bold_font(9.5), color);
    let rect = Rect::from_min_size(
        Pos2::new(left.x, left.y - (galley.size().y + 4.0) / 2.0),
        galley.size() + vec2(10.0, 4.0),
    );
    ui.painter()
        .rect_filled(rect, CornerRadius::same(4), theme::with_alpha(color, 38));
    ui.painter().galley(rect.min + vec2(5.0, 2.0), galley, color);
    Some(rect)
}

/// Small source badge "● Spotify".
pub fn source_badge(ui: &mut Ui, source: Source) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(
            egui::RichText::new(theme::source_icon(source))
                .family(theme::icons())
                .size(13.0)
                .color(source_color(source)),
        );
        ui.label(
            egui::RichText::new(source.label())
                .size(12.0)
                .color(source_color(source)),
        );
    });
}

pub fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(egui::Margin::same(16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picking_rows() {
        let mut sel = Selection::default();
        assert!(sel.rows(5).is_empty());
        sel.click(1, 5, false, false);
        assert_eq!(sel.rows(5), vec![1]);
        // Shift: everything from the last clicked row.
        sel.click(3, 5, false, true);
        assert_eq!(sel.rows(5), vec![1, 2, 3]);
        // Ctrl: add or drop one.
        sel.click(2, 5, true, false);
        assert_eq!(sel.rows(5), vec![1, 3]);
        sel.click(0, 5, true, false);
        assert_eq!(sel.rows(5), vec![0, 1, 3]);
        // Ctrl+Shift adds a range to what is picked.
        sel.click(4, 5, true, true);
        assert_eq!(sel.rows(5), vec![0, 1, 2, 3, 4]);
        // A plain click starts over.
        sel.click(2, 5, false, false);
        assert_eq!(sel.rows(5), vec![2]);
        sel.all(5);
        assert_eq!(sel.rows(5).len(), 5);
        // A shorter list never reports rows past its end.
        assert_eq!(sel.rows(3), vec![0, 1, 2]);
        sel.click(9, 3, false, false);
        assert!(!sel.has(9));
    }

    #[test]
    fn songs_copy_as_links() {
        let mut t = Track {
            id: "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into(),
            source: Source::Spotify,
            title: "Song".into(),
            artist: "Artist".into(),
            album: String::new(),
            duration_ms: 0,
            track_no: None,
            art: None,
            uri: String::new(),
            added_at: 0,
        };
        assert_eq!(song_link(&t), "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC");
        t.source = Source::SoundCloud;
        t.uri = "https://soundcloud.com/a/b".into();
        assert_eq!(song_link(&t), "https://soundcloud.com/a/b");
        t.source = Source::Local;
        t.uri = "/music/a.flac".into();
        assert_eq!(song_link(&t), "/music/a.flac");
        t.source = Source::AppleMusic;
        assert_eq!(song_link(&t), "Artist - Song");
        // Every link a copy makes is understood by a paste.
        assert!(crate::links::target("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC").is_some());
        assert!(crate::links::target("https://soundcloud.com/a/b").is_some());
    }
}
