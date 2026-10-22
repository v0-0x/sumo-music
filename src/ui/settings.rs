//! Settings page.

use egui::{Color32, RichText, Ui};
use egui_phosphor::regular as icon;

use super::picker::{self, Pick};
use super::theme::{self, *};
use super::widgets;
use super::{Action, Cx};
use crate::config::{expand_home, Config, Paths, SPOTIFY_DEFAULT_CLIENT_ID};
use crate::model::Source;
use crate::service::{AccountStatus, Command};

#[derive(Default)]
pub struct SettingsState {
    new_folder: String,
    import_path: String,
    /// Settings export: where to, and what to leave out.
    export_to: String,
    /// Off until ticked: an exported file is often sent to someone or synced to the cloud.
    export_keys: bool,
    export_no_playlists: bool,
    /// The settings file to import (the newest one found, to begin with).
    settings_file: String,
    settings_file_looked: bool,
    show_spotify_advanced: bool,
    devices_requested: bool,
}

fn section(ui: &mut Ui, glyph: &str, color: Color32, title: &str, add: impl FnOnce(&mut Ui)) {
    widgets::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(glyph).family(theme::icons()).size(20.0).color(color));
            ui.label(RichText::new(title).font(theme::bold_font(17.0)));
        });
        ui.add_space(8.0);
        add(ui);
    });
    ui.add_space(14.0);
}

fn hint(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(TEXT_FAINT));
}

fn status(ui: &mut Ui, s: &AccountStatus) {
    let (text, color) = match s {
        AccountStatus::Off => ("Not connected".to_string(), TEXT_FAINT),
        AccountStatus::Working(m) => (m.clone(), TEXT_DIM),
        AccountStatus::Connected(name) if name.is_empty() => {
            ("Connected".to_string(), Color32::from_rgb(0x4a, 0xd6, 0x8a))
        }
        AccountStatus::Connected(name) => (format!("Connected as {name}"), Color32::from_rgb(0x4a, 0xd6, 0x8a)),
        AccountStatus::Error(e) => (e.clone(), DANGER),
    };
    ui.horizontal_wrapped(|ui| {
        if matches!(s, AccountStatus::Working(_)) {
            ui.spinner();
        }
        ui.label(RichText::new(text).color(color));
    });
}

fn text_field(ui: &mut Ui, label: &str, value: &mut String, hint_text: &str, password: bool) {
    ui.label(RichText::new(label).color(TEXT_DIM));
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint_text)
            .password(password)
            .desired_width(f32::INFINITY),
    );
    ui.add_space(4.0);
}

