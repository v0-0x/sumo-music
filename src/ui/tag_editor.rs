//! The tag editor: changes the details (and cover) of local files, one song or several at once.

use std::path::PathBuf;
use std::sync::mpsc;

use egui::{vec2, Color32, Key, Rect, RichText, Sense};
use egui_phosphor::regular as icon;

use super::art::ArtCache;
use super::theme::{self, *};
use super::widgets;
use crate::library::tags::{self, Field, Metadata};
use crate::model::{Source, Track};
use crate::service::{Command, CoverChange, Feed};

const FIELDS: usize = Field::ALL.len();

/// What becomes of the cover.
#[derive(Clone, PartialEq)]
enum Cover {
    Keep,
    Remove,
    /// An image file or link.
    From(String),
}

pub struct TagEditor {
    tracks: Vec<Track>,
    /// Each field as the files had it; `None` where they differ (or before they are read).
    original: [Option<String>; FIELDS],
    values: [String; FIELDS],
    reading: Option<mpsc::Receiver<Result<Vec<Metadata>, String>>>,
    error: Option<String>,
    cover: Cover,
    cover_text: String,
    /// The Spotify lookup waited for.
    lookup: Option<u64>,
    note: Option<String>,
    /// Closing was asked for with edits not saved: asks whether to drop them.
    confirm_close: bool,
}

impl TagEditor {
    /// Opens the editor for the local songs among `tracks`; `None` when there are none.
    pub fn open(tracks: Vec<Track>, rt: &tokio::runtime::Handle) -> Option<TagEditor> {
        let tracks: Vec<Track> = tracks.into_iter().filter(|t| t.source == Source::Local).collect();
        if tracks.is_empty() {
            return None;
        }
        let paths: Vec<PathBuf> = tracks.iter().map(|t| PathBuf::from(&t.uri)).collect();
        let (tx, rx) = mpsc::channel();
        rt.spawn_blocking(move || {
            let read: Result<Vec<Metadata>, String> = paths
                .iter()
                .map(|p| tags::read(p).map_err(|e| format!("{e:#}")))
                .collect();
            let _ = tx.send(read);
        });
        Some(TagEditor {
            tracks,
            original: Default::default(),
            values: Default::default(),
            reading: Some(rx),
            error: None,
            cover: Cover::Keep,
            cover_text: String::new(),
            lookup: None,
            note: None,
            confirm_close: false,
        })
    }

    fn several(&self) -> bool {
        self.tracks.len() > 1
    }

