//! Sumo: a lightweight native music player for local files, Spotify and SoundCloud.

// No console window next to the app on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod backup;
mod clock;
mod config;
mod downloader;
mod http;
mod integrations;
mod library;
mod links;
mod memory;
mod model;
mod player;
mod providers;
mod service;
mod tools;
mod ui;
mod updater;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, Paths};
use crate::service::{Command, Shared};

fn main() -> anyhow::Result<()> {
    memory::tune();
    // How Spotify's audio stream shows up in pavucontrol / the PipeWire graph. Set before
    // any other thread exists, as required for set_var.
    std::env::set_var("PULSE_PROP_application.name", "Sumo");
    std::env::set_var("PULSE_PROP_application.icon_name", "sumo");
    std::env::set_var("PULSE_PROP_stream.description", "Spotify");
    let paths = Paths::new();
    let filter = EnvFilter::try_from_env("MULTIMUSIC_LOG")
        .unwrap_or_else(|_| EnvFilter::new("multimusic=info,librespot=warn,warn"));
    match log_file(&paths) {
        // Windows and macOS apps have no terminal: the log goes to a file in the data folder.
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
    tracing::info!("Sumo {} starting", env!("CARGO_PKG_VERSION"));
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));

    // Windows: starting Sumo again brings the running one to the front.
    #[cfg(windows)]
    if instance::running_elsewhere() {
        return Ok(());
    }

    let cfg = Config::load(&paths);

    // Two workers are plenty: everything heavy is IO bound or runs in mpv/librespot threads.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .thread_name("multimusic-rt")
        .enable_all()
        .build()?;

    let shared = Arc::new(Shared::default());
    let (cmd, service) = service::start(rt.handle().clone(), shared.clone(), paths.clone(), cfg.clone());
    #[cfg(windows)]
    instance::listen(rt.handle(), cmd.clone());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Sumo")
            .with_app_id("multimusic")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([880.0, 560.0])
            .with_icon(Arc::new(app_icon())),
        multisampling: 0,
        depth_buffer: 0,
        stencil_buffer: 0,
        ..Default::default()
    };
    let handle = rt.handle().clone();
    let ui_cmd = cmd.clone();
    let ui_shared = shared.clone();
    let result = eframe::run_native(
        "Sumo",
        options,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc, ui_shared, ui_cmd, cfg, paths, handle)))),
    );

    // Let the service stop mpv, clear Discord and save the session.
    let _ = cmd.send(Command::Quit);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !service.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    rt.shutdown_timeout(Duration::from_millis(500));
    // Imported settings take effect in a fresh start; a restart may also have been asked for.
    if shared
        .feed
        .read()
        .map(|f| f.imported_settings.is_some() || f.restart)
        .unwrap_or(false)
    {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .args(std::env::args_os().skip(1))
                .spawn();
        }
    }
    result.map_err(|e| anyhow!("{e}"))
}

/// The log file used where there is no terminal to log to (the Windows and macOS apps).
fn log_file(paths: &Paths) -> Option<std::fs::File> {
    use std::io::IsTerminal;
    if !cfg!(any(windows, target_os = "macos")) || std::io::stderr().is_terminal() {
        return None;
    }
    std::fs::File::create(paths.data_dir.join("multimusic.log")).ok()
}

/// One Sumo at a time on Windows: a second start asks the first to show itself.
#[cfg(windows)]
mod instance {
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::sync::mpsc::UnboundedSender;

    use crate::service::Command;

    /// One name per user: pipe names are shared by everyone on the computer, and another
    /// user's pipe must not count as this user's Sumo running.
    fn pipe() -> String {
        let user: String = std::env::var("USERNAME")
            .unwrap_or_default()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        format!(r"\\.\pipe\multimusic-running-{user}")
    }

    /// True when another Sumo runs (it was asked to come to the front).
    pub fn running_elsewhere() -> bool {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        // SECURITY_IDENTIFICATION: whoever made the pipe may learn who connects, but can't
        // act as this user.
        const SECURITY_IDENTIFICATION: u32 = 1 << 16;
        match std::fs::OpenOptions::new()
            .write(true)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(pipe())
        {
            Ok(mut pipe) => {
                let _ = pipe.write_all(b"raise\n");
                true
            }
            Err(_) => false,
        }
    }

    pub fn listen(rt: &tokio::runtime::Handle, commands: UnboundedSender<Command>) {
        let _guard = rt.enter();
        let Ok(server) = ServerOptions::new().first_pipe_instance(true).create(pipe()) else {
            return;
        };
        rt.spawn(serve(server, commands));
    }

    async fn serve(mut server: NamedPipeServer, commands: UnboundedSender<Command>) {
        loop {
            if server.connect().await.is_err() {
                return;
            }
            let _ = commands.send(Command::Raise);
            // A new instance of the pipe for the next start.
            match ServerOptions::new().create(pipe()) {
                Ok(next) => server = next,
                Err(_) => return,
            }
        }
    }
}

/// Window/taskbar icon, from the same artwork as the launcher icon and the in-app logo.
fn app_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon-256.png"))
        .expect("bundled icon is a valid PNG")
        .to_rgba8();
    egui::IconData {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    }
}
