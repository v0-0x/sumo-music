//! The Last.fm profile page (stats for a period picked from a menu per section), the account
//! card at the bottom of the sidebar and the Last.fm card on Home.

use egui::{vec2, Align, Align2, Color32, CornerRadius, CursorIcon, Id, Layout, Pos2, Rect, RichText, Sense, Ui};
use egui_phosphor::regular as icon;

use super::theme::{self, *};
use super::widgets::{self, text_trunc};
use super::{Action, Cx, View};
use crate::clock;
use crate::config::ProfilePrefs;
use crate::integrations::lastfm_stats::{
    ActivityRange, ArtLookup, Bucket, Fetch, Period, ProfileUser, Scrobble, StatsData, StatsRequest, Summary, TagShare,
    TopItem, TopKind, TopList,
};
use crate::model::now_unix;
use crate::service::{AccountStatus, Command, Feed};

/// Last.fm's red.
pub const LASTFM_RED: Color32 = Color32::from_rgb(0xd5, 0x10, 0x07);

/// The signed-in Last.fm account's name.
pub fn username(feed: &Feed) -> Option<&str> {
    match &feed.lastfm {
        AccountStatus::Connected(name) if !name.trim().is_empty() => Some(name.trim()),
        _ => None,
    }
}

/// What has been loaded for `req`; asks for it when it's missing or old.
fn fetch<'a>(cx: &mut Cx<'a>, req: StatsRequest) -> Option<&'a Fetch> {
    let feed: &'a Feed = cx.feed;
    let f = feed.profile.get(req);
    if username(feed).is_some() && f.is_none_or(|f| f.wanted(req)) {
        cx.actions.push(Action::Cmd(Command::LastfmStats(req)));
    }
    f
}

fn busy(cx: &Cx, req: StatsRequest) -> bool {
    cx.feed.profile.get(req).is_some_and(|f| f.loading)
}

fn user_of(f: Option<&Fetch>) -> Option<&ProfileUser> {
    match f?.data.as_ref()? {
        StatsData::User(u) => Some(u),
        _ => None,
    }
}

fn summary_of(f: Option<&Fetch>) -> Option<&Summary> {
    match f?.data.as_ref()? {
        StatsData::Summary(s) => Some(s),
        _ => None,
    }
}

fn top_of(f: Option<&Fetch>) -> Option<&TopList> {
    match f?.data.as_ref()? {
        StatsData::Top(t) => Some(t),
        _ => None,
    }
}

fn activity_of(f: Option<&Fetch>) -> Option<&[Bucket]> {
    match f?.data.as_ref()? {
        StatsData::Activity(b) => Some(b),
        _ => None,
    }
}

fn tags_of(f: Option<&Fetch>) -> Option<&[TagShare]> {
    match f?.data.as_ref()? {
        StatsData::Tags(t) => Some(t),
        _ => None,
    }
}

fn recent_of(f: Option<&Fetch>) -> Option<&[Scrobble]> {
    match f?.data.as_ref()? {
        StatsData::Recent(r) => Some(r),
        _ => None,
    }
}

/// A picture for an artist (`title` empty) or song: the one Last.fm listed, else one looked
/// up (asked for the first time it's needed).
fn picture(cx: &mut Cx, artist: &str, title: &str, given: Option<&str>) -> Option<String> {
    if let Some(src) = given {
        return Some(src.to_string());
    }
    match cx.feed.profile.art(artist, title) {
        Some(ArtLookup::Found(src)) => Some(src.clone()),
        Some(_) => None,
        None => {
            cx.actions.push(Action::Cmd(Command::LastfmPictures(vec![(
                artist.to_string(),
                title.to_string(),
            )])));
            None
        }
    }
}

/// 150316 -> "150,316".
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn plays(n: u64) -> String {
    if n == 1 {
        "1 play".into()
    } else {
        format!("{} plays", thousands(n))
    }
}

/// A round profile picture, or the name's first letter on Last.fm red.
pub fn avatar(ui: &Ui, cx: &mut Cx, image: Option<&str>, name: &str, rect: Rect) {
    let size = if rect.width() <= 64.0 {
        super::art::THUMB
    } else {
        super::art::MEDIUM
    };
    if let Some(tex) = cx.art.get(image, size) {
        egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), rect.size()))
            .corner_radius(CornerRadius::same((rect.width() / 2.0).min(255.0) as u8))
            .paint_at(ui, rect);
        return;
    }
    ui.painter()
        .circle_filled(rect.center(), rect.width() / 2.0, theme::mix(LASTFM_RED, CARD, 0.25));
    let letter: String = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default();
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        letter,
        theme::bold_font(rect.height() * 0.46),
        Color32::WHITE,
    );
}

// ------------------------------------------------------------------ page

pub fn page(ui: &mut Ui, cx: &mut Cx, prefs: &mut ProfilePrefs) {
    let Some(name) = username(cx.feed).map(str::to_string) else {
        not_connected(ui, cx);
        return;
    };
    let info = fetch(cx, StatsRequest::User);
    super::views::page(ui, "lastfm-profile", |ui, _viewport, _origin| {
        header(ui, cx, &name, info);
        summary_section(ui, cx, &mut prefs.summary);
        activity_section(ui, cx, &mut prefs.activity);
        top_section(ui, cx, TopKind::Artists, &mut prefs.artists);
        top_section(ui, cx, TopKind::Albums, &mut prefs.albums);
        top_section(ui, cx, TopKind::Tracks, &mut prefs.tracks);
        genres_section(ui, cx, &mut prefs.genres);
        recent_section(ui, cx);
    });
}