    /// The fields shown: all for one song, the ones songs share for several.
    fn shown(&self) -> impl Iterator<Item = (usize, Field)> + '_ {
        Field::ALL
            .into_iter()
            .enumerate()
            .filter(|(_, f)| !self.several() || f.shared())
    }

    fn take_read(&mut self, read: Result<Vec<Metadata>, String>) {
        let all = match read {
            Ok(all) => all,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        for (i, field) in Field::ALL.into_iter().enumerate() {
            let mut texts = all.iter().map(|m| field.get(m));
            let first = texts.next().unwrap_or_default();
            let same = texts.all(|t| t == first);
            self.original[i] = same.then(|| first.clone());
            self.values[i] = if same { first } else { String::new() };
        }
    }

    /// Fills in what Spotify knows.
    fn take_lookup(&mut self, meta: &Metadata) {
        for (i, field) in Field::ALL.into_iter().enumerate() {
            let value = field.get(meta);
            if !value.is_empty() && (!self.several() || field.shared()) {
                self.values[i] = value;
            }
        }
        if let Some(url) = meta.cover_urls.first() {
            self.cover = Cover::From(url.clone());
        }
        self.note = Some("Filled in from Spotify: check it and save".into());
    }

    /// The fields that changed. For songs whose values differ, only fields typed into count.
    fn changes(&self) -> Vec<(Field, String)> {
        self.shown()
            .filter_map(|(i, field)| {
                let value = self.values[i].trim();
                let changed = match &self.original[i] {
                    Some(old) => value != old.trim(),
                    None => !value.is_empty(),
                };
                changed.then(|| (field, value.to_string()))
            })
            .collect()
    }

    /// Draws the editor. Returns false once it is closed; commands for the service go to `out`.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        art: &mut ArtCache,
        feed: &Feed,
        accent: Color32,
        out: &mut Vec<Command>,
    ) -> bool {
        if let Some(rx) = &self.reading {
            match rx.try_recv() {
                Ok(read) => {
                    self.reading = None;
                    self.take_read(read);
                }
                Err(mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(50)),
                Err(mpsc::TryRecvError::Disconnected) => self.reading = None,
            }
        }
        if let (Some(want), Some((got, result))) = (self.lookup, &feed.tag_lookup) {
            if want == *got {
                self.lookup = None;
                match result {
                    Ok(meta) => self.take_lookup(meta),
                    Err(e) => self.note = Some(e.clone()),
                }
            }
        }
        // An image dropped on the window becomes the cover.
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .find(|p| is_image(&p.to_string_lossy()))
        });
        if let Some(path) = dropped {
            self.cover = Cover::From(path.to_string_lossy().to_string());
        }

        // Read before the text fields, which take Enter for themselves.
        let save_key = ctx.input(|i| i.modifiers.command && i.key_pressed(Key::Enter));
        let mut open = true;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::new("tag-editor")).show(ctx, |ui| {
            ui.set_width(580.0);
            let title = if self.several() {
                format!("Edit {} songs", self.tracks.len())
            } else {
                "Edit song details".to_string()
            };
            ui.label(RichText::new(title).font(theme::bold_font(18.0)));
            let sub = if self.several() {
                "Changes go to every song. Fields marked “Several values” stay as they are unless you type in them."
                    .to_string()
            } else {
                self.tracks[0].uri.clone()
            };
            ui.label(RichText::new(sub).small().color(TEXT_FAINT));
            ui.add_space(10.0);
            if let Some(e) = &self.error {
                ui.colored_label(theme::DANGER, e);
                ui.add_space(8.0);
                if widgets::pill(ui, "Close", CARD, TEXT).clicked() {
                    open = false;
                }
                return;
            }
            if self.reading.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading the tags…");
                });
                return;
            }

            // Scrolls when the window (or a big interface scale) leaves too little room.
            let room = (ctx.content_rect().height() - 260.0).max(160.0);
            egui::ScrollArea::vertical()
                .id_salt("tag-editor-scroll")
                .max_height(room)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        self.cover_box(ui, art, accent, out);
                        ui.add_space(12.0);
                        ui.vertical(|ui| self.fields(ui));
                    });
                    if !self.several() {
                        let i = Field::ALL.iter().position(|f| *f == Field::Lyrics).unwrap_or(0);
                        egui::CollapsingHeader::new("Lyrics")
                            .id_salt("tag-editor-lyrics")
                            .default_open(false)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut self.values[i])
                                        .hint_text("Plain text, or LRC lines like [00:12.50] for synced lyrics")
                                        .desired_rows(6)
                                        .desired_width(f32::INFINITY),
                                );
                            });
                    }
                });
            if let Some(note) = &self.note {
                ui.label(RichText::new(note).small().color(TEXT_DIM));
            }
            ui.add_space(10.0);

            let problem = self.shown().find_map(|(i, f)| f.check(&self.values[i]).err());
            if let Some(p) = &problem {
                ui.colored_label(theme::DANGER, p);
                ui.add_space(4.0);
            }
            if self.confirm_close {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Discard your changes?").color(TEXT));
                    ui.add_space(8.0);
                    if widgets::pill(ui, "Discard", theme::DANGER, TEXT).clicked() {
                        open = false;
                    }
                    if widgets::pill(ui, "Keep editing", CARD, TEXT).clicked() {
                        self.confirm_close = false;
                    }
                });
                return;
            }
            ui.horizontal(|ui| {
                let save = widgets::pill(ui, "Save", accent, theme::on_color(accent))
                    .on_hover_text("Ctrl+Enter")
                    .clicked();
                if (save || save_key) && problem.is_none() {
                    let changes = self.changes();
                    let cover = match &self.cover {
                        Cover::Keep => None,
                        Cover::Remove => Some(CoverChange::Remove),
                        Cover::From(src) => Some(CoverChange::From(src.clone())),
                    };
                    if !changes.is_empty() || cover.is_some() {
                        out.push(Command::EditTags {
                            paths: self.tracks.iter().map(|t| PathBuf::from(&t.uri)).collect(),
                            changes,
                            cover,
                        });
                    }
                    open = false;
                }
                if widgets::pill(ui, "Cancel", CARD, TEXT).clicked() {
                    cancel = true;
                }
            });
        });
        // Escape or a click beside the editor closes it too; edits not saved are asked about.
        if cancel || modal.should_close() {
            if self.confirm_close {
                // Escape again: back to editing.
                self.confirm_close = false;
            } else if self.edited() && self.error.is_none() && self.reading.is_none() {
                self.confirm_close = true;
            } else {
                open = false;
            }
        }
        open
    }

    /// Whether anything was changed and not saved yet.
    fn edited(&self) -> bool {
        !self.changes().is_empty() || self.cover != Cover::Keep
    }

    fn fields(&mut self, ui: &mut egui::Ui) {
        let row = |ui: &mut egui::Ui, label: &str, value: &mut String, mixed: bool, width: f32| {
            ui.label(RichText::new(label).color(TEXT_DIM));
            let hint = if mixed { "Several values" } else { "" };
            ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(width));
        };
        let index = |f: Field| Field::ALL.iter().position(|x| *x == f).unwrap_or(0);
        egui::Grid::new("tag-editor-fields")
            .num_columns(2)
            .spacing(vec2(10.0, 6.0))
            .show(ui, |ui| {
                for field in [
                    Field::Title,
                    Field::Artist,
                    Field::Album,
                    Field::AlbumArtist,
                    Field::Genre,
                    Field::Date,
                ] {
                    if self.several() && !field.shared() {
                        continue;
                    }
                    let i = index(field);
                    let mixed = self.original[i].is_none();
                    row(ui, field.label(), &mut self.values[i], mixed, 290.0);
                    ui.end_row();
                }
                // "Track 3 of 12", "Disc 1 of 2".
                for (number, total) in [(Field::Track, Field::TrackTotal), (Field::Disc, Field::DiscTotal)] {
                    let (n, t) = (index(number), index(total));
                    let both = !self.several() || number.shared();
                    let label = if both { number.label() } else { total.label() };
                    ui.label(RichText::new(label).color(TEXT_DIM));
                    ui.horizontal(|ui| {
                        if both {
                            let mixed = self.original[n].is_none();
                            let hint = if mixed { "…" } else { "" };
                            ui.add(
                                egui::TextEdit::singleline(&mut self.values[n])
                                    .hint_text(hint)
                                    .desired_width(48.0),
                            );
                            ui.label(RichText::new("of").color(TEXT_DIM));
                        }
                        let mixed = self.original[t].is_none();
                        let hint = if mixed { "…" } else { "" };
                        ui.add(
                            egui::TextEdit::singleline(&mut self.values[t])
                                .hint_text(hint)
                                .desired_width(48.0),
                        );
                    });
                    ui.end_row();
                }
            });
    }

    fn cover_box(&mut self, ui: &mut egui::Ui, art: &mut ArtCache, accent: Color32, out: &mut Vec<Command>) {
        ui.vertical(|ui| {
            ui.set_width(170.0);
            let (rect, _) = ui.allocate_exact_size(vec2(170.0, 170.0), Sense::hover());
            let src = match &self.cover {
                Cover::Keep => self.tracks[0].art.clone(),
                Cover::Remove => None,
                Cover::From(src) => Some(src.clone()),
            };
            widgets::cover(
                ui,
                art,
                src.as_deref(),
                rect,
                8,
                widgets::track_fallback(&self.tracks[0]),
            );
            if self.cover != Cover::Keep {
                badge(
                    ui,
                    rect,
                    if self.cover == Cover::Remove {
                        "No cover"
                    } else {
                        "New cover"
                    },
                    accent,
                );
            }
            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.cover_text)
                    .hint_text("Image file or link")
                    .desired_width(170.0),
            );
            ui.horizontal(|ui| {
                let text = self.cover_text.trim().to_string();
                if ui.add_enabled(!text.is_empty(), egui::Button::new("Use")).clicked() {
                    self.cover = Cover::From(text);
                }
                if ui.button("Browse…").clicked() {
                    super::picker::open(
                        ui.ctx(),
                        "cover-file",
                        "Cover image",
                        super::picker::Pick::File("Images", &["jpg", "jpeg", "png", "webp"]),
                    );
                }
                match super::picker::take(ui.ctx(), "cover-file") {
                    Some(Ok(p)) => {
                        self.cover_text = p.to_string_lossy().into_owned();
                        self.cover = Cover::From(self.cover_text.clone());
                    }
                    Some(Err(e)) => self.note = Some(e),
                    None => {}
                }
                if ui.button("Remove").clicked() {
                    self.cover = Cover::Remove;
                }
                if self.cover != Cover::Keep && ui.button("Undo").on_hover_text("Keep the cover it has").clicked() {
                    self.cover = Cover::Keep;
                }
            });
            ui.label(RichText::new("or drop an image here").small().color(TEXT_FAINT));
            if !self.several() {
                ui.add_space(6.0);
                if self.lookup.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Asking Spotify…").small());
                    });
                } else if ui
                    .add(
                        egui::Button::new(theme::ic(icon::MAGNIFYING_GLASS, "Look up on Spotify"))
                            .wrap_mode(egui::TextWrapMode::Extend),
                    )
                    .on_hover_text("Fill in the album, date, track numbers and cover from Spotify")
                    .clicked()
                {
                    let request = rand::random::<u64>();
                    self.lookup = Some(request);
                    self.note = None;
                    let names = |f: Field| self.values[Field::ALL.iter().position(|x| *x == f).unwrap_or(0)].clone();
                    let mut track = self.tracks[0].clone();
                    track.title = names(Field::Title);
                    track.artist = names(Field::Artist);
                    out.push(Command::LookUpTags { request, track });
                }
            }
        });
    }
}

