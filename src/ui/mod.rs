//! The egui front-end.

mod art;
mod panels;
mod picker;
mod profile;
mod settings;
mod tag_editor;
pub mod theme;
mod views;
mod widgets;

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Color32, Key, Modifiers, Rect};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{Config, Paths};
use crate::library::Library;
use crate::model::Track;
use crate::service::{Command, Feed, PlayStatus, PlayerView, Shared};

use art::ArtCache;

#[derive(Debug, Clone, PartialEq)]
pub enum View {
    Home,
    Search,
    /// Local files.
    Songs,
    Albums,
    /// Every artist in the library.
    Artists,
    Album(String),
    Playlist(String),
    /// An artist in the library, by key.
    Artist(String),
    /// A Spotify / SoundCloud / Apple Music page, by page key or link.
    Page(String),
    NowPlaying,
    Downloads,
    Settings,
    /// Last.fm stats of the signed-in account.
    Profile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightTab {
    NowPlaying,
    Lyrics,
    Queue,
}

/// Things widgets ask for; applied after the frame is drawn.
#[derive(Debug, Clone)]
pub enum Action {
    Cmd(Command),
    Go(View),
    Back,
    Forward,
    /// Open a page key (`local:artist:…`, `spotify:artist:…`, …) or a pasted link.
    Open(String),
    /// Search everywhere for this text.
    Search(String),
    NewPlaylist(Vec<Track>),
    Rename(String, String),
    Delete(String),
    OpenUrl(String),
    RightTab(RightTab),
    ToggleRightPanel,
    ToggleLibrary,
    /// Opens the tag editor for these songs (the local ones).
    EditTags(Vec<Track>),
    /// Asks before importing a settings file.
    ImportSettings(std::path::PathBuf),
}

/// Per-frame context handed to every view.
pub struct Cx<'a> {
    pub lib: &'a Library,
    pub player: &'a PlayerView,
    pub feed: &'a Feed,
    pub art: &'a mut ArtCache,
    /// Buttons and the playing song.
    pub accent: Color32,
    /// Background colour taken from the current cover (or a neutral grey).
    pub tint: Color32,
    pub actions: &'a mut Vec<Action>,
    /// The app logo (same artwork as the window and launcher icon).
    pub logo: egui::TextureId,
}

/// Largest texture egui may make (its glyph atlas grows up to this square).
const MAX_TEXTURE_SIDE: usize = 2048;

/// Background tint when there is no cover colour to use.
const NEUTRAL_TINT: Color32 = Color32::from_rgb(0x53, 0x53, 0x53);

enum Dialog {
    NewPlaylist {
        name: String,
        tracks: Vec<Track>,
    },
    Rename {
        id: String,
        name: String,
    },
    /// Delete a playlist, or remove a saved album (`album`: where it's from).
    Delete {
        id: String,
        name: String,
        album: Option<crate::model::Source>,
    },
    ImportSettings {
        path: std::path::PathBuf,
    },
    /// The password to install a downloaded update with (Linux).
    UpdatePassword {
        password: String,
        wrong: bool,
    },
}

/// What the `?` overlay lists.
const SHORTCUTS: &[(&str, &str)] = &[
    ("Space", "Play / pause"),
    ("← / →", "Back / forward 5 seconds"),
    ("Ctrl ← / Ctrl →", "Previous / next song"),
    ("↑ / ↓", "Volume up / down"),
    ("M", "Mute"),
    ("S", "Shuffle on / off"),
    ("R", "Repeat: all, one, off"),
    ("H", "Like the playing song"),
    ("L", "Full screen lyrics"),
    ("/ or Ctrl F", "Search"),
    ("Alt ← / Alt →", "Back / forward a page"),
    ("Ctrl A / C / X / V", "Pick all, copy, cut, paste songs"),
    ("Delete", "Remove picked songs from your playlist"),
    ("Ctrl + / − / 0", "Bigger / smaller / normal interface"),
    ("Ctrl Q", "Quit"),
    ("?", "This list"),
];

/// The interface scale's range (Settings, and Ctrl +/−).
pub const SCALE: (f32, f32) = (0.6, 2.5);