fn not_connected(ui: &mut Ui, cx: &mut Cx) {
    super::panels::empty_state(
        ui,
        icon::LASTFM_LOGO,
        "Connect your Last.fm account in Settings to see your listening stats here.",
    );
    ui.add_space(12.0);
    ui.vertical_centered(|ui| {
        if widgets::pill(ui, "Open Settings", cx.accent, theme::on_color(cx.accent)).clicked() {
            cx.actions.push(Action::Go(View::Settings));
        }
    });
}

fn header(ui: &mut Ui, cx: &mut Cx, name: &str, info: Option<&Fetch>) {
    let user = user_of(info);
    let w = ui.available_width();
    let size = if w > 700.0 { 168.0 } else { 116.0 };
    let pad = 24.0;
    let (card, _) = ui.allocate_exact_size(vec2(w, size + 2.0 * pad), Sense::hover());
    let image = user.and_then(|u| u.image.as_deref());
    let tint = image.and_then(|src| cx.art.accent(src)).unwrap_or(LASTFM_RED);
    widgets::rounded_gradient(
        ui,
        card,
        theme::mix(tint, CARD, 0.55),
        theme::mix(tint, CARD, 0.9),
        RADIUS + 4,
    );
    let pic = Rect::from_min_size(card.min + vec2(pad, pad), vec2(size, size));
    ui.painter().add(
        egui::epaint::Shadow {
            offset: [0, 6],
            blur: 40,
            spread: 0,
            color: theme::with_alpha(tint, 90),
        }
        .as_shape(pic, CornerRadius::same((size / 2.0) as u8)),
    );
    avatar(ui, cx, image, name, pic);

    let tx = pic.right() + 28.0;
    let tw = (card.right() - pad - tx).max(40.0);
    let title = user.map(|u| u.name.as_str()).filter(|n| !n.is_empty()).unwrap_or(name);
    let title_px = super::views::title_size(ui, title, tw);
    let mut details: Vec<String> = Vec::new();
    if let Some(u) = user {
        if !u.real_name.is_empty() {
            details.push(u.real_name.clone());
        }
        if !u.country.is_empty() {
            details.push(u.country.clone());
        }
    }
    let meta = match user {
        Some(u) => {
            let mut parts = vec![format!("{} scrobbles", thousands(u.scrobbles))];
            if u.artists > 0 {
                parts.push(format!("{} artists", thousands(u.artists)));
            }
            if let Some(loved) = u.loved {
                parts.push(format!("{} loved", thousands(loved)));
            }
            if u.registered > 0 {
                parts.push(format!("since {}", clock::date(u.registered, clock::utc_offset())));
            }
            parts.join(" · ")
        }
        None if info.is_some_and(|f| f.error.is_some()) => info.and_then(|f| f.error.clone()).unwrap_or_default(),
        None => "Loading your profile…".into(),
    };
    let details_h = if details.is_empty() { 0.0 } else { 24.0 };
    let block = 18.0 + 8.0 + title_px * 1.2 + details_h + 8.0 + 18.0;
    let mut y = card.center().y - block / 2.0;
    theme::paint_icon(
        ui.painter(),
        Pos2::new(tx + 7.0, y + 7.0),
        icon::LASTFM_LOGO,
        theme::icon_font(15.0),
        LASTFM_RED,
    );
    let kind = if user.is_some_and(|u| u.subscriber) {
        "LAST.FM PROFILE · PRO"
    } else {
        "LAST.FM PROFILE"
    };
    text_trunc(
        ui,
        Pos2::new(tx + 22.0, y),
        kind,
        theme::bold_font(11.5),
        TEXT_DIM,
        tw - 22.0,
    );
    y += 18.0 + 8.0;
    text_trunc(ui, Pos2::new(tx, y), title, theme::bold_font(title_px), TEXT, tw);
    y += title_px * 1.2;
    if !details.is_empty() {
        text_trunc(
            ui,
            Pos2::new(tx, y + 2.0),
            &details.join(" · "),
            theme::font(13.5),
            TEXT_DIM,
            tw,
        );
        y += details_h;
    }
    text_trunc(ui, Pos2::new(tx, y + 8.0), &meta, theme::font(13.5), TEXT, tw);

    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let url = user
            .map(|u| u.url.clone())
            .filter(|u| u.starts_with("http"))
            .unwrap_or_else(|| format!("https://www.last.fm/user/{}", urlencoding::encode(name)));
        if widgets::action_button(ui, icon::ARROW_SQUARE_OUT, "Open on Last.fm", true, cx.accent).clicked() {
            cx.actions.push(Action::OpenUrl(url));
        }
        if widgets::action_button(ui, icon::ARROWS_CLOCKWISE, "Refresh", false, cx.accent).clicked() {
            cx.actions.push(Action::Cmd(Command::RefreshLastfmProfile));
        }
        if cx.feed.profile.stats.values().any(|f| f.loading) {
            ui.add_space(4.0);
            ui.spinner();
        }
    });
    ui.add_space(6.0);
}

/// A section title with its period menu on the right.
fn section_head<T: Copy + PartialEq>(
    ui: &mut Ui,
    title: &str,
    id: &str,
    options: &[(T, &'static str)],
    selected: &mut T,
    busy: bool,
) {
    ui.add_space(18.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(theme::bold_font(18.0)).color(TEXT));
        if busy {
            ui.add_space(4.0);
            ui.spinner();
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let current = options
                .iter()
                .find(|(v, _)| v == selected)
                .map(|(_, l)| *l)
                .unwrap_or_default();
            egui::ComboBox::from_id_salt(id)
                .selected_text(current)
                .width(150.0)
                .show_ui(ui, |ui| {
                    for (value, label) in options {
                        ui.selectable_value(selected, *value, *label);
                    }
                });
        });
    });
    ui.add_space(8.0);
}

fn periods() -> Vec<(Period, &'static str)> {
    Period::ALL.iter().map(|p| (*p, p.label())).collect()
}