fn badge(ui: &egui::Ui, rect: Rect, text: &str, accent: Color32) {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), theme::bold_font(11.0), theme::on_color(accent));
    let r = Rect::from_min_size(rect.left_top() + vec2(6.0, 6.0), galley.size() + vec2(10.0, 4.0));
    ui.painter().rect_filled(r, egui::CornerRadius::same(4), accent);
    ui.painter()
        .galley(r.min + vec2(5.0, 2.0), galley, theme::on_color(accent));
}

fn is_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".webp"].iter().any(|e| lower.ends_with(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(n: usize) -> TagEditor {
        let track = |i: usize| Track {
            album_artist: String::new(),
            disc_no: None,
            id: format!("local:/m/{i}.mp3"),
            source: Source::Local,
            title: format!("Song {i}"),
            artist: "A".into(),
            album: String::new(),
            duration_ms: 0,
            track_no: None,
            art: None,
            uri: format!("/m/{i}.mp3"),
            added_at: 0,
        };
        TagEditor {
            tracks: (0..n).map(track).collect(),
            original: Default::default(),
            values: Default::default(),
            reading: None,
            error: None,
            cover: Cover::Keep,
            cover_text: String::new(),
            lookup: None,
            note: None,
            confirm_close: false,
        }
    }

    fn at(f: Field) -> usize {
        Field::ALL.iter().position(|x| *x == f).unwrap()
    }

    #[test]
    fn unsaved_edits_are_noticed() {
        let mut e = editor(1);
        assert!(!e.edited());
        e.values[at(Field::Genre)] = "Rap".into();
        assert!(e.edited());
        let mut e = editor(1);
        e.cover = Cover::Remove;
        assert!(e.edited());
    }

    #[test]
    fn only_changed_fields_are_written() {
        let mut e = editor(1);
        let meta = Metadata {
            title: "Waster".into(),
            artist: "Bladee".into(),
            track: Some(3),
            ..Default::default()
        };
        e.take_read(Ok(vec![meta]));
        assert!(e.changes().is_empty());
        e.values[at(Field::Album)] = "Red Light".into();
        e.values[at(Field::Track)] = String::new();
        assert_eq!(
            e.changes(),
            vec![(Field::Album, "Red Light".into()), (Field::Track, String::new())]
        );
    }

    #[test]
    fn several_songs_keep_what_differs() {
        let mut e = editor(2);
        let a = Metadata {
            title: "One".into(),
            artist: "Bladee".into(),
            album: "X".into(),
            ..Default::default()
        };
        let b = Metadata {
            title: "Two".into(),
            artist: "Bladee".into(),
            album: "Y".into(),
            ..Default::default()
        };
        e.take_read(Ok(vec![a, b]));
        assert_eq!(e.values[at(Field::Artist)], "Bladee");
        assert_eq!(e.original[at(Field::Album)], None);
        // Titles aren't shown for several songs, and an untouched mixed album stays.
        e.values[at(Field::Title)] = "Same".into();
        assert!(e.changes().is_empty());
        e.values[at(Field::Album)] = "Z".into();
        e.values[at(Field::Genre)] = "Rap".into();
        assert_eq!(
            e.changes(),
            vec![(Field::Album, "Z".into()), (Field::Genre, "Rap".into())]
        );
    }

    #[test]
    fn lookups_fill_in_details() {
        let mut e = editor(1);
        e.take_read(Ok(vec![Metadata {
            title: "waster".into(),
            ..Default::default()
        }]));
        e.take_lookup(&Metadata {
            title: "Waster".into(),
            artist: "Bladee".into(),
            album: "Red Light".into(),
            date: "2018-05-11".into(),
            track: Some(4),
            cover_urls: vec!["https://i.scdn.co/image/x".into()],
            ..Default::default()
        });
        assert_eq!(e.values[at(Field::Album)], "Red Light");
        assert!(e.cover == Cover::From("https://i.scdn.co/image/x".into()));
        assert_eq!(e.changes().len(), 5);
    }
}
