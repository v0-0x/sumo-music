//! The Downloads page.

use super::*;

/// One line of the Downloads page.
pub(super) struct DownloadRow {
    pub(super) track: Track,
    pub(super) state: DownloadState,
}

/// SoundCloud downloads: what's going on now, then everything saved earlier.
pub(super) fn downloads(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
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
            let failed: Vec<Track> = rows
                .iter()
                .filter(|r| matches!(r.state, DownloadState::Failed(_)))
                .map(|r| r.track.clone())
                .collect();
            if !failed.is_empty() && widgets::pill(ui, &format!("Retry {} failed", failed.len()), HOVER, TEXT).clicked()
            {
                cx.actions.push(Action::Cmd(Command::Download(failed)));
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

pub(super) fn download_row(ui: &mut Ui, cx: &mut Cx, row: &DownloadRow, tracks: &[Track], i: usize) {
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