/// Spinner or error in place of a section that has nothing to show yet.
fn waiting(ui: &mut Ui, f: Option<&Fetch>, height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(RADIUS), CARD);
    match f.and_then(|f| f.error.as_deref()) {
        Some(error) => {
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                format!("Couldn't load this: {error}"),
                theme::font(13.5),
                TEXT_DIM,
            );
        }
        None => {
            let t = ui.input(|i| i.time) as f32;
            for k in 0..3 {
                let phase = ((t * 3.0 - k as f32 * 0.6).sin() + 1.0) / 2.0;
                ui.painter().circle_filled(
                    rect.center() + vec2((k as f32 - 1.0) * 14.0, 0.0),
                    3.5,
                    theme::with_alpha(TEXT_DIM, (80.0 + 150.0 * phase) as u8),
                );
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
        }
    }
}

fn subtitle(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::font(13.0)).color(TEXT_DIM));
    ui.add_space(8.0);
}

// ------------------------------------------------------------------ totals

fn summary_section(ui: &mut Ui, cx: &mut Cx, period: &mut Period) {
    let busy = busy(cx, StatsRequest::Summary(*period));
    section_head(ui, "Listening stats", "lastfm-summary", &periods(), period, busy);
    let f = fetch(cx, StatsRequest::Summary(*period));
    let Some(s) = summary_of(f) else {
        waiting(ui, f, 88.0);
        return;
    };
    let average = if *period == Period::Today {
        let offset = clock::utc_offset();
        let now = now_unix();
        let hours = ((now - clock::day_start(clock::local_day(now, offset), offset)) as f64 / 3600.0).max(1.0);
        (format!("{:.1}", s.scrobbles as f64 / hours), "Per hour")
    } else if s.days > 0.0 {
        let per_day = s.scrobbles as f64 / s.days;
        let shown = if per_day < 10.0 {
            format!("{per_day:.1}")
        } else {
            thousands(per_day.round() as u64)
        };
        (shown, "Per day")
    } else {
        ("–".into(), "Per day")
    };
    let tiles = [
        (icon::WAVEFORM, thousands(s.scrobbles), "Scrobbles"),
        (icon::MICROPHONE_STAGE, thousands(s.artists), "Artists"),
        (icon::VINYL_RECORD, thousands(s.albums), "Albums"),
        (icon::MUSIC_NOTES, thousands(s.tracks), "Tracks"),
        (icon::CHART_BAR, average.0, average.1),
    ];
    stat_tiles(ui, &tiles);
}

fn stat_tiles(ui: &mut Ui, tiles: &[(&str, String, &str)]) {
    let gap = 12.0;
    let w = ui.available_width();
    let fit = (((w + gap) / (128.0 + gap)).floor() as usize).clamp(1, tiles.len());
    // Rows as even as they can be (3 + 2 rather than 4 + 1).
    let cols = tiles.len().div_ceil(tiles.len().div_ceil(fit));
    let tile_w = ((w - gap * (cols as f32 - 1.0)) / cols as f32).floor();
    for row in tiles.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (glyph, value, label) in row {
                let (r, _) = ui.allocate_exact_size(vec2(tile_w, 88.0), Sense::hover());
                ui.painter().rect_filled(r, CornerRadius::same(RADIUS), CARD);
                theme::paint_icon(
                    ui.painter(),
                    Pos2::new(r.right() - 24.0, r.top() + 24.0),
                    glyph,
                    theme::icon_font(18.0),
                    TEXT_FAINT,
                );
                text_trunc(
                    ui,
                    Pos2::new(r.left() + 16.0, r.top() + 16.0),
                    value,
                    theme::bold_font(26.0),
                    TEXT,
                    tile_w - 56.0,
                );
                text_trunc(
                    ui,
                    Pos2::new(r.left() + 16.0, r.bottom() - 28.0),
                    label,
                    theme::font(13.0),
                    TEXT_DIM,
                    tile_w - 32.0,
                );
            }
        });
        ui.add_space(gap);
    }
}

// ------------------------------------------------------------------ chart