pub struct App {
    /// The zoom last seen, to notice Ctrl +/− changing it.
    zoom: f32,
    /// The `?` shortcut list is open.
    show_keys: bool,
    shared: Arc<Shared>,
    cmd: UnboundedSender<Command>,
    pub(crate) cfg: Config,
    sent_cfg: Config,
    cfg_changed_at: Option<Instant>,
    paths: Paths,
    art: ArtCache,
    logo: egui::TextureHandle,
    view: View,
    history: Vec<View>,
    forward: Vec<View>,
    right_tab: RightTab,
    /// The right panel floating over the page (when the window is too narrow for it beside).
    right_overlay: bool,
    /// Whether the right panel fits beside the page this frame.
    right_fits: bool,
    accent: Color32,
    styled_accent: Color32,
    tint: Color32,
    search_text: String,
    search_sent: String,
    search_changed_at: Option<Instant>,
    search_cache: (String, u64, Vec<String>),
    filter_text: String,
    /// The page last asked of the service (see `View::Page`).
    requested_page: String,
    /// Bumped to make the sidebar panel start over at its saved width (collapse / expand).
    sidebar_epoch: u32,
    /// The view drawn last frame and when it changed, for the fade between views.
    shown_view: View,
    view_changed_at: Instant,
    dialog: Option<Dialog>,
    tag_editor: Option<tag_editor::TagEditor>,
    /// Closing to start again with imported settings.
    restarting: bool,
    /// A new version the user put off for now.
    update_dismissed: String,
    /// The last password request (`UpdateStatus::NeedsPassword`) a dialog was opened for.
    password_asked: u32,
    /// The last `Feed::art_changed` number handled.
    art_changed_seen: u64,
    rt: tokio::runtime::Handle,
    settings: settings::SettingsState,
    cjk_loaded: bool,
    cjk_checked_version: u64,
    focus_search: bool,
    mem_checked: Instant,
    trimmed_at: Instant,
    pub(crate) rss_mb: f32,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shared: Arc<Shared>,
        cmd: UnboundedSender<Command>,
        cfg: Config,
        paths: Paths,
        rt: tokio::runtime::Handle,
    ) -> App {
        let ctx = cc.egui_ctx.clone();
        let _ = shared.ctx.set(ctx.clone());
        let accent = Color32::from_rgb(cfg.ui.accent[0], cfg.ui.accent[1], cfg.ui.accent[2]);
        theme::setup_fonts(&ctx, false);
        theme::apply_style(&ctx, accent);
        let zoom = cfg.ui.scale.clamp(SCALE.0, SCALE.1);
        ctx.set_zoom_factor(zoom);
        let art = ArtCache::new(rt.clone(), paths.art_cache(), cfg.ui.art_cache_size);
        let logo = theme::load_logo(&ctx);
        // Windows' media overlay and keys are tied to the window.
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(handle) = cc.window_handle() {
                if let RawWindowHandle::Win32(win) = handle.as_raw() {
                    let _ = cmd.send(Command::AttachWindow(win.hwnd.get()));
                }
            }
        }
        App {
            shared,
            cmd,
            sent_cfg: cfg.clone(),
            cfg,
            cfg_changed_at: None,
            paths,
            art,
            logo,
            view: View::Home,
            history: Vec::new(),
            forward: Vec::new(),
            right_tab: RightTab::NowPlaying,
            right_overlay: false,
            right_fits: true,
            zoom,
            show_keys: false,
            accent,
            styled_accent: accent,
            tint: NEUTRAL_TINT,
            search_text: String::new(),
            search_sent: String::new(),
            search_changed_at: None,
            search_cache: (String::new(), 0, Vec::new()),
            filter_text: String::new(),
            requested_page: String::new(),
            sidebar_epoch: 0,
            shown_view: View::Home,
            view_changed_at: Instant::now() - Duration::from_secs(1),
            dialog: None,
            tag_editor: None,
            restarting: false,
            update_dismissed: String::new(),
            password_asked: 0,
            art_changed_seen: 0,
            rt,
            settings: settings::SettingsState::default(),
            cjk_loaded: false,
            cjk_checked_version: u64::MAX,
            focus_search: false,
            mem_checked: Instant::now() - Duration::from_secs(60),
            trimmed_at: Instant::now(),
            rss_mb: 0.0,
        }
    }

    fn send(&self, c: Command) {
        let _ = self.cmd.send(c);
    }

    fn go(&mut self, v: View) {
        if v != self.view {
            self.history.push(std::mem::replace(&mut self.view, v));
            if self.history.len() > 50 {
                self.history.remove(0);
            }
            self.forward.clear();
            self.filter_text.clear();
            if self.view == View::Search {
                self.focus_search = true;
            }
        }
    }

    fn back(&mut self) {
        if let Some(v) = self.history.pop() {
            self.forward.push(std::mem::replace(&mut self.view, v));
            self.filter_text.clear();
        }
    }

    fn forward(&mut self) {
        if let Some(v) = self.forward.pop() {
            self.history.push(std::mem::replace(&mut self.view, v));
            self.filter_text.clear();
        }
    }

    /// Opens a page key or link: library artists directly, everything else via the service.
    fn open(&mut self, key: &str) {
        let key = key.trim();
        if let Some(name) = key.strip_prefix("local:artist:") {
            self.go(View::Artist(name.to_lowercase()));
        } else if !key.is_empty() {
            self.go(View::Page(key.to_string()));
            self.request_page(key);
        }
    }

    fn request_page(&mut self, key: &str) {
        if self.requested_page != key {
            self.requested_page = key.to_string();
            self.send(Command::OpenPage(key.to_string()));
        }
    }

    fn search_for(&mut self, text: String) {
        self.search_text = text.trim().to_string();
        self.search_sent = self.search_text.clone();
        self.search_changed_at = None;
        self.send(Command::Search(self.search_sent.clone()));
        self.go(View::Search);
        self.focus_search = false;
    }

    fn apply(&mut self, ctx: &egui::Context, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Cmd(c) => {
                    // Settings typed a moment ago (e.g. a redirect URI right before clicking
                    // Authorize) must reach the service before the command does.
                    if self.cfg != self.sent_cfg {
                        self.send(Command::UpdateConfig(Box::new(self.cfg.clone())));
                        self.sent_cfg = self.cfg.clone();
                        self.cfg_changed_at = None;
                    }
                    self.send(c)
                }
                Action::Go(v) => self.go(v),
                Action::Back => self.back(),
                Action::Forward => self.forward(),
                Action::Open(key) => {
                    if key == self.requested_page {
                        // Opening the same page again (e.g. "Try again") reloads it.
                        self.requested_page.clear();
                    }
                    self.open(&key);
                }
                Action::Search(text) => self.search_for(text),
                Action::NewPlaylist(tracks) => {
                    self.dialog = Some(Dialog::NewPlaylist {
                        name: String::new(),
                        tracks,
                    })
                }
                Action::Rename(id, name) => self.dialog = Some(Dialog::Rename { id, name }),
                Action::Delete(id) => {
                    let (name, album) = self
                        .shared
                        .library
                        .read()
                        .unwrap()
                        .playlist(&id)
                        .map(|p| (p.name.clone(), p.kind.is_album().then(|| p.kind.source()).flatten()))
                        .unwrap_or_default();
                    self.dialog = Some(Dialog::Delete { id, name, album });
                }
                Action::OpenUrl(url) => {
                    if can_open(&url) {
                        let _ = open::that_detached(url);
                    } else {
                        tracing::warn!("not opening {url}: only web pages and folders are opened");
                    }
                }
                Action::RightTab(t) if !self.right_fits => {
                    // No room beside the page: it opens over it, just for now.
                    self.right_overlay = !(self.right_overlay && self.right_tab == t);
                    self.right_tab = t;
                }
                Action::RightTab(t) => {
                    if self.cfg.ui.show_right_panel && self.right_tab == t {
                        self.cfg.ui.show_right_panel = false;
                    } else {
                        self.right_tab = t;
                        self.cfg.ui.show_right_panel = true;
                    }
                    self.cfg_changed_at = Some(Instant::now());
                }
                Action::ToggleRightPanel if self.right_overlay => self.right_overlay = false,
                Action::ToggleRightPanel => {
                    self.cfg.ui.show_right_panel = !self.cfg.ui.show_right_panel;
                    self.cfg_changed_at = Some(Instant::now());
                }
                Action::ImportSettings(path) => {
                    self.dialog = Some(Dialog::ImportSettings { path });
                }
                Action::EditTags(tracks) => {
                    self.tag_editor = tag_editor::TagEditor::open(tracks, &self.rt);
                }
                Action::ToggleLibrary => {
                    self.cfg.ui.collapse_library = !self.cfg.ui.collapse_library;
                    self.sidebar_epoch += 1;
                    self.cfg_changed_at = Some(Instant::now());
                }
            }
        }
        let _ = ctx;
    }

    fn keyboard(&mut self, ctx: &egui::Context, player: &PlayerView) {
        if ctx.text_edit_focused() {
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                ctx.memory_mut(|m| m.stop_text_input());
            }
            return;
        }
        // Clicked buttons/rows keep keyboard focus, which would make Space press them
        // again. Media-player shortcuts win over widget keyboard navigation.
        if let Some(id) = ctx.memory(|m| m.focused()) {
            ctx.memory_mut(|m| m.surrender_focus(id));
        }
        let (space, left, right, up, down, ctrl, f, esc, l, q) = ctx.input(|i| {
            (
                i.key_pressed(Key::Space),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.modifiers.matches_logically(Modifiers::COMMAND),
                i.key_pressed(Key::F) || i.key_pressed(Key::K),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::L),
                i.key_pressed(Key::Q),
            )
        });
        let (alt, mouse_back, mouse_fwd, slash) = ctx.input(|i| {
            (
                i.modifiers.alt,
                i.pointer.button_pressed(egui::PointerButton::Extra1),
                i.pointer.button_pressed(egui::PointerButton::Extra2),
                i.key_pressed(Key::Slash) && !i.modifiers.shift,
            )
        });
        // Plain letters, without Ctrl or Alt.
        let (s, r, m, h, help) = ctx.input(|i| {
            let plain = |k: Key| i.key_pressed(k) && !i.modifiers.command && !i.modifiers.alt;
            let question =
                i.key_pressed(Key::Questionmark) || i.events.iter().any(|e| *e == egui::Event::Text("?".into()));
            (plain(Key::S), plain(Key::R), plain(Key::M), plain(Key::H), question)
        });
        if help {
            self.show_keys = !self.show_keys;
        }
        if s {
            self.send(Command::SetShuffle(!player.shuffle));
        }
        if r {
            self.send(Command::CycleRepeat);
        }
        if m {
            self.send(panels::mute_command(ctx, player.volume));
        }
        if h {
            if let Some(t) = &player.current {
                self.send(Command::ToggleLike(t.clone()));
            }
        }
        if mouse_back || (alt && left) {
            self.back();
        }
        if mouse_fwd || (alt && right) {
            self.forward();
        }
        if alt && (left || right) {
            return;
        }
        if space {
            self.send(Command::TogglePause);
        }
        if left {
            self.send(if ctrl {
                Command::Previous
            } else {
                Command::SeekRelative(-5.0)
            });
        }
        if right {
            self.send(if ctrl {
                Command::Next
            } else {
                Command::SeekRelative(5.0)
            });
        }
        if up || down {
            let v = player.volume + if up { 5.0 } else { -5.0 };
            self.send(Command::SetVolume(v));
        }
        if (ctrl && f) || slash {
            self.focus_search = true;
        }
        if l {
            if self.view == View::NowPlaying {
                self.back();
            } else {
                self.go(View::NowPlaying);
            }
        }
        if esc && self.view == View::NowPlaying {
            self.back();
        }
        if ctrl && q {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// The `?` overlay: every keyboard shortcut.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if !self.show_keys {
            return;
        }
        let modal = egui::Modal::new(egui::Id::new("shortcuts")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(egui::RichText::new("Keyboard shortcuts").font(theme::bold_font(18.0)));
            ui.add_space(10.0);
            egui::Grid::new("shortcut-grid")
                .num_columns(2)
                .spacing([24.0, 8.0])
                .show(ui, |ui| {
                    for (keys, what) in SHORTCUTS {
                        ui.label(egui::RichText::new(*keys).color(theme::TEXT).strong());
                        ui.label(egui::RichText::new(*what).color(theme::TEXT_DIM));
                        ui.end_row();
                    }
                });
            ui.add_space(10.0);
            if widgets::pill(ui, "Close", theme::CARD, theme::TEXT).clicked() {
                self.show_keys = false;
            }
        });
        if modal.should_close() {
            self.show_keys = false;
        }
    }

    /// Dropped folders join the library, .m3u files and Library.xml get imported.
    fn dropped_files(&mut self, ctx: &egui::Context) {
        let files: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        for path in files {
            let lower = path.to_string_lossy().to_lowercase();
            if path.is_dir() {
                if !self.cfg.library.folders.contains(&path) {
                    self.cfg.library.folders.push(path);
                    self.cfg_changed_at = Some(Instant::now() - Duration::from_secs(5));
                }
            } else if lower.ends_with(".m3u") || lower.ends_with(".m3u8") {
                self.send(Command::ImportM3u(path));
            } else if crate::backup::is_settings_file(&path) {
                self.dialog = Some(Dialog::ImportSettings { path });
            } else if lower.ends_with(".xml") {
                self.send(Command::ImportAppleXml(path));
            }
        }
    }

    /// The accent comes from the settings; the background tint follows the current cover.
    fn update_colors(&mut self, ctx: &egui::Context, player: &PlayerView) {
        let accent = Color32::from_rgb(self.cfg.ui.accent[0], self.cfg.ui.accent[1], self.cfg.ui.accent[2]);
        if accent != self.styled_accent {
            theme::apply_style(ctx, accent);
            self.styled_accent = accent;
        }
        self.accent = accent;
        let target = if self.cfg.ui.dynamic_accent {
            player
                .current
                .as_ref()
                .and_then(|t| {
                    t.art
                        .clone()
                        .or_else(|| player.via.as_ref().and_then(|v| v.art.clone()))
                })
                .and_then(|a| {
                    // Make sure the thumbnail is requested so its colour gets computed.
                    let _ = self.art.get(Some(&a), art::THUMB);
                    self.art.accent(&a)
                })
                .unwrap_or(NEUTRAL_TINT)
        } else {
            NEUTRAL_TINT
        };
        // Fade between covers.
        let anim = |id: &str, v: u8| ctx.animate_value_with_time(egui::Id::new(id), v as f32, 0.6).round() as u8;
        self.tint = Color32::from_rgb(
            anim("tint-r", target.r()),
            anim("tint-g", target.g()),
            anim("tint-b", target.b()),
        );
    }

    /// Saves panel widths the user dragged to. A sidebar dragged narrow snaps to the rail.
    fn remember_sizes(&mut self, ctx: &egui::Context, sidebar: Option<f32>, right: Option<f32>) {
        let ui_cfg = &mut self.cfg.ui;
        let mut changed = false;
        if let Some(w) = sidebar {
            if w < panels::SIDEBAR_COLLAPSE_AT {
                changed |= !ui_cfg.collapse_library;
                ui_cfg.collapse_library = true;
                if (w - panels::SIDEBAR_RAIL).abs() > 0.5 {
                    // Start the panel over at the rail width.
                    self.sidebar_epoch += 1;
                    ctx.request_repaint();
                }
            } else {
                changed |= ui_cfg.collapse_library || (w - ui_cfg.sidebar_width).abs() >= 1.0;
                ui_cfg.collapse_library = false;
                ui_cfg.sidebar_width = w.round();
            }
        }
        if let Some(w) = right {
            if (w - ui_cfg.right_panel_width).abs() >= 1.0 {
                ui_cfg.right_panel_width = w.round();
                changed = true;
            }
        }
        if changed {
            self.cfg_changed_at = Some(Instant::now());
        }
    }

    fn maybe_load_cjk(&mut self, ctx: &egui::Context, lib: &Library) {
        if self.cjk_loaded || self.cjk_checked_version == lib.version {
            return;
        }
        self.cjk_checked_version = lib.version;
        // Only pay for a CJK font when the library actually needs one.
        let needed = lib
            .tracks
            .values()
            .take(20_000)
            .any(|t| theme::has_cjk(&t.title) || theme::has_cjk(&t.artist) || theme::has_cjk(&t.album))
            || lib.playlists.iter().any(|p| theme::has_cjk(&p.name));
        if needed {
            theme::setup_fonts(ctx, true);
            self.cjk_loaded = true;
        }
    }

    /// Keeps the zoom (Ctrl +/− or Settings) and the window's size in the settings.
    fn remember_window(&mut self, ctx: &egui::Context) {
        let zoom = ctx.zoom_factor();
        if zoom != self.zoom {
            let kept = zoom.clamp(SCALE.0, SCALE.1);
            if kept != zoom {
                ctx.set_zoom_factor(kept);
            }
            self.zoom = kept;
            self.cfg.ui.scale = (kept * 100.0).round() / 100.0;
        }
        let (inner, maximized, fullscreen, minimized) = ctx.input(|i| {
            let v = i.viewport();
            (v.inner_rect, v.maximized, v.fullscreen, v.minimized)
        });
        if fullscreen == Some(true) || minimized == Some(true) {
            return;
        }
        if let Some(maximized) = maximized {
            self.cfg.ui.maximized = maximized;
            if maximized {
                // The size to go back to is the one from before.
                return;
            }
        }
        if let Some(r) = inner {
            // In screen points, as the window is made with: without the zoom.
            let size = [(r.width() * self.zoom).round(), (r.height() * self.zoom).round()];
            if size[0] >= 200.0 && size[1] >= 200.0 && self.cfg.ui.window_size != Some(size) {
                self.cfg.ui.window_size = Some(size);
            }
        }
    }

    /// Saves settings shortly after the last change.
    fn sync_config(&mut self, ctx: &egui::Context) {
        if self.cfg != self.sent_cfg {
            let changed = *self.cfg_changed_at.get_or_insert_with(Instant::now);
            let delay = Duration::from_millis(700);
            if changed.elapsed() > delay {
                self.send(Command::UpdateConfig(Box::new(self.cfg.clone())));
                self.sent_cfg = self.cfg.clone();
                self.cfg_changed_at = None;
            } else {
                // Nothing else may redraw (and so save) once the mouse stops moving.
                ctx.request_repaint_after(delay.saturating_sub(changed.elapsed()) + Duration::from_millis(20));
            }
        } else {
            self.cfg_changed_at = None;
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        // A downloaded update waits for the password.
        let asked = match self.shared.feed.read().unwrap().update.status {
            crate::service::UpdateStatus::NeedsPassword { attempt, wrong } => Some((attempt, wrong)),
            _ => None,
        };
        if let Some((attempt, wrong)) = asked.filter(|(attempt, _)| *attempt > self.password_asked) {
            self.password_asked = attempt;
            self.dialog = Some(Dialog::UpdatePassword {
                password: String::new(),
                wrong,
            });
        }
        let Some(dialog) = self.dialog.as_mut() else { return };
        let mut close = false;
        let mut submit: Option<Command> = None;
        // Read before the text fields take the key for themselves.
        let enter_pressed = ctx.input(|i| i.key_pressed(Key::Enter));
        let modal = egui::Modal::new(egui::Id::new("dialog")).show(ctx, |ui| {
            ui.set_width(380.0);
            match dialog {
                Dialog::NewPlaylist { name, tracks } => {
                    ui.label(egui::RichText::new("New playlist").font(theme::bold_font(18.0)));
                    ui.add_space(8.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("Playlist name")
                            .desired_width(f32::INFINITY),
                    );
                    r.request_focus();
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let enter = enter_pressed;
                        if (widgets::pill(ui, "Create", self.accent, theme::on_color(self.accent)).clicked() || enter)
                            && !name.trim().is_empty()
                        {
                            submit = Some(Command::CreatePlaylist {
                                name: name.trim().to_string(),
                                tracks: std::mem::take(tracks),
                            });
                        }
                        if widgets::pill(ui, "Cancel", theme::CARD, theme::TEXT).clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Rename { id, name } => {
                    ui.label(egui::RichText::new("Rename playlist").font(theme::bold_font(18.0)));
                    ui.add_space(8.0);
                    let r = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    r.request_focus();
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let enter = enter_pressed;
                        if (widgets::pill(ui, "Save", self.accent, theme::on_color(self.accent)).clicked() || enter)
                            && !name.trim().is_empty()
                        {
                            submit = Some(Command::RenamePlaylist {
                                playlist_id: id.clone(),
                                name: name.trim().to_string(),
                            });
                        }
                        if widgets::pill(ui, "Cancel", theme::CARD, theme::TEXT).clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::ImportSettings { path } => {
                    ui.label(egui::RichText::new("Import settings?").font(theme::bold_font(18.0)));
                    ui.add_space(6.0);
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ui.label(format!(
                        "Your settings are replaced with the ones in “{name}”, its playlists are added, and \
                         Sumo restarts."
                    ));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if widgets::pill(ui, "Import and restart", self.accent, theme::on_color(self.accent)).clicked()
                        {
                            submit = Some(Command::ImportSettings(path.clone()));
                        }
                        if widgets::pill(ui, "Cancel", theme::CARD, theme::TEXT).clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::UpdatePassword { password, wrong } => {
                    let version = self
                        .shared
                        .feed
                        .read()
                        .unwrap()
                        .update
                        .available
                        .as_ref()
                        .map(|r| r.version.clone())
                        .unwrap_or_default();
                    ui.label(egui::RichText::new(format!("Install Sumo {version}")).font(theme::bold_font(18.0)));
                    ui.add_space(6.0);
                    ui.label(
                        "The new version is downloaded and checked. Enter your password to install it \
                         (with sudo pacman); Sumo then starts again.",
                    );
                    ui.add_space(10.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(password)
                            .id(PASSWORD_FIELD.into())
                            .password(true)
                            .hint_text("Your password")
                            .desired_width(f32::INFINITY),
                    );
                    r.request_focus();
                    // The field's undo history would keep copies of what was typed.
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), r.id) {
                        state.clear_undoer();
                        egui::TextEdit::store_state(ui.ctx(), r.id, state);
                    }
                    if *wrong {
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new("That password didn't work. Try again.").color(theme::DANGER));
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let enter = enter_pressed;
                        if (widgets::pill(ui, "Install", self.accent, theme::on_color(self.accent)).clicked() || enter)
                            && !password.is_empty()
                        {
                            let typed = std::mem::take(password);
                            submit = Some(Command::AuthorizeUpdate(crate::updater::Secret::new(typed)));
                        }
                        if widgets::pill(ui, "Cancel", theme::CARD, theme::TEXT).clicked() {
                            submit = Some(Command::CancelUpdate);
                        }
                    });
                }
                Dialog::Delete { id, name, album } => {
                    let (title, text, button) = match album {
                        Some(crate::model::Source::Spotify) => (
                            "Remove album?",
                            format!("“{name}” will be removed from your albums and your Spotify library."),
                            "Remove",
                        ),
                        Some(_) => (
                            "Remove album?",
                            format!("“{name}” will be removed from your albums."),
                            "Remove",
                        ),
                        None => (
                            "Delete playlist?",
                            format!("“{name}” will be removed from Sumo. Songs stay in your library."),
                            "Delete",
                        ),
                    };
                    ui.label(egui::RichText::new(title).font(theme::bold_font(18.0)));
                    ui.add_space(6.0);
                    ui.label(text);
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if widgets::pill(ui, button, theme::DANGER, Color32::WHITE).clicked() {
                            submit = Some(Command::DeletePlaylist(id.clone()));
                        }
                        if widgets::pill(ui, "Cancel", theme::CARD, theme::TEXT).clicked() {
                            close = true;
                        }
                    });
                }
            }
        });
        if modal.should_close() {
            close = true;
            // Closing the password dialog (Escape, a click outside) cancels the update.
            if matches!(self.dialog, Some(Dialog::UpdatePassword { .. })) {
                submit.get_or_insert(Command::CancelUpdate);
            }
        }
        if let Some(c) = submit {
            if let Command::DeletePlaylist(id) = &c {
                if self.view == View::Playlist(id.clone()) {
                    self.view = View::Home;
                    let gone = View::Playlist(id.clone());
                    self.history.retain(|v| *v != gone);
                    self.forward.retain(|v| *v != gone);
                }
            }
            self.send(c);
            close = true;
        }
        if close {
            if let Some(Dialog::UpdatePassword { password, .. }) = self.dialog.as_mut() {
                crate::updater::wipe(password);
                ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(PASSWORD_FIELD.into()));
            }
            self.dialog = None;
        }
    }

    /// The player runs in the background; if it stopped (a bug), nothing would respond anymore.
    /// Say so and offer to start Sumo again instead of leaving dead buttons.
    fn service_stopped(&mut self, ctx: &egui::Context) {
        if !self.cmd.is_closed() || self.restarting {
            return;
        }
        egui::Modal::new(egui::Id::new("service-stopped")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(egui::RichText::new("Sumo's player stopped").font(theme::bold_font(18.0)));
            ui.add_space(8.0);
            ui.label("Something went wrong in the background, so playback and the buttons won't respond. Your library and playlists are safe.");
            ui.add_space(12.0);
            if widgets::pill(ui, "Restart Sumo", self.accent, theme::on_color(self.accent)).clicked() {
                self.restarting = true;
                self.shared.feed.write().unwrap().restart = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }

    fn tag_editor(&mut self, ctx: &egui::Context) {
        let shared = self.shared.clone();
        let feed = shared.feed.read().unwrap();
        // Covers that were just changed are loaded again.
        if feed.art_changed.0 != self.art_changed_seen {
            self.art_changed_seen = feed.art_changed.0;
            for src in &feed.art_changed.1 {
                self.art.forget(src);
            }
        }
        let Some(editor) = self.tag_editor.as_mut() else { return };
        let mut commands = Vec::new();
        let open = editor.show(ctx, &mut self.art, &feed, self.accent, &mut commands);
        drop(feed);
        for c in commands {
            self.send(c);
        }
        if !open {
            self.tag_editor = None;
        }
    }

    fn toasts(&self, ctx: &egui::Context, feed: &Feed) {
        if feed.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -100.0))
            .interactable(false)
            .show(ctx, |ui| {
                for t in feed.toasts.iter().rev().take(3) {
                    let fill = match t.kind {
                        crate::service::ToastKind::Info => Color32::from_rgb(0x2e, 0x77, 0xd0),
                        crate::service::ToastKind::Error => Color32::from_rgb(0xc4, 0x23, 0x35),
                    };
                    egui::Frame::new()
                        .fill(fill)
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(16, 10))
                        .shadow(egui::epaint::Shadow {
                            offset: [0, 4],
                            blur: 16,
                            spread: 0,
                            color: Color32::from_black_alpha(120),
                        })
                        .show(ui, |ui| {
                            ui.set_max_width(520.0);
                            ui.label(egui::RichText::new(&t.text).color(Color32::WHITE));
                        });
                    ui.add_space(6.0);
                }
            });
    }

    fn check_memory(&mut self, ctx: &egui::Context) {
        if self.mem_checked.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.mem_checked = Instant::now();
        // Freed cover images and layouts go back to the system now and then.
        if self.trimmed_at.elapsed() > Duration::from_secs(30) {
            self.trimmed_at = Instant::now();
            crate::memory::trim();
        }
        self.rss_mb = crate::tools::memory_mb();
        tracing::debug!(
            "memory: rss {:.0} MB, font atlas {:?}, cached covers {}",
            self.rss_mb,
            ctx.fonts(|f| f.font_image_size()),
            self.art.len()
        );
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.art.poll(&ctx);
        self.dropped_files(&ctx);

        let shared = self.shared.clone();
        if shared.feed.read().unwrap().quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        {
            let mut feed = shared.feed.write().unwrap();
            if std::mem::take(&mut feed.raise) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            // Imported settings: take them over (so closing doesn't save the old ones) and
            // restart; `main` starts the app again once everything has shut down.
            if let Some(cfg) = feed.imported_settings.as_deref().filter(|_| !self.restarting) {
                self.cfg = cfg.clone();
                self.sent_cfg = cfg.clone();
                self.restarting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        let player_guard = shared.player.read().unwrap();
        let player: &PlayerView = &player_guard;
        self.update_colors(&ctx, player);
        self.keyboard(&ctx, player);
        let lib_guard = shared.library.read().unwrap();
        let feed_guard = shared.feed.read().unwrap();
        self.maybe_load_cjk(&ctx, &lib_guard);

        let mut actions = Vec::new();
        let mut resized: Option<(Option<f32>, Option<f32>)> = None;
        if self.shown_view != self.view {
            self.shown_view = self.view.clone();
            self.view_changed_at = Instant::now();
        }
        // New views fade in over a few frames.
        let fade = (self.view_changed_at.elapsed().as_secs_f32() / 0.16).min(1.0);
        if fade < 1.0 {
            ctx.request_repaint();
        }
        {
            // Window background.
            ui.painter().rect_filled(ui.max_rect(), 0.0, theme::WINDOW_BG);
            let width = ui.available_width();
            // Narrow windows get the icon sidebar and no right panel.
            let narrow = width < 820.0;
            let collapsed = self.cfg.ui.collapse_library || narrow;
            let sidebar_w = if collapsed {
                panels::SIDEBAR_RAIL
            } else {
                self.cfg.ui.sidebar_width
            };
            let right_w = self
                .cfg
                .ui
                .right_panel_width
                .clamp(panels::RIGHT_MIN, panels::RIGHT_MAX);
            self.right_fits = width - sidebar_w - right_w - 2.0 * panels::GAP >= 420.0;
            let show_right = self.cfg.ui.show_right_panel && self.view != View::NowPlaying && self.right_fits;
            if self.right_fits || self.view == View::NowPlaying {
                self.right_overlay = false;
            }
            let overlay = self.right_overlay;
            let right_tab = self.right_tab;

            let mut cx = Cx {
                lib: &lib_guard,
                player,
                feed: &feed_guard,
                art: &mut self.art,
                accent: self.accent,
                tint: self.tint,
                actions: &mut actions,
                logo: self.logo.id(),
            };

            panels::player_dock(ui, &mut cx, (show_right || overlay).then_some(right_tab));
            let mut side = panels::SidebarState {
                view: &self.view,
                search_text: &mut self.search_text,
                focus_search: &mut self.focus_search,
                collapsed,
            };
            let sidebar_size = if narrow {
                panels::SidebarSize {
                    id: egui::Id::new("sidebar-narrow"),
                    default: None,
                }
            } else {
                panels::SidebarSize {
                    id: egui::Id::new(("sidebar", self.sidebar_epoch)),
                    default: Some(sidebar_w),
                }
            };
            let sidebar_used = panels::sidebar(ui, &mut cx, &mut side, sidebar_size);
            let collapsed = side.collapsed;
            let right_used = show_right.then(|| panels::right_panel(ui, &mut cx, right_tab, right_w));
            // Remember dragged sizes once the mouse is released.
            if !ctx.input(|i| i.pointer.any_down()) {
                resized = Some(((!narrow).then_some(sidebar_used), right_used));
            }
            let (can_back, can_forward) = (!self.history.is_empty(), !self.forward.is_empty());
            let page_area = ui.available_rect_before_wrap();
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(theme::PANEL)
                        .corner_radius(egui::CornerRadius::same(theme::RADIUS))
                        .outer_margin(egui::Margin {
                            left: panels::GAP as i8,
                            right: panels::GAP as i8,
                            top: panels::GAP as i8,
                            bottom: 0,
                        }),
                )
                .show(ui, |ui| {
                    ui.multiply_opacity(1.0 - (1.0 - fade).powi(2));
                    if self.view != View::NowPlaying {
                        // The icon-only sidebar has no room for the search field: it moves up here.
                        let mut search = collapsed.then_some(panels::SidebarState {
                            view: &self.view,
                            search_text: &mut self.search_text,
                            focus_search: &mut self.focus_search,
                            collapsed,
                        });
                        panels::toolbar(ui, &mut cx, can_back, can_forward, search.as_mut());
                    }
                    let skipped_before = self.cfg.updates.skipped.clone();
                    panels::update_bar(ui, &mut cx, &mut self.cfg.updates.skipped, &mut self.update_dismissed);
                    if self.cfg.updates.skipped != skipped_before {
                        self.cfg_changed_at = Some(Instant::now());
                    }
                    if self.view == View::Settings {
                        settings::show(ui, &mut cx, &mut self.cfg, &mut self.settings, &self.paths, self.rss_mb);
                    } else if self.view == View::Profile {
                        let before = self.cfg.ui.profile;
                        profile::page(ui, &mut cx, &mut self.cfg.ui.profile);
                        if self.cfg.ui.profile != before {
                            self.cfg_changed_at = Some(Instant::now());
                        }
                    } else {
                        let custom = self.cfg.downloads.folder.trim();
                        let download_dir = if custom.is_empty() {
                            self.cfg.library_root()
                        } else {
                            crate::config::expand_home(custom)
                        };
                        let mut state = views::ViewState {
                            view: &self.view,
                            download_dir: &download_dir,
                            download_custom: !custom.is_empty(),
                            ytdlp: self
                                .cfg
                                .downloads
                                .youtube
                                .then_some(self.cfg.downloads.ytdlp_path.as_str()),
                            search_text: &self.search_text,
                            search_cache: &mut self.search_cache,
                            filter_text: &mut self.filter_text,
                        };
                        views::show(ui, &mut cx, &mut state);
                    }
                });
            if overlay {
                let area = Rect::from_min_max(
                    egui::pos2(page_area.left() + panels::GAP, page_area.top() + panels::GAP),
                    egui::pos2(page_area.right() - panels::GAP, page_area.bottom()),
                );
                panels::right_overlay(&ctx, &mut cx, right_tab, area);
                // A click on the page (or Escape) puts it away.
                let width = panels::RIGHT_MIN.max(area.width().min(360.0));
                let on_page = Rect::from_min_max(area.min, egui::pos2(area.right() - width, area.bottom()));
                let away = ctx.input(|i| {
                    i.key_pressed(Key::Escape)
                        || (i.pointer.any_click() && i.pointer.interact_pos().is_some_and(|p| on_page.contains(p)))
                });
                if away {
                    self.right_overlay = false;
                }
            }
        }
        self.toasts(&ctx, &feed_guard);
        if let Some((sidebar, right)) = resized {
            self.remember_sizes(&ctx, sidebar, right);
        }

        // Pages are loaded by the service; ask again after going back / forward to one.
        if let View::Page(key) = &self.view {
            let key = key.clone();
            self.request_page(&key);
        }
        // A library artist's page also shows their songs on Spotify and SoundCloud.
        if let View::Artist(key) = &self.view {
            // The library is already locked for this frame: locking it again could wait on a
            // scan that waits on us.
            let name = lib_guard.artist(key).map(|a| a.name.clone());
            if let Some(name) = name {
                self.request_page(&format!("artist:{name}"));
            }
        }

        // Debounced remote search while typing (links open their page instead).
        let typed = self.search_text.trim().to_string();
        if self.view == View::Search && typed != self.search_sent && !crate::links::looks_like_link(&typed) {
            let changed = *self.search_changed_at.get_or_insert_with(Instant::now);
            if changed.elapsed() > Duration::from_millis(200) {
                self.search_sent = typed;
                self.search_changed_at = None;
                actions.push(Action::Cmd(Command::Search(self.search_sent.clone())));
            } else {
                ctx.request_repaint_after(
                    Duration::from_millis(200).saturating_sub(changed.elapsed()) + Duration::from_millis(10),
                );
            }
        }

        // Repaint pacing: only while something moves.
        if player.status == PlayStatus::Playing {
            let fast = self.view == View::NowPlaying
                || (self.cfg.ui.show_right_panel
                    && self.right_tab != RightTab::Queue
                    && feed_guard.lyrics.lyrics.is_some());
            ctx.request_repaint_after(Duration::from_millis(if fast { 100 } else { 500 }));
        } else if player.status == PlayStatus::Loading
            || feed_guard.scan.is_some()
            || feed_guard.page.loading
            || feed_guard.search.pending()
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        drop(feed_guard);
        drop(lib_guard);
        drop(player_guard);

        self.apply(&ctx, actions);
        self.dialogs(&ctx);
        self.shortcuts(&ctx);
        self.service_stopped(&ctx);
        self.tag_editor(&ctx);
        self.remember_window(&ctx);
        self.sync_config(&ctx);
        self.check_memory(&ctx);

        if ctx.input(|i| i.viewport().close_requested()) {
            // Persist UI settings right away on exit.
            if self.cfg != self.sent_cfg {
                self.send(Command::UpdateConfig(Box::new(self.cfg.clone())));
                self.sent_cfg = self.cfg.clone();
            }
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // egui makes its glyph atlas as wide as the largest texture the GPU takes (often
        // 16384 px), and every row of glyphs is as tall as the tallest one in it, so one big
        // cover letter reserves a row that wide. A narrower atlas holds the same glyphs in a
        // fraction of the memory, in RAM and on the GPU. Covers are far smaller than this.
        if let Some(side) = raw_input.max_texture_side.as_mut() {
            *side = (*side).min(MAX_TEXTURE_SIDE);
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::WINDOW_BG.to_normalized_gamma_f32()
    }
}

/// The update password field (its state is dropped with the dialog).
const PASSWORD_FIELD: &str = "update-password";

/// What Sumo hands to the system to open: web pages and folders, never a program, a file or
/// another kind of link (names and links can come from online services or imported files).
fn can_open(target: &str) -> bool {
    let lower = target.trim().to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return true;
    }
    let path = std::path::Path::new(target);
    // On macOS apps (and other bundles that run) are folders too.
    let runs = path.extension().is_some_and(|e| {
        let e = e.to_string_lossy().to_ascii_lowercase();
        cfg!(target_os = "macos")
            && matches!(
                e.as_str(),
                "app" | "appex" | "prefpane" | "workflow" | "action" | "saver" | "service" | "bundle" | "plugin"
            )
    });
    path.is_absolute() && path.is_dir() && !runs
}

#[cfg(test)]
mod tests {
    #[test]
    fn opens_only_web_pages_and_folders() {
        use super::can_open;
        assert!(can_open("https://open.spotify.com/track/x"));
        assert!(can_open("http://www.last.fm/user/x"));
        assert!(can_open(&std::env::temp_dir().to_string_lossy()));
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "smb://host/share",
            "relative/dir",
            "/bin/sh",
            "",
        ] {
            assert!(!can_open(bad), "{bad}");
        }
    }
}
