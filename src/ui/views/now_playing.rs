//! The full-screen Now Playing view.

use super::*;

pub(super) fn now_playing(ui: &mut Ui, cx: &mut Cx) {
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
            let text = egui::RichText::new(format!("PLAYING FROM {}", cx.player.context.to_uppercase()))
                .size(12.0)
                .color(TEXT_DIM);
            match widgets::context_action(cx) {
                Some(go) => {
                    let link = ui
                        .add(egui::Label::new(text).sense(Sense::click()))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("Go to it");
                    if link.clicked() {
                        cx.actions.push(go);
                    }
                }
                None => {
                    ui.label(text);
                }
            }
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

pub(super) fn track_titles(ui: &mut Ui, cx: &mut Cx, t: &Track, w: f32) {
    let (r, _) = ui.allocate_exact_size(vec2(w, 36.0), Sense::hover());
    text_trunc(ui, r.min, &t.title, theme::bold_font(28.0), TEXT, w);
    let (r, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
    let artist = widgets::link_text(
        ui,
        Id::new("np-big-artist"),
        r.min,
        &t.artist,
        theme::font(17.0),
        TEXT_DIM,
        w,
    );
    if artist.clicked() {
        cx.actions.push(widgets::artist_action(cx.lib, &t.artist));
    }
    if !t.album.is_empty() {
        let (r, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
        let album = widgets::link_text(
            ui,
            Id::new("np-big-album"),
            r.min,
            &t.album,
            theme::font(13.5),
            TEXT_FAINT,
            w,
        );
        if album.clicked() {
            if let Some(go) = widgets::album_action(cx.lib, t) {
                cx.actions.push(go);
            }
        }
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