fn activity_section(ui: &mut Ui, cx: &mut Cx, range: &mut ActivityRange) {
    let busy = busy(cx, StatsRequest::Activity(*range));
    let options: Vec<(ActivityRange, &'static str)> = ActivityRange::ALL.iter().map(|r| (*r, r.label())).collect();
    section_head(ui, "Listening activity", "lastfm-activity", &options, range, busy);
    let f = fetch(cx, StatsRequest::Activity(*range));
    let Some(bars) = activity_of(f) else {
        waiting(ui, f, 220.0);
        return;
    };
    let per = match range {
        ActivityRange::Week | ActivityRange::Month => "a day",
        ActivityRange::Year => "a month",
        ActivityRange::Years => "a year",
    };
    chart(ui, cx.accent, bars, per);
}

fn chart(ui: &mut Ui, accent: Color32, bars: &[Bucket], per: &str) {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 220.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(RADIUS), CARD);
    let total: u64 = bars.iter().map(|b| b.plays).sum();
    let max = bars.iter().map(|b| b.plays).max().unwrap_or(0).max(1);
    p.text(
        rect.left_top() + vec2(18.0, 16.0),
        Align2::LEFT_TOP,
        format!("{} scrobbles", thousands(total)),
        theme::bold_font(15.0),
        TEXT,
    );
    if !bars.is_empty() {
        let avg = total as f64 / bars.len() as f64;
        p.text(
            rect.right_top() + vec2(-18.0, 17.0),
            Align2::RIGHT_TOP,
            format!("about {} {per}", thousands(avg.round() as u64)),
            theme::font(13.0),
            TEXT_DIM,
        );
    }
    let plot = Rect::from_min_max(rect.min + vec2(18.0, 52.0), rect.max - vec2(18.0, 30.0));
    // The busiest bar's height, as a guide line.
    p.line_segment(
        [plot.left_top(), plot.right_top()],
        egui::Stroke::new(1.0, theme::with_alpha(TEXT_FAINT, 60)),
    );
    p.line_segment(
        [plot.left_bottom(), plot.right_bottom()],
        egui::Stroke::new(1.0, theme::with_alpha(TEXT_FAINT, 90)),
    );
    let n = bars.len().max(1);
    let slot = plot.width() / n as f32;
    let bar_w = (slot * 0.68).clamp(2.0, 56.0);
    let pointer = resp.hover_pos();
    // The bar under the pointer stays bright; the others fade back.
    let pointed = pointer
        .filter(|p| p.x >= plot.left() && p.x < plot.right())
        .map(|p| (((p.x - plot.left()) / slot) as usize).min(n - 1));
    let mut hovered: Option<(usize, Rect)> = None;
    for (i, b) in bars.iter().enumerate() {
        let x = plot.left() + slot * (i as f32 + 0.5);
        let h = plot.height() * b.plays as f32 / max as f32;
        let bar = Rect::from_min_max(
            Pos2::new(x - bar_w / 2.0, plot.bottom() - h.max(2.0)),
            Pos2::new(x + bar_w / 2.0, plot.bottom()),
        );
        let over = pointed == Some(i);
        let color = match (b.plays, pointed) {
            (0, _) => theme::with_alpha(TEXT_FAINT, 70),
            (_, Some(_)) if !over => theme::mix(accent, CARD, 0.55),
            _ => accent,
        };
        let r = (bar_w / 4.0).min(5.0) as u8;
        p.rect_filled(
            bar,
            CornerRadius {
                nw: r,
                ne: r,
                sw: 0,
                se: 0,
            },
            color,
        );
        let every = if n > 12 { 5 } else { 1 };
        if (n - 1 - i) % every == 0 || over {
            p.text(
                Pos2::new(x, plot.bottom() + 8.0),
                Align2::CENTER_TOP,
                &b.label,
                theme::font(11.5),
                if over { TEXT } else { TEXT_FAINT },
            );
        }
        if over {
            hovered = Some((i, bar));
        }
    }
    // The hovered bar's numbers in a bubble above it.
    if let Some((i, bar)) = hovered {
        let b = &bars[i];
        let text = format!("{} · {}", b.detail, plays_word(b.plays));
        let galley = p.layout_no_wrap(text, theme::bold_font(12.5), TEXT);
        let size = galley.size() + vec2(16.0, 10.0);
        let mut bubble = Rect::from_center_size(Pos2::new(bar.center().x, bar.top() - 8.0 - size.y / 2.0), size);
        let shift_x = (rect.left() + 6.0 - bubble.left()).max(0.0) + (rect.right() - 6.0 - bubble.right()).min(0.0);
        let shift_y = (rect.top() + 6.0 - bubble.top()).max(0.0);
        bubble = bubble.translate(vec2(shift_x, shift_y));
        p.rect_filled(bubble, CornerRadius::same(8), SELECTED);
        p.galley(bubble.min + vec2(8.0, 5.0), galley, TEXT);
    }
}

fn plays_word(n: u64) -> String {
    if n == 1 {
        "1 scrobble".into()
    } else {
        format!("{} scrobbles", thousands(n))
    }
}

// ------------------------------------------------------------------ top lists

fn top_section(ui: &mut Ui, cx: &mut Cx, kind: TopKind, period: &mut Period) {
    let (title, id, noun) = match kind {
        TopKind::Artists => ("Top artists", "lastfm-artists", "artists"),
        TopKind::Albums => ("Top albums", "lastfm-albums", "albums"),
        TopKind::Tracks => ("Top tracks", "lastfm-tracks", "tracks"),
    };
    let busy = busy(cx, StatsRequest::Top(kind, *period));
    section_head(ui, title, id, &periods(), period, busy);
    let f = fetch(cx, StatsRequest::Top(kind, *period));
    let Some(list) = top_of(f) else {
        waiting(ui, f, if kind == TopKind::Tracks { 160.0 } else { 220.0 });
        return;
    };
    if list.items.is_empty() {
        subtitle(ui, &format!("Nothing played {}.", period.phrase()));
        return;
    }
    if list.total > 0 {
        subtitle(
            ui,
            &format!("{} different {noun} {}", thousands(list.total), period.phrase()),
        );
    }
    let more_id = Id::new(("lastfm-show-all", id));
    let all = ui.data(|d| d.get_temp::<bool>(more_id)).unwrap_or(false);
    let shown = match kind {
        TopKind::Tracks => 10,
        _ => {
            let cols = (((ui.available_width() + 20.0) / (150.0 + 20.0)).floor() as usize).max(1);
            (cols * 2).min(12)
        }
    };
    let n = if all {
        list.items.len()
    } else {
        shown.min(list.items.len())
    };
    let items = &list.items[..n];
    if kind == TopKind::Tracks {
        // The whole list plays (or queues) by name: each song from the library or a service.
        let named =
            || -> Vec<(String, String)> { list.items.iter().map(|t| (t.artist.clone(), t.name.clone())).collect() };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let context = format!("Your top tracks {}", period.phrase());
            if widgets::pill(ui, "Play all", cx.accent, theme::on_color(cx.accent)).clicked() {
                cx.actions.push(Action::Cmd(Command::PlayNamed {
                    songs: named(),
                    context: context.clone(),
                    queue: false,
                }));
            }
            if widgets::pill(ui, "Add to queue", HOVER, TEXT).clicked() {
                cx.actions.push(Action::Cmd(Command::PlayNamed {
                    songs: named(),
                    context,
                    queue: true,
                }));
            }
        });
        ui.add_space(8.0);
    }
    match kind {
        TopKind::Tracks => track_rows(ui, cx, items),
        _ => tiles(ui, cx, kind, items),
    }
    if list.items.len() > shown {
        let label = if all {
            "Show less".to_string()
        } else {
            format!("Show all {}", list.items.len())
        };
        let resp = ui
            .add(
                egui::Label::new(RichText::new(label).font(theme::bold_font(13.5)).color(TEXT_DIM))
                    .sense(Sense::click()),
            )
            .on_hover_cursor(CursorIcon::PointingHand);
        if resp.clicked() {
            ui.data_mut(|d| d.insert_temp(more_id, !all));
        }
    }
}