pub fn show(ui: &mut Ui, cx: &mut Cx, cfg: &mut Config, st: &mut SettingsState, paths: &Paths, rss_mb: f32) {
    egui::ScrollArea::vertical().id_salt("settings").auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::new().inner_margin(egui::Margin::same(24)).show(ui, |ui| {
            // `set_max_width` can also widen; never go past the panel.
            ui.set_max_width(ui.available_width().min(760.0));
            ui.label(RichText::new("Settings").font(theme::bold_font(32.0)));
            ui.add_space(16.0);

            // ---------------------------------------------------------- library
            section(ui, icon::FOLDER, source_color(Source::Local), "Local library", |ui| {
                let mut remove = None;
                for (i, f) in cfg.library.folders.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if widgets::icon_button(ui, icon::X, 13.0, TEXT_DIM, "Remove folder").clicked() {
                            remove = Some(i);
                        }
                        ui.label(RichText::new(icon::FOLDER_OPEN).family(theme::icons()).color(TEXT_DIM));
                        ui.add(egui::Label::new(f.display().to_string()).truncate());
                    });
                }
                if let Some(i) = remove {
                    cfg.library.folders.remove(i);
                }
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut st.new_folder)
                            .hint_text("~/Music  (or drop a folder onto the window)")
                            .desired_width(ui.available_width() - 160.0),
                    );
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button("Add").clicked() || enter) && !st.new_folder.trim().is_empty() {
                        let p = expand_home(&st.new_folder);
                        if !cfg.library.folders.contains(&p) {
                            cfg.library.folders.push(p);
                        }
                        st.new_folder.clear();
                    }
                    if ui.button(theme::ic(icon::FOLDER_OPEN, "Browse…")).clicked() {
                        picker::open(ui.ctx(), "library-folder", "Add a music folder", Pick::Folder);
                    }
                    match picker::take(ui.ctx(), "library-folder") {
                        Some(Ok(p)) if !cfg.library.folders.contains(&p) => cfg.library.folders.push(p),
                        Some(Err(e)) => cx.actions.push(Action::Cmd(Command::Notify(e))),
                        _ => {}
                    }
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut cfg.library.scan_on_startup, "Rescan on startup");
                    if ui.button(theme::ic(icon::ARROWS_CLOCKWISE, "Rescan now")).clicked() {
                        cx.actions.push(Action::Cmd(Command::Rescan));
                    }
                });
                hint(ui, "Only new or changed files are read again, so rescans are quick.");
            });

            // ---------------------------------------------------------- spotify
            section(ui, icon::SPOTIFY_LOGO, source_color(Source::Spotify), "Spotify", |ui| {
                status(ui, &cx.feed.spotify);
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if cx.feed.spotify_logged_in {
                        if ui.button(theme::ic(icon::ARROWS_CLOCKWISE, "Sync playlists")).clicked() {
                            cx.actions.push(Action::Cmd(Command::SyncSpotify));
                        }
                        if ui.button("Log out").clicked() {
                            cx.actions.push(Action::Cmd(Command::SpotifyLogout));
                        }
                    } else if widgets::pill(ui, "Log in with Spotify", source_color(Source::Spotify), Color32::BLACK).clicked() {
                        cx.actions.push(Action::Cmd(Command::SpotifyLogin));
                    }
                });
                hint(ui, "Opens Spotify's login page in your browser. Playback needs Spotify Premium; free accounts can still import playlists.");
                ui.add_space(6.0);
                ui.checkbox(&mut cfg.spotify.enabled, "Enable Spotify");
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Quality").color(TEXT_DIM));
                    egui::ComboBox::from_id_salt("bitrate")
                        .selected_text(format!("{} kbps", cfg.spotify.bitrate))
                        .show_ui(ui, |ui| {
                            for b in [96u16, 160, 320] {
                                ui.selectable_value(&mut cfg.spotify.bitrate, b, format!("{b} kbps"));
                            }
                            ui.add_enabled(false, egui::Button::selectable(false, "Lossless (FLAC)")).on_disabled_hover_text(
                                "Spotify only sends its lossless FLAC streams to the official Spotify apps. \
                                 Third-party players (librespot) get 320 kbps Ogg Vorbis at most.",
                            );
                        });
                    ui.checkbox(&mut cfg.spotify.normalisation, "Normalize volume");
                });
                ui.checkbox(&mut cfg.spotify.cache_audio, "Cache audio on disk (saves bandwidth, up to 2 GB)");
                // Windows and macOS have one system output that Spotify follows.
                if cfg!(target_os = "linux") {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Audio output").color(TEXT_DIM));
                        let label = |v: &str| match v {
                            "pulseaudio" => "PipeWire / PulseAudio",
                            "alsa" => "ALSA",
                            _ => "Automatic",
                        };
                        egui::ComboBox::from_id_salt("spotify-output")
                            .selected_text(label(&cfg.spotify.audio_output))
                            .show_ui(ui, |ui| {
                                for v in ["auto", "pulseaudio", "alsa"] {
                                    ui.selectable_value(&mut cfg.spotify.audio_output, v.to_string(), label(v));
                                }
                            });
                    });
                    hint(
                        ui,
                        "Automatic uses PipeWire/PulseAudio when it's running (it follows your system's output \
                         device). A change takes effect the next time Spotify playback starts (or after restarting \
                         Sumo).",
                    );
                }
                ui.collapsing("Advanced", |ui| {
                    st.show_spotify_advanced = true;
                    ui.label(RichText::new("Your own Spotify app (recommended for search)").strong());
                    hint(
                        ui,
                        "Your playlists import through Sumo's direct Spotify connection. Search and artist / album \
                         pages use Spotify's Web API, whose shared key is often rate limited (HTTP 429). Fix it with a free \
                         app: developer.spotify.com → Dashboard → Create app → tick \"Web API\". Then copy its Client ID \
                         and Client secret (app → Settings → View client secret) here.",
                    );
                    ui.add_space(4.0);
                    text_field(ui, "Client ID", &mut cfg.spotify.web_api_client_id, "e.g. 1a2b3c4d5e6f…", false);
                    text_field(
                        ui,
                        "Client secret",
                        &mut cfg.spotify.web_api_client_secret,
                        "from your app's page: Settings → View client secret",
                        true,
                    );
                    hint(
                        ui,
                        "With the secret, Sumo uses your app straight away: no browser login and no Redirect URI. \
                         Likes still sync through your normal Spotify login.",
                    );
                    status(ui, &cx.feed.spotify_web_api);
                    ui.add_space(4.0);
                    ui.collapsing("Or log in to your app in the browser (Redirect URI)", |ui| {
                        ui.label(RichText::new("Redirect URI (must match your app's exactly)").color(TEXT_DIM));
                        let default_uri = crate::player::oauth::redirect_uri(cfg.spotify.web_api_redirect_port);
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut cfg.spotify.web_api_redirect_uri)
                                    .hint_text(&default_uri)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(ui.available_width() - 80.0),
                            );
                            if ui.small_button(theme::ic(icon::COPY, "Copy")).clicked() {
                                ui.ctx().copy_text(cfg.spotify.web_api_redirect());
                            }
                        });
                        let redirect_ok = match crate::player::oauth::parse_redirect(&cfg.spotify.web_api_redirect()) {
                            Ok(_) => true,
                            Err(e) => {
                                ui.label(RichText::new(format!("{e}")).size(12.0).color(DANGER));
                                false
                            }
                        };
                        hint(
                            ui,
                            &format!(
                                "Leave it empty to use {default_uri}, or paste the Redirect URI your app already lists. \
                                 Use http:// with 127.0.0.1 (Spotify only wants https for internet addresses)."
                            ),
                        );
                        let ready = !cfg.spotify.web_api_client_id.trim().is_empty() && redirect_ok;
                        if ui
                            .add_enabled(ready, egui::Button::new(theme::ic(icon::SIGN_IN, "Authorize")))
                            .clicked()
                        {
                            cx.actions.push(Action::Cmd(Command::SpotifyWebApiLogin));
                        }
                        hint(
                            ui,
                            "If Spotify says \"redirect_uri: Not matching configuration\", your app doesn't have the \
                             Redirect URI above saved: on developer.spotify.com open the app → Settings → Edit, add it \
                             under Redirect URIs exactly as shown, click Add, then scroll down and click Save (it only \
                             counts after Save). Or skip all this and use the Client secret above.",
                        );
                    });
                    ui.add_space(8.0);
                    ui.label(RichText::new("Login client").strong());
                    text_field(ui, "Login client ID", &mut cfg.spotify.client_id, SPOTIFY_DEFAULT_CLIENT_ID, false);
                    if cfg.spotify.client_id.trim() != SPOTIFY_DEFAULT_CLIENT_ID {
                        ui.label(
                            RichText::new(
                                "Playback and library import only work with Spotify's own ID here. Put your own app's \
                                 Client ID in the field above instead, then click Reset.",
                            )
                            .size(12.0)
                            .color(DANGER),
                        );
                    }
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Login redirect port").color(TEXT_DIM));
                        ui.add(egui::DragValue::new(&mut cfg.spotify.redirect_port).range(1024..=65535));
                        if ui.button("Reset").clicked() {
                            cfg.spotify.client_id = SPOTIFY_DEFAULT_CLIENT_ID.into();
                            cfg.spotify.redirect_port = 8898;
                        }
                    });
                    hint(ui, "Leave the login client on Spotify's own ID: playback only works with it.");
                });
            });

            // ---------------------------------------------------------- soundcloud
            section(ui, icon::SOUNDCLOUD_LOGO, source_color(Source::SoundCloud), "SoundCloud", |ui| {
                status(ui, &cx.feed.soundcloud);
                ui.add_space(4.0);
                ui.checkbox(&mut cfg.soundcloud.enabled, "Enable SoundCloud");
                text_field(ui, "Profile URL", &mut cfg.soundcloud.profile_url, "https://soundcloud.com/yourname", false);
                text_field(ui, "OAuth token (optional, for private likes/playlists)", &mut cfg.soundcloud.oauth_token, "2-123456-…", true);
                hint(ui, "Find it in your browser on soundcloud.com: DevTools → Application → Cookies → oauth_token.");
                ui.add_space(4.0);
                if ui.button(theme::ic(icon::ARROWS_CLOCKWISE, "Sync likes & playlists")).clicked() {
                    cx.actions.push(Action::Cmd(Command::SyncSoundCloud));
                }
            });

            // ---------------------------------------------------------- downloads
            section(ui, icon::DOWNLOAD_SIMPLE, TEXT, "Downloads", |ui| {
                let root = cfg.library_root().to_string_lossy().into_owned();
                ui.label(RichText::new("Download folder").color(TEXT_DIM));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut cfg.downloads.folder)
                            .hint_text(format!("{root}/<service>"))
                            .desired_width(ui.available_width() - 100.0),
                    );
                    if ui.button(theme::ic(icon::FOLDER_OPEN, "Browse…")).clicked() {
                        picker::open(ui.ctx(), "download-folder", "Download folder", Pick::Folder);
                    }
                    match picker::take(ui.ctx(), "download-folder") {
                        Some(Ok(p)) => cfg.downloads.folder = p.to_string_lossy().into_owned(),
                        Some(Err(e)) => cx.actions.push(Action::Cmd(Command::Notify(e))),
                        None => {}
                    }
                });
                ui.add_space(4.0);
                hint(
                    ui,
                    "Empty: a SoundCloud, Spotify or Apple Music folder inside your first library folder, so \
                     downloads show up in Local Files too.",
                );
                ui.add_space(6.0);
                ui.checkbox(&mut cfg.downloads.youtube, "Find Spotify and Apple Music songs on YouTube");
                if cfg.downloads.youtube {
                    text_field(ui, "yt-dlp program", &mut cfg.downloads.ytdlp_path, "yt-dlp", false);
                    let green = Color32::from_rgb(0x4a, 0xd6, 0x8a);
                    match widgets::ytdlp_status(ui, cx, &cfg.downloads.ytdlp_path) {
                        Some(Ok(version)) => {
                            ui.label(RichText::new(format!("yt-dlp {version} is ready")).size(12.5).color(green));
                        }
                        Some(Err(why)) => {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(format!("yt-dlp {why}. {}", crate::tools::install_hint("yt-dlp")))
                                        .size(12.5)
                                        .color(DANGER),
                                );
                                if ui.small_button("Check again").clicked() {
                                    let program = cfg.downloads.ytdlp_path.trim().to_string();
                                    cx.actions.push(Action::Cmd(Command::CheckYtDlp(program)));
                                }
                            });
                        }
                        None => {
                            ui.label(RichText::new("Checking yt-dlp…").size(12.5).color(TEXT_FAINT));
                        }
                    }
                    ui.add_space(4.0);
                    text_field(
                        ui,
                        "Extra yt-dlp options",
                        &mut cfg.downloads.ytdlp_args,
                        "--cookies-from-browser firefox",
                        false,
                    );
                    hint(
                        ui,
                        "If YouTube asks yt-dlp to confirm you're not a bot, --cookies-from-browser firefox (or \
                         chrome) lets it use your browser's YouTube login. With a YouTube Music Premium login that \
                         way, downloads get YouTube's 256 kbps AAC instead of ~160 kbps Opus.",
                    );
                    ui.checkbox(&mut cfg.downloads.youtube_mp3, "Save YouTube downloads as MP3 instead of Opus");
                    hint(
                        ui,
                        "Opus at ~160 kbps already sounds like a high-bitrate MP3; converting can't add quality, \
                         but MP3 plays on every device and player.",
                    );
                }
                hint(
                    ui,
                    "Spotify and Apple Music audio is DRM-protected, so Sumo downloads the same recording \
                     from YouTube (with yt-dlp) or SoundCloud, then tags it with the \
                     song's details from Spotify: album, artists, track and disc number, release date, ISRC, \
                     label, copyright and full-size cover.",
                );
                ui.add_space(6.0);
                ui.checkbox(&mut cfg.downloads.lyrics, "Embed lyrics (time-synced when available)");
            });

            // ---------------------------------------------------------- apple music
            section(ui, icon::APPLE_LOGO, source_color(Source::AppleMusic), "Apple Music", |ui| {
                hint(
                    ui,
                    "Apple Music streams are DRM-protected and can't play on Linux. Sumo imports your library and \
                     playlists, then plays each song from your local files, Spotify or SoundCloud.",
                );
                ui.add_space(6.0);
                ui.label(RichText::new("Import a Library.xml / .m3u file").color(TEXT_DIM));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut st.import_path)
                            .hint_text("~/Downloads/Library.xml")
                            .desired_width(ui.available_width() - 90.0),
                    );
                    if ui.button("Import").clicked() && !st.import_path.trim().is_empty() {
                        let p = expand_home(&st.import_path);
                        let lower = p.to_string_lossy().to_lowercase();
                        if lower.ends_with(".m3u") || lower.ends_with(".m3u8") {
                            cx.actions.push(Action::Cmd(Command::ImportM3u(p)));
                        } else {
                            cx.actions.push(Action::Cmd(Command::ImportAppleXml(p)));
                        }
                        st.import_path.clear();
                    }
                });
                hint(ui, "Music app on macOS / iTunes on Windows: File → Library → Export Library…");
                ui.add_space(8.0);
                ui.collapsing("Import with the Apple Music API", |ui| {
                    text_field(ui, "Media user token", &mut cfg.apple_music.user_token, "media-user-token cookie from music.apple.com", true);
                    text_field(ui, "Developer token (optional)", &mut cfg.apple_music.developer_token, "fetched automatically when empty", true);
                    text_field(ui, "Storefront", &mut cfg.apple_music.storefront, "us", false);
                    if ui.button(theme::ic(icon::ARROWS_CLOCKWISE, "Import library & playlists")).clicked() {
                        cx.actions.push(Action::Cmd(Command::ImportAppleApi));
                    }
                });
            });

            // ---------------------------------------------------------- last.fm
            section(ui, icon::LASTFM_LOGO, Color32::from_rgb(0xd5, 0x10, 0x07), "Last.fm scrobbling", |ui| {
                status(ui, &cx.feed.lastfm);
                ui.add_space(4.0);
                ui.checkbox(&mut cfg.lastfm.enabled, "Scrobble what I listen to");
                ui.checkbox(&mut cfg.lastfm.scrobble_instantly, "Scrobble as soon as a song starts");
                text_field(ui, "API key", &mut cfg.lastfm.api_key, "from last.fm/api/account/create", false);
                text_field(ui, "API secret", &mut cfg.lastfm.api_secret, "", true);
                ui.horizontal(|ui| {
                    let connected = matches!(cx.feed.lastfm, AccountStatus::Connected(_));
                    if connected {
                        if ui.button(theme::ic(icon::CHART_BAR, "Your stats")).clicked() {
                            cx.actions.push(Action::Go(super::View::Profile));
                        }
                        if ui.button("Disconnect").clicked() {
                            cx.actions.push(Action::Cmd(Command::LastfmLogout));
                        }
                    } else if ui.button(theme::ic(icon::SIGN_IN, "Connect account")).clicked() {
                        cx.actions.push(Action::Cmd(Command::LastfmLogin));
                    }
                });
                hint(
                    ui,
                    if cfg.lastfm.scrobble_instantly {
                        "Every song scrobbles the moment it starts playing, even if you skip it. \
                         Offline scrobbles are queued and sent later."
                    } else {
                        "Songs scrobble after half their length (or 4 minutes), Last.fm's usual rule. \
                         Offline scrobbles are queued and sent later."
                    },
                );
            });

            // ---------------------------------------------------------- discord
            section(ui, icon::DISCORD_LOGO, Color32::from_rgb(0x58, 0x65, 0xf2), "Discord Rich Presence", |ui| {
                ui.checkbox(&mut cfg.discord.enabled, "Show what I'm listening to on Discord");
                text_field(ui, "Application ID", &mut cfg.discord.app_id, "e.g. 1234567890123456789", false);
                ui.checkbox(&mut cfg.discord.song_as_activity_name, "Show the song title as \"Listening to …\"");
                hint(
                    ui,
                    "Create a free app at discord.com/developers/applications (name it e.g. \"Sumo\") and paste its \
                     Application ID. Works with the Discord desktop app, Vesktop and arRPC.",
                );
            });

            // ---------------------------------------------------------- playback
            section(ui, icon::HEADPHONES, cx.accent, "Playback", |ui| {
                ui.checkbox(&mut cfg.playback.gapless, "Gapless playback");
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Crossfade").color(TEXT_DIM));
                    ui.add(
                        egui::Slider::new(&mut cfg.playback.crossfade, 0.0..=12.0)
                            .step_by(0.5)
                            .custom_formatter(|v, _| if v <= 0.0 { "Off".into() } else { format!("{v:.1} s") }),
                    );
                });
                if cfg.playback.crossfade > 0.0 {
                    ui.checkbox(
                        &mut cfg.playback.crossfade_albums,
                        "Also crossfade between songs of the same album",
                    );
                    hint(
                        ui,
                        "Each song fades into the next, between any sources (local files, SoundCloud, Spotify). \
                         Albums stay gapless unless you tick the box above. Crossfade is off in bit-perfect mode.",
                    );
                }
                ui.checkbox(&mut cfg.playback.replaygain, "Even out loudness (local files and SoundCloud)");
                hint(
                    ui,
                    "Plays songs about as loud as Spotify's \"Normalize volume\": files with ReplayGain tags \
                     follow their tags, SoundCloud and untagged files play 6 dB quieter. Off in bit-perfect mode.",
                );
                ui.checkbox(&mut cfg.lyrics.enabled, "Show lyrics");
                ui.checkbox(
                    &mut cfg.lyrics.online,
                    "Fetch lyrics online when there are no local lyrics (LRCLIB, then Genius)",
                );
                text_field(ui, "mpv binary", &mut cfg.playback.mpv_path, "mpv", false);
                if !st.devices_requested {
                    st.devices_requested = true;
                    cx.actions.push(Action::Cmd(Command::ListAudioDevices));
                }
                ui.label(RichText::new("Output device (local files & SoundCloud)").color(TEXT_DIM));
                ui.horizontal(|ui| {
                    let current = cfg.playback.audio_device.clone();
                    let shown = if current.is_empty() {
                        "Automatic".to_string()
                    } else {
                        cx.feed
                            .audio_devices
                            .iter()
                            .find(|(id, _)| *id == current)
                            .map(|(_, d)| d.clone())
                            .unwrap_or(current)
                    };
                    egui::ComboBox::from_id_salt("audio-device")
                        .width(ui.available_width() - 90.0)
                        .selected_text(shown)
                        .truncate()
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut cfg.playback.audio_device, String::new(), "Automatic");
                            for (id, desc) in cx.feed.audio_devices.iter().filter(|(id, _)| id != "auto") {
                                ui.selectable_value(&mut cfg.playback.audio_device, id.clone(), format!("{desc}  —  {id}"));
                            }
                        });
                    if ui.button("Refresh").clicked() {
                        cx.actions.push(Action::Cmd(Command::ListAudioDevices));
                    }
                });
                ui.add_space(4.0);
                ui.checkbox(&mut cfg.playback.bit_perfect, "Bit-perfect output for lossless files");
                hint(
                    ui,
                    if cfg!(windows) {
                        "Opens the device exclusively (WASAPI exclusive mode) and skips ReplayGain so FLAC/ALAC/WAV \
                         reach your DAC untouched. Keep the volume at 100% and use your DAC or amp for volume."
                    } else if cfg!(target_os = "macos") {
                        "Opens the device exclusively (Core Audio hog mode) and skips ReplayGain so FLAC/ALAC/WAV \
                         reach your DAC untouched at their own sample rate. Keep the volume at 100% and use your DAC \
                         or amp for volume."
                    } else {
                        "Opens the device exclusively and skips ReplayGain so FLAC/ALAC/WAV reach your DAC untouched. \
                         For true bit-perfect playback pick an \"alsa/hw:…\" device above, keep the volume at 100% and \
                         use your DAC or amp for volume. Through PipeWire, audio is resampled to PipeWire's rate unless \
                         you allow more rates (see the README)."
                    },
                );
                hint(ui, "Local files and SoundCloud play through mpv; Spotify plays through librespot (max 320 kbps Ogg Vorbis).");
            });

            // ---------------------------------------------------------- appearance
            section(ui, icon::SPARKLE, cx.accent, "Appearance", |ui| {
                ui.checkbox(&mut cfg.ui.dynamic_accent, "Colour backgrounds with the current cover");
                if cfg!(not(any(windows, target_os = "macos"))) {
                    ui.checkbox(&mut cfg.ui.notifications, "Show a notification when a song starts");
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Accent colour").color(TEXT_DIM));
                    ui.color_edit_button_srgb(&mut cfg.ui.accent);
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Interface scale").color(TEXT_DIM));
                    let r = ui.add(egui::Slider::new(&mut cfg.ui.scale, super::SCALE.0..=super::SCALE.1).step_by(0.05));
                    if r.drag_stopped() || r.lost_focus() {
                        ui.ctx().set_zoom_factor(cfg.ui.scale);
                    }
                    ui.label(RichText::new("Ctrl + / Ctrl − / Ctrl 0").color(TEXT_FAINT).small());
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Covers kept in memory").color(TEXT_DIM));
                    ui.add(egui::DragValue::new(&mut cfg.ui.art_cache_size).range(32..=2000));
                });
                hint(ui, "Fewer cached covers = less RAM. Takes effect after a restart.");
            });

            // ---------------------------------------------------------- updates
            section(ui, icon::ARROW_CIRCLE_UP, TEXT, "Updates", |ui| {
                use crate::service::UpdateStatus;
                ui.label(format!("You have Sumo {}", env!("CARGO_PKG_VERSION")));
                ui.checkbox(&mut cfg.updates.check, "Check for updates automatically");
                let update = &cx.feed.update;
                ui.horizontal(|ui| {
                    let busy = matches!(
                        update.status,
                        UpdateStatus::Checking
                            | UpdateStatus::Downloading(_)
                            | UpdateStatus::NeedsPassword { .. }
                            | UpdateStatus::Installing
                    );
                    if ui.add_enabled(!busy, egui::Button::new("Check now")).clicked() {
                        cx.actions.push(Action::Cmd(Command::CheckUpdates { manual: true }));
                    }
                    match (&update.status, &update.available) {
                        (UpdateStatus::Checking, _) => {
                            ui.spinner();
                        }
                        (UpdateStatus::Downloading(done), _) => {
                            ui.add(egui::ProgressBar::new(*done).desired_width(160.0).show_percentage());
                        }
                        (UpdateStatus::NeedsPassword { .. }, _) => {
                            ui.label("Waiting for your password…");
                        }
                        (UpdateStatus::Installing, _) => {
                            ui.label("Installing…");
                        }
                        (UpdateStatus::Installed, _) => {
                            ui.label("Installed. Start Sumo again to use the new version.");
                        }
                        (UpdateStatus::Failed(why), _) => {
                            ui.label(RichText::new(why).color(DANGER));
                        }
                        (_, Some(release)) => {
                            ui.label(RichText::new(format!("Version {} is available", release.version)).strong());
                            if crate::updater::can_install() && ui.button("Update now").clicked() {
                                cx.actions.push(Action::Cmd(Command::InstallUpdate));
                            }
                        }
                        (UpdateStatus::UpToDate, None) => {
                            ui.label(RichText::new("You have the newest version").color(TEXT_DIM));
                        }
                        _ => {}
                    }
                });
                if !crate::updater::can_install() {
                    hint(ui, "On Linux, update the way you installed: git pull && makepkg -sif");
                } else if crate::updater::NEEDS_PASSWORD {
                    hint(
                        ui,
                        "Update now downloads the new version's package, checks it, asks for your password and \
                         installs it with pacman. Sumo then starts again.",
                    );
                }
            });

            // ---------------------------------------------------------- backup
            section(ui, icon::ARCHIVE, TEXT, "Back up or move your settings", |ui| {
                hint(
                    ui,
                    "Saves everything on this page to one file, with your keys and logins and your own playlists if \
                     you like. Import it on another computer (Linux, Windows or macOS) to carry on where you left off.",
                );
                ui.add_space(4.0);
                ui.checkbox(
                    &mut st.export_keys,
                    "Include keys and logins (Spotify, Last.fm, SoundCloud, Apple Music, Discord)",
                );
                let mut playlists = !st.export_no_playlists;
                ui.checkbox(&mut playlists, "Include your playlists and Liked Songs");
                st.export_no_playlists = !playlists;
                if st.export_to.is_empty() {
                    st.export_to = crate::backup::default_export_path().to_string_lossy().into_owned();
                }
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut st.export_to).desired_width(ui.available_width() - 90.0));
                    if ui.button(theme::ic(icon::UPLOAD_SIMPLE, "Export")).clicked() && !st.export_to.trim().is_empty() {
                        cx.actions.push(Action::Cmd(Command::ExportSettings {
                            path: expand_home(&st.export_to),
                            include: crate::backup::Include {
                                keys: st.export_keys,
                                playlists,
                            },
                        }));
                    }
                });
                if st.export_keys {
                    hint(ui, "The file then holds your passwords and tokens: keep it private.");
                }
                if let Some(saved) = &cx.feed.exported_settings {
                    ui.horizontal(|ui| {
                        if ui.small_button("Show in folder").clicked() {
                            if let Some(dir) = saved.parent() {
                                cx.actions.push(Action::OpenUrl(dir.to_string_lossy().into_owned()));
                            }
                        }
                        let name = saved.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        ui.add(
                            egui::Label::new(RichText::new(format!("Saved as {name}")).small().color(TEXT_DIM))
                                .truncate(),
                        )
                        .on_hover_text(saved.display().to_string());
                    });
                }
                ui.add_space(8.0);
                if !st.settings_file_looked {
                    st.settings_file_looked = true;
                    if let Some(found) = crate::backup::find_settings_files().first() {
                        st.settings_file = found.to_string_lossy().into_owned();
                    }
                }
                if st.settings_file.is_empty() {
                    if let Some(saved) = &cx.feed.exported_settings {
                        st.settings_file = saved.to_string_lossy().into_owned();
                    }
                }
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut st.settings_file)
                            .hint_text("Settings file to import (or drop it onto the window)")
                            .desired_width(ui.available_width() - 190.0),
                    );
                    if ui.button(theme::ic(icon::FOLDER_OPEN, "Browse…")).clicked() {
                        picker::open(
                            ui.ctx(),
                            "settings-file",
                            "Settings file to import",
                            Pick::File("Sumo settings", &["json"]),
                        );
                    }
                    match picker::take(ui.ctx(), "settings-file") {
                        Some(Ok(p)) => st.settings_file = p.to_string_lossy().into_owned(),
                        Some(Err(e)) => cx.actions.push(Action::Cmd(Command::Notify(e))),
                        None => {}
                    }
                    let file = expand_home(&st.settings_file);
                    if ui
                        .add_enabled(file.is_file(), egui::Button::new(theme::ic(icon::DOWNLOAD_SIMPLE, "Import…")))
                        .clicked()
                    {
                        cx.actions.push(Action::ImportSettings(file));
                    }
                });
                hint(
                    ui,
                    "Importing replaces your settings (folders and programs that aren't on this computer stay as they \
                     are), adds the playlists, and restarts Sumo.",
                );
            });

            // ---------------------------------------------------------- about
            section(ui, icon::INFO, TEXT_DIM, "About", |ui| {
                ui.horizontal(|ui| {
                    theme::logo(ui, cx.logo, 40.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Sumo").font(theme::bold_font(18.0)));
                        ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(TEXT_DIM));
                    });
                });
                ui.add_space(4.0);
                ui.label(RichText::new(format!("Memory in use: {rss_mb:.0} MB (mpv runs as a separate process)")).color(TEXT_DIM));
                ui.label(RichText::new(format!("Config: {}", paths.config_file().display())).color(TEXT_FAINT).small());
                ui.label(RichText::new(format!("Data: {}", paths.data_dir.display())).color(TEXT_FAINT).small());
                ui.add_space(4.0);
                hint(
                    ui,
                    "Shortcuts: Space play/pause · ←/→ seek · Ctrl+←/→ prev/next · ↑/↓ volume · Ctrl+K or / search · \
                     Alt+←/→ back/forward · L lyrics view",
                );
            });
        });
    });
}