/// Where clicking an album or song leads: a search for it.
fn search_for(item: &TopItem) -> Action {
    Action::Search(format!("{} {}", item.artist, item.name).trim().to_string())
}

fn item_menu(ui: &mut Ui, cx: &mut Cx, item: &TopItem, song: bool) {
    if song {
        let named = vec![(item.artist.clone(), item.name.clone())];
        if ui.button(theme::ic(icon::PLAY, "Play")).clicked() {
            cx.actions.push(Action::Cmd(Command::PlayNamed {
                songs: named.clone(),
                context: "Last.fm".into(),
                queue: false,
            }));
            ui.close();
        }
        if ui.button(theme::ic(icon::LIST_PLUS, "Add to queue")).clicked() {
            cx.actions.push(Action::Cmd(Command::PlayNamed {
                songs: named,
                context: "Last.fm".into(),
                queue: true,
            }));
            ui.close();
        }
    }
    if ui.button(theme::ic(icon::MAGNIFYING_GLASS, "Search in Sumo")).clicked() {
        cx.actions.push(if item.artist.is_empty() {
            Action::Search(item.name.clone())
        } else {
            search_for(item)
        });
        ui.close();
    }
    if !item.artist.is_empty() && ui.button(theme::ic(icon::USER, "Go to artist")).clicked() {
        cx.actions.push(widgets::artist_action(cx.lib, &item.artist));
        ui.close();
    }
    if item.url.starts_with("http")
        && ui
            .button(theme::ic(icon::ARROW_SQUARE_OUT, "Open on Last.fm"))
            .clicked()
    {
        cx.actions.push(Action::OpenUrl(item.url.clone()));
        ui.close();
    }
}

fn tiles(ui: &mut Ui, cx: &mut Cx, kind: TopKind, items: &[TopItem]) {
    widgets::grid(ui, items.len(), 150.0, |ui, i, w| {
        let item = &items[i];
        let art = match kind {
            TopKind::Artists => picture(cx, &item.name, "", item.image.as_deref()),
            _ => item.image.clone(),
        };
        let height = w + 54.0;
        let (rect, resp) = ui.allocate_exact_size(vec2(w, height), Sense::click());
        widgets::fade_fill(ui, resp.id, rect.expand(8.0), RADIUS + 2, resp.hovered(), CARD);
        let art_rect = Rect::from_min_size(rect.min, vec2(w, w));
        if kind == TopKind::Artists {
            widgets::cover_round(ui, cx.art, art.as_deref(), art_rect, widgets::artist_fallback());
        } else {
            widgets::cover(
                ui,
                cx.art,
                art.as_deref(),
                art_rect,
                8,
                (Color32::from_rgb(0x55, 0x5a, 0x78), icon::VINYL_RECORD),
            );
        }
        rank_badge(ui, art_rect.left_top() + vec2(8.0, 8.0), i + 1);
        text_trunc(
            ui,
            art_rect.left_bottom() + vec2(0.0, 8.0),
            &item.name,
            theme::bold_font(14.0),
            TEXT,
            w,
        );
        let sub = if item.artist.is_empty() {
            plays(item.plays)
        } else {
            format!("{} · {}", item.artist, plays(item.plays))
        };
        text_trunc(
            ui,
            art_rect.left_bottom() + vec2(0.0, 28.0),
            &sub,
            theme::font(12.5),
            TEXT_DIM,
            w,
        );
        let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
        if resp.clicked() {
            cx.actions.push(match kind {
                TopKind::Artists => widgets::artist_action(cx.lib, &item.name),
                _ => search_for(item),
            });
        }
        resp.context_menu(|ui| item_menu(ui, cx, item, false));
    });
}

fn rank_badge(ui: &Ui, pos: Pos2, rank: usize) {
    let text = format!("#{rank}");
    let galley = ui
        .painter()
        .layout_no_wrap(text, theme::bold_font(12.0), Color32::WHITE);
    let r = Rect::from_min_size(pos, galley.size() + vec2(12.0, 6.0));
    ui.painter()
        .rect_filled(r, CornerRadius::same(7), Color32::from_black_alpha(170));
    ui.painter().galley(r.min + vec2(6.0, 3.0), galley, Color32::WHITE);
}

fn track_rows(ui: &mut Ui, cx: &mut Cx, items: &[TopItem]) {
    let max = items.iter().map(|t| t.plays).max().unwrap_or(1).max(1);
    for (i, item) in items.iter().enumerate() {
        let art = picture(cx, &item.artist, &item.name, item.image.as_deref());
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 56.0), Sense::click());
        widgets::fade_fill(ui, resp.id, rect, 10, resp.hovered(), CARD);
        ui.painter().text(
            Pos2::new(rect.left() + 28.0, rect.center().y),
            Align2::RIGHT_CENTER,
            (i + 1).to_string(),
            theme::font(14.0),
            TEXT_DIM,
        );
        let art_rect = Rect::from_min_size(Pos2::new(rect.left() + 42.0, rect.top() + 8.0), vec2(40.0, 40.0));
        widgets::cover(
            ui,
            cx.art,
            art.as_deref(),
            art_rect,
            6,
            (Color32::from_rgb(0x55, 0x5a, 0x78), icon::MUSIC_NOTE),
        );
        let bar_w = if w > 640.0 { 220.0 } else { 120.0 };
        let text_x = art_rect.right() + 12.0;
        let text_w = (rect.right() - bar_w - 24.0 - text_x).max(40.0);
        text_trunc(
            ui,
            Pos2::new(text_x, rect.top() + 9.0),
            &item.name,
            theme::bold_font(14.0),
            TEXT,
            text_w,
        );
        let artist = widgets::link_text(
            ui,
            resp.id.with("artist"),
            Pos2::new(text_x, rect.top() + 30.0),
            &item.artist,
            theme::font(12.5),
            TEXT_DIM,
            text_w,
        );
        if artist.clicked() {
            cx.actions.push(widgets::artist_action(cx.lib, &item.artist));
        }
        // How it compares with the most played one.
        let track = Rect::from_min_size(
            Pos2::new(rect.right() - bar_w - 12.0, rect.center().y + 2.0),
            vec2(bar_w, 6.0),
        );
        ui.painter()
            .rect_filled(track, CornerRadius::same(3), theme::with_alpha(TEXT_FAINT, 50));
        let filled = Rect::from_min_size(track.min, vec2(track.width() * item.plays as f32 / max as f32, 6.0));
        ui.painter().rect_filled(filled, CornerRadius::same(3), cx.accent);
        ui.painter().text(
            Pos2::new(track.right(), track.top() - 5.0),
            Align2::RIGHT_BOTTOM,
            plays(item.plays),
            theme::font(12.5),
            TEXT_DIM,
        );
        let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
        if resp.clicked() && !artist.hovered() {
            cx.actions.push(search_for(item));
        }
        resp.context_menu(|ui| item_menu(ui, cx, item, true));
    }
}

// ------------------------------------------------------------------ genres

fn genres_section(ui: &mut Ui, cx: &mut Cx, period: &mut Period) {
    let busy = busy(cx, StatsRequest::Tags(*period));
    section_head(ui, "Top genres", "lastfm-genres", &periods(), period, busy);
    let f = fetch(cx, StatsRequest::Tags(*period));
    let Some(tags) = tags_of(f) else {
        waiting(ui, f, 140.0);
        return;
    };
    if tags.is_empty() {
        subtitle(ui, &format!("Nothing played {}.", period.phrase()));
        return;
    }
    subtitle(
        ui,
        "How much of your listening each genre covers, from the tags on your top artists",
    );
    let top = tags.first().map(|t| t.share).unwrap_or(1.0).max(0.01);
    let w = ui.available_width();
    let name_w = if w > 640.0 { 180.0 } else { 120.0 };
    for t in tags {
        let (row, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
        let name = capitalized(&t.name);
        text_trunc(
            ui,
            Pos2::new(row.left() + 4.0, row.center().y - 9.0),
            &name,
            theme::bold_font(14.0),
            TEXT,
            name_w - 12.0,
        );
        let track = Rect::from_min_max(
            Pos2::new(row.left() + name_w, row.center().y - 5.0),
            Pos2::new(row.right() - 56.0, row.center().y + 5.0),
        );
        ui.painter()
            .rect_filled(track, CornerRadius::same(5), theme::with_alpha(TEXT_FAINT, 40));
        let filled = Rect::from_min_size(
            track.min,
            vec2((track.width() * t.share / top).max(4.0), track.height()),
        );
        let color = if resp.hovered() {
            theme::mix(cx.accent, Color32::WHITE, 0.3)
        } else {
            cx.accent
        };
        ui.painter().rect_filled(filled, CornerRadius::same(5), color);
        ui.painter().text(
            Pos2::new(row.right() - 4.0, row.center().y),
            Align2::RIGHT_CENTER,
            format!("{:.0}%", t.share * 100.0),
            theme::font(13.0),
            TEXT_DIM,
        );
        if resp
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text(format!("Search for {name}"))
            .clicked()
        {
            cx.actions.push(Action::Search(t.name.clone()));
        }
        ui.add_space(2.0);
    }
}

fn capitalized(tag: &str) -> String {
    tag.split(' ')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ------------------------------------------------------------------ recent

fn recent_section(ui: &mut Ui, cx: &mut Cx) {
    ui.add_space(18.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Recent scrobbles")
                .font(theme::bold_font(18.0))
                .color(TEXT),
        );
        if busy(cx, StatsRequest::Recent) {
            ui.add_space(4.0);
            ui.spinner();
        }
    });
    ui.add_space(8.0);
    let f = fetch(cx, StatsRequest::Recent);
    let Some(list) = recent_of(f) else {
        waiting(ui, f, 160.0);
        return;
    };
    if list.is_empty() {
        subtitle(ui, "Nothing scrobbled yet.");
        return;
    }
    let more_id = Id::new("lastfm-recent-all");
    let all = ui.data(|d| d.get_temp::<bool>(more_id)).unwrap_or(false);
    let n = if all { list.len() } else { list.len().min(15) };
    let offset = clock::utc_offset();
    let now = now_unix();
    for s in &list[..n] {
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 56.0), Sense::click());
        let playing = s.at.is_none();
        if playing {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(10), theme::with_alpha(cx.accent, 24));
        }
        widgets::fade_fill(ui, resp.id, rect, 10, resp.hovered(), CARD);
        let art_rect = Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.top() + 8.0), vec2(40.0, 40.0));
        widgets::cover(
            ui,
            cx.art,
            s.image.as_deref(),
            art_rect,
            6,
            (Color32::from_rgb(0x55, 0x5a, 0x78), icon::MUSIC_NOTE),
        );
        let when = match s.at {
            None => "Scrobbling now".to_string(),
            Some(at) => clock::ago(at, now, offset),
        };
        let when_w = ui
            .painter()
            .layout_no_wrap(when.clone(), theme::font(12.5), TEXT_DIM)
            .size()
            .x;
        let right = rect.right() - 12.0;
        let text_x = art_rect.right() + 12.0;
        let text_w = (right - when_w - 40.0 - text_x).max(40.0);
        text_trunc(
            ui,
            Pos2::new(text_x, rect.top() + 9.0),
            &s.title,
            theme::bold_font(14.0),
            if playing { cx.accent } else { TEXT },
            text_w,
        );
        let sub = if s.album.is_empty() {
            s.artist.clone()
        } else {
            format!("{} · {}", s.artist, s.album)
        };
        text_trunc(
            ui,
            Pos2::new(text_x, rect.top() + 30.0),
            &sub,
            theme::font(12.5),
            TEXT_DIM,
            text_w,
        );
        ui.painter().text(
            Pos2::new(right, rect.center().y),
            Align2::RIGHT_CENTER,
            &when,
            theme::font(12.5),
            if playing { cx.accent } else { TEXT_DIM },
        );
        let mut x = right - when_w - 14.0;
        if playing {
            theme::paint_icon(
                ui.painter(),
                Pos2::new(x, rect.center().y),
                icon::WAVEFORM,
                theme::icon_font(16.0),
                cx.accent,
            );
            x -= 22.0;
        }
        if s.loved {
            theme::paint_icon(
                ui.painter(),
                Pos2::new(x, rect.center().y),
                egui_phosphor::fill::HEART,
                theme::fill_icon_font(15.0),
                LASTFM_RED,
            );
        }
        let item = TopItem {
            name: s.title.clone(),
            artist: s.artist.clone(),
            plays: 0,
            image: s.image.clone(),
            url: s.url.clone(),
        };
        let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
        if resp.clicked() {
            cx.actions.push(search_for(&item));
        }
        resp.context_menu(|ui| item_menu(ui, cx, &item, true));
    }
    if list.len() > 15 {
        let label = if all {
            "Show less".to_string()
        } else {
            format!("Show all {}", list.len())
        };
        let resp = ui
            .add(
                egui::Label::new(RichText::new(label).font(theme::bold_font(13.5)).color(TEXT_DIM))
                    .sense(Sense::click()),
            )
            .on_hover_cursor(CursorIcon::PointingHand);
        if resp.clicked() {
            ui.data_mut(|d| d.insert_temp(more_id, !all));
        }
    }
}

// ------------------------------------------------------------------ sidebar and home

/// The account at the bottom of the sidebar: picture and name (opens the profile) and a
/// Settings button. `false` when not signed in to Last.fm (the sidebar shows Settings alone).
pub fn sidebar_account(ui: &mut Ui, cx: &mut Cx, view: &View, collapsed: bool) -> bool {
    let Some(name) = username(cx.feed).map(str::to_string) else {
        return false;
    };
    let info = fetch(cx, StatsRequest::User);
    let user = user_of(info);
    let image = user.and_then(|u| u.image.clone());
    let on_profile = *view == View::Profile;
    let on_settings = *view == View::Settings;
    let w = ui.available_width() - 4.0;
    if collapsed {
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 40.0), Sense::click());
        if on_profile {
            ui.painter().rect_filled(rect, CornerRadius::same(10), SELECTED);
        } else {
            widgets::fade_fill(ui, resp.id, rect, 10, resp.hovered(), CARD);
        }
        let pic = Rect::from_center_size(rect.center(), vec2(28.0, 28.0));
        avatar(ui, cx, image.as_deref(), &name, pic);
        let resp = resp
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text(format!("{name} on Last.fm"));
        if resp.clicked() {
            cx.actions.push(Action::Go(View::Profile));
        }
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 36.0), Sense::click());
        if on_settings {
            ui.painter().rect_filled(rect, CornerRadius::same(10), SELECTED);
        } else {
            widgets::fade_fill(ui, resp.id, rect, 10, resp.hovered(), CARD);
        }
        theme::paint_icon(
            ui.painter(),
            rect.center(),
            icon::GEAR,
            if on_settings {
                theme::fill_icon_font(18.0)
            } else {
                theme::icon_font(18.0)
            },
            if on_settings || resp.hovered() { TEXT } else { TEXT_DIM },
        );
        if resp
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text("Settings")
            .clicked()
        {
            cx.actions.push(Action::Go(View::Settings));
        }
        return true;
    }
    let (rect, _) = ui.allocate_exact_size(vec2(w, 52.0), Sense::hover());
    let gear = Rect::from_center_size(Pos2::new(rect.right() - 22.0, rect.center().y), vec2(36.0, 36.0));
    let card = Rect::from_min_max(rect.min, Pos2::new(gear.left() - 4.0, rect.bottom()));
    let resp = ui
        .interact(card, Id::new("sidebar-account"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    if on_profile {
        ui.painter().rect_filled(card, CornerRadius::same(12), SELECTED);
    } else {
        widgets::fade_fill(ui, resp.id, card, 12, resp.hovered(), CARD);
    }
    let pic = Rect::from_min_size(Pos2::new(card.left() + 8.0, card.center().y - 17.0), vec2(34.0, 34.0));
    avatar(ui, cx, image.as_deref(), &name, pic);
    // A small Last.fm mark on the picture.
    let mark = Pos2::new(pic.right() - 2.0, pic.bottom() - 2.0);
    ui.painter().circle_filled(mark, 7.5, WINDOW_BG);
    ui.painter().circle_filled(mark, 6.0, LASTFM_RED);
    theme::paint_icon(
        ui.painter(),
        mark,
        icon::LASTFM_LOGO,
        theme::icon_font(8.5),
        Color32::WHITE,
    );
    let tx = pic.right() + 10.0;
    let tw = card.right() - tx - 6.0;
    let shown = user.map(|u| u.name.as_str()).filter(|n| !n.is_empty()).unwrap_or(&name);
    text_trunc(
        ui,
        Pos2::new(tx, card.center().y - 17.0),
        shown,
        theme::bold_font(14.0),
        TEXT,
        tw,
    );
    let sub = match user {
        Some(u) => format!("{} scrobbles", thousands(u.scrobbles)),
        None => "Last.fm".to_string(),
    };
    text_trunc(
        ui,
        Pos2::new(tx, card.center().y + 2.0),
        &sub,
        theme::font(12.0),
        TEXT_DIM,
        tw,
    );
    if resp.on_hover_text("Your Last.fm stats").clicked() {
        cx.actions.push(Action::Go(View::Profile));
    }
    let resp = ui
        .interact(gear, Id::new("sidebar-settings"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Settings");
    if on_settings {
        ui.painter().rect_filled(gear, CornerRadius::same(10), SELECTED);
    } else {
        widgets::fade_fill(ui, resp.id, gear, 10, resp.hovered(), CARD);
    }
    theme::paint_icon(
        ui.painter(),
        gear.center(),
        icon::GEAR,
        if on_settings {
            theme::fill_icon_font(18.0)
        } else {
            theme::icon_font(18.0)
        },
        if on_settings || resp.hovered() { TEXT } else { TEXT_DIM },
    );
    if resp.clicked() {
        cx.actions.push(Action::Go(View::Settings));
    }
    true
}

/// Home's card: this week on Last.fm, with the profile picture and name.
pub fn home_card(ui: &mut Ui, cx: &mut Cx) {
    let Some(name) = username(cx.feed).map(str::to_string) else {
        return;
    };
    let user = user_of(fetch(cx, StatsRequest::User));
    let week = summary_of(fetch(cx, StatsRequest::Summary(Period::Week)));
    let artists = top_of(fetch(cx, StatsRequest::Top(TopKind::Artists, Period::Week)));
    ui.add_space(18.0);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 96.0), Sense::click());
    let image = user.and_then(|u| u.image.clone());
    let tint = image
        .as_deref()
        .and_then(|src| cx.art.accent(src))
        .unwrap_or(LASTFM_RED);
    widgets::rounded_gradient(
        ui,
        rect,
        theme::mix(tint, CARD, 0.7),
        theme::mix(tint, CARD, 0.9),
        RADIUS,
    );
    widgets::fade_fill(ui, resp.id, rect, RADIUS, resp.hovered(), theme::with_alpha(HOVER, 120));
    let pic = Rect::from_min_size(Pos2::new(rect.left() + 18.0, rect.center().y - 30.0), vec2(60.0, 60.0));
    avatar(ui, cx, image.as_deref(), &name, pic);
    let shown = user.map(|u| u.name.clone()).filter(|n| !n.is_empty()).unwrap_or(name);
    // Up to three of the week's top artists on the right.
    let faces: Vec<&TopItem> = artists.map(|a| a.items.iter().take(3).collect()).unwrap_or_default();
    let faces_w = if w > 560.0 {
        faces.len() as f32 * 34.0 + 22.0
    } else {
        0.0
    };
    let tx = pic.right() + 16.0;
    let tw = (rect.right() - tx - faces_w - 120.0).max(60.0);
    text_trunc(
        ui,
        Pos2::new(tx, rect.center().y - 22.0),
        &shown,
        theme::bold_font(18.0),
        TEXT,
        tw,
    );
    let line = match (week, artists.and_then(|a| a.items.first())) {
        (Some(s), Some(top)) => format!(
            "{} scrobbles this week · most played: {}",
            thousands(s.scrobbles),
            top.name
        ),
        (Some(s), None) => format!("{} scrobbles this week", thousands(s.scrobbles)),
        _ => "Your week on Last.fm".to_string(),
    };
    text_trunc(
        ui,
        Pos2::new(tx, rect.center().y + 4.0),
        &line,
        theme::font(13.5),
        TEXT_DIM,
        tw,
    );
    let label = "See your stats";
    let pill_w = widgets::pill_width(ui, label);
    let pill = Rect::from_min_size(
        Pos2::new(rect.right() - 18.0 - pill_w, rect.center().y - 17.0),
        vec2(pill_w, 34.0),
    );
    if faces_w > 0.0 {
        let mut x = pill.left() - 22.0 - 30.0;
        for a in faces.iter().rev() {
            let r = Rect::from_min_size(Pos2::new(x, rect.center().y - 15.0), vec2(30.0, 30.0));
            let art = picture(cx, &a.name, "", a.image.as_deref());
            ui.painter().circle_filled(r.center(), 16.5, CARD);
            widgets::cover_round(ui, cx.art, art.as_deref(), r, widgets::artist_fallback());
            x -= 22.0;
        }
    }
    let over = ui.rect_contains_pointer(pill);
    ui.painter().rect_filled(
        pill,
        CornerRadius::same(17),
        if over {
            theme::mix(cx.accent, Color32::WHITE, 0.12)
        } else {
            cx.accent
        },
    );
    ui.painter().text(
        pill.center(),
        Align2::CENTER_CENTER,
        label,
        theme::bold_font(14.0),
        theme::on_color(cx.accent),
    );
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        cx.actions.push(Action::Go(View::Profile));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_easily() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(150_316), "150,316");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(plays(1), "1 play");
        assert_eq!(plays(1200), "1,200 plays");
        assert_eq!(capitalized("cloud rap"), "Cloud Rap");
        assert_eq!(capitalized("hip-hop"), "Hip-hop");
    }
}
