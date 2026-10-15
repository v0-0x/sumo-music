//! mpv playback engine (local files and SoundCloud streams), controlled over its JSON IPC socket.
//!
//! mpv is spawned once as an audio-only, config-less child process. It handles every codec,
//! HLS streams, ReplayGain, gapless playback and outputs straight to PipeWire/PulseAudio/ALSA.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, PartialEq)]
pub enum MpvEvent {
    /// A new file started (playlist advanced, possibly gaplessly).
    StartFile {
        playlist_entry_id: i64,
    },
    FileLoaded,
    EndFile {
        reason: String,
        error: Option<String>,
        /// The playlist entry that ended (see [`MpvEvent::StartFile`]).
        playlist_entry_id: i64,
    },
    Pause(bool),
    Duration(f64),
    /// The mpv process exited or the socket closed.
    Died,
}

#[derive(Debug, Clone)]
pub struct MpvOptions {
    pub binary: String,
    pub volume: f32,
    pub replaygain: bool,
    pub gapless: bool,
    pub audio_device: String,
    /// Exclusive device access (bit-perfect with ALSA hw devices).
    pub exclusive: bool,
}

/// ReplayGain 2.0 tags level songs to -18 LUFS, Spotify's "Normalize volume" to about
/// -14 LUFS: this lifts tagged files to Spotify's level (mpv lowers it again where the
/// song would clip).
const REPLAYGAIN_PREAMP_DB: f64 = 4.0;
/// Songs without ReplayGain tags (SoundCloud streams, untagged files) are mostly mastered
/// at -10 to -6 LUFS, well above Spotify's level.
const REPLAYGAIN_FALLBACK_DB: f64 = -6.0;

/// mpv's loudness levelling properties. mpv applies the fallback gain even with ReplayGain
/// off, so it must be 0 then (bit-perfect playback included).
fn loudness(on: bool) -> [(&'static str, Value); 3] {
    let (mode, preamp, fallback) = if on {
        ("track", REPLAYGAIN_PREAMP_DB, REPLAYGAIN_FALLBACK_DB)
    } else {
        ("no", 0.0, 0.0)
    };
    [
        ("replaygain", json!(mode)),
        ("replaygain-preamp", json!(preamp)),
        ("replaygain-fallback", json!(fallback)),
    ]
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// Events from one mpv instance, tagged with its [`Mpv::id`] (crossfades run two at once).
pub type MpvSender = mpsc::UnboundedSender<(u64, MpvEvent)>;

type IpcReader = Box<dyn AsyncRead + Send + Unpin>;
type IpcWriter = Box<dyn AsyncWrite + Send + Unpin>;

pub struct Mpv {
    id: u64,
    child: Child,
    writer: tokio::sync::Mutex<IpcWriter>,
    pending: Pending,
    next_id: AtomicU64,
    socket: PathBuf,
}

impl Mpv {
    /// Starts an mpv process. `id` tags its events and keeps its IPC socket apart from other
    /// instances.
    pub async fn spawn(opts: &MpvOptions, id: u64, events: MpvSender) -> Result<Mpv> {
        let socket = ipc_path(id);
        // A socket left at our path that can't be removed isn't ours to talk to.
        if let Err(e) = std::fs::remove_file(&socket) {
            if e.kind() != std::io::ErrorKind::NotFound {
                bail!("can't reuse {}: {e}", socket.display());
            }
        }

        let mut cmd = crate::tools::command(&opts.binary);
        cmd.arg("--no-config")
            .arg("--idle=yes")
            .arg("--video=no")
            .arg("--audio-display=no")
            .arg("--no-terminal")
            .arg("--ytdl=no")
            .arg("--keep-open=no")
            .arg("--prefetch-playlist=yes")
            .arg("--audio-client-name=Sumo")
            // Cache network streams only; a few MB of demuxer buffer is plenty for audio.
            .arg("--cache=auto")
            .arg("--demuxer-max-bytes=4MiB")
            .arg("--demuxer-max-back-bytes=1MiB")
            .arg("--volume-max=100")
            .arg(format!("--volume={}", opts.volume.clamp(0.0, 100.0)))
            .arg(format!("--gapless-audio={}", if opts.gapless { "weak" } else { "no" }))
            .arg(format!(
                "--audio-exclusive={}",
                if opts.exclusive { "yes" } else { "no" }
            ))
            .arg(format!("--input-ipc-server={}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // Make mpv exit with us even if we're killed without a chance to clean up.
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        if !opts.audio_device.is_empty() {
            cmd.arg(format!("--audio-device={}", opts.audio_device));
        }
        for (name, value) in loudness(opts.replaygain) {
            match value.as_str() {
                Some(text) => cmd.arg(format!("--{name}={text}")),
                None => cmd.arg(format!("--{name}={value}")),
            };
        }
        // Skip mpv's built-in Lua scripts (OSC, console, stats, ...): an audio backend
        // doesn't need them and each one costs memory. Options differ between versions,
        // so only pass the ones this mpv knows.
        let supported = supported_options(&opts.binary).await;
        for flag in LEAN_FLAGS {
            let name = flag.split('=').next().unwrap_or(flag);
            if supported.iter().any(|o| o == name) {
                cmd.arg(flag);
            }
        }
        let child = cmd.spawn().with_context(|| {
            format!(
                "could not start mpv (`{}`). {}",
                opts.binary,
                crate::tools::install_hint("mpv")
            )
        })?;
        crate::tools::end_with_us(&child);

        // Wait for the IPC socket to show up.
        let mut stream = None;
        for _ in 0..100 {
            if let Ok(s) = connect(&socket).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let (read, write) = stream.ok_or_else(|| anyhow!("mpv did not open its IPC socket"))?;

        let pending: Pending = Arc::default();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = msg.get("request_id").and_then(Value::as_u64) {
                    if let Some(tx) = reader_pending.lock().unwrap().remove(&id) {
                        let ok = msg.get("error").and_then(Value::as_str) == Some("success");
                        let _ = tx.send(if ok {
                            Ok(msg.get("data").cloned().unwrap_or(Value::Null))
                        } else {
                            Err(anyhow!(
                                "mpv: {}",
                                msg.get("error").and_then(Value::as_str).unwrap_or("error")
                            ))
                        });
                    }
                    continue;
                }
                if let Some(ev) = parse_event(&msg) {
                    if events.send((id, ev)).is_err() {
                        break;
                    }
                }
            }
            let _ = events.send((id, MpvEvent::Died));
        });

        let mpv = Mpv {
            id,
            child,
            writer: tokio::sync::Mutex::new(write),
            pending,
            next_id: AtomicU64::new(1),
            socket,
        };
        mpv.command(json!(["observe_property", 1, "pause"])).await?;
        mpv.command(json!(["observe_property", 2, "duration"])).await?;
        Ok(mpv)
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub async fn command(&self, args: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut line = serde_json::to_vec(&json!({ "command": args, "request_id": id }))?;
        line.push(b'\n');
        {
            let mut w = self.writer.lock().await;
            if let Err(e) = w.write_all(&line).await {
                self.pending.lock().unwrap().remove(&id);
                bail!("mpv IPC write failed: {e}");
            }
        }
        match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => bail!("mpv closed"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("mpv did not answer")
            }
        }
    }

    /// Replaces the playlist with `url` and starts playing at `start` seconds.
    pub async fn load(&self, url: &str, start: f64) -> Result<()> {
        if start > 0.5 {
            let opt = format!("start={start:.1}");
            // mpv >= 0.38 takes an insertion index before the per-file options; older versions don't.
            if self
                .command(json!(["loadfile", url, "replace", -1, opt]))
                .await
                .is_err()
            {
                self.command(json!(["loadfile", url, "replace", opt])).await?;
            }
        } else {
            self.command(json!(["loadfile", url, "replace"])).await?;
        }
        self.set_pause(false).await
    }

    /// Appends `url` so mpv can continue into it gaplessly.
    pub async fn append(&self, url: &str) -> Result<()> {
        self.command(json!(["loadfile", url, "append"])).await.map(|_| ())
    }

    /// Removes every playlist entry except the current one.
    pub async fn clear_upcoming(&self) -> Result<()> {
        self.command(json!(["playlist-clear"])).await.map(|_| ())
    }

    pub async fn set_pause(&self, pause: bool) -> Result<()> {
        self.command(json!(["set_property", "pause", pause])).await.map(|_| ())
    }

    pub async fn seek(&self, secs: f64) -> Result<()> {
        self.command(json!(["seek", secs.max(0.0), "absolute"]))
            .await
            .map(|_| ())
    }

    pub async fn set_volume(&self, volume: f32) -> Result<()> {
        self.command(json!(["set_property", "volume", volume.clamp(0.0, 100.0)]))
            .await
            .map(|_| ())
    }

    /// Turns loudness levelling (see [`loudness`]) on or off.
    pub async fn set_replaygain(&self, on: bool) -> Result<()> {
        for (name, value) in loudness(on) {
            self.command(json!(["set_property", name, value])).await?;
        }
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        self.command(json!(["stop"])).await.map(|_| ())
    }

    pub async fn time_pos(&self) -> Option<f64> {
        self.command(json!(["get_property", "time-pos"])).await.ok()?.as_f64()
    }

    pub async fn quit(mut self) {
        let _ = self.command(json!(["quit"])).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await;
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.socket);
    }
}

const IPC_PREFIX: &str = "multimusic-mpv-";

/// Where mpv listens for commands: a Unix socket, or a named pipe on Windows.
fn ipc_path(id: u64) -> PathBuf {
    let name = format!("{IPC_PREFIX}{}-{id}.sock", std::process::id());
    if cfg!(windows) {
        return PathBuf::from(format!(r"\\.\pipe\{name}"));
    }
    ipc_dir().join(&name)
}

/// A folder only this user can enter, for mpv's sockets: anyone who can reach a socket can
/// make mpv run programs, and anyone who could put one at our path first would get our
/// commands. The login's runtime folder when there is one, otherwise a folder of our own.
#[cfg(unix)]
fn ipc_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        // Unix socket paths are limited to ~104 bytes.
        let short = |dir: &Path| dir.join(format!("{IPC_PREFIX}{}-99.sock", u32::MAX)).as_os_str().len() <= 100;
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
            if short(&dir) && is_private_dir(&dir) {
                return dir;
            }
        }
        let base = std::env::temp_dir();
        let base = if short(&base.join("multimusic-4294967295-xxxxxxxx")) {
            base
        } else {
            PathBuf::from("/tmp")
        };
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let mine = base.join(format!("multimusic-{uid}"));
        if private_dir(&mine) {
            return mine;
        }
        // Someone else has that name: a fresh one with a random name.
        for _ in 0..16 {
            let dir = base.join(format!("multimusic-{uid}-{:08x}", rand::random::<u32>()));
            if private_dir(&dir) {
                return dir;
            }
        }
        mine
    })
    .clone()
}

#[cfg(windows)]
fn ipc_dir() -> PathBuf {
    std::env::temp_dir()
}

/// Makes `dir` (mode 0700) or checks the one that is there is ours and private.
#[cfg(unix)]
fn private_dir(dir: &Path) -> bool {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => is_private_dir(dir),
        Err(_) => false,
    }
}

/// A real folder (not a link) owned by this user that nobody else can enter.
#[cfg(unix)]
fn is_private_dir(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(dir).is_ok_and(|m| {
        // SAFETY: geteuid has no preconditions.
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0
    })
}

async fn connect(path: &Path) -> std::io::Result<(IpcReader, IpcWriter)> {
    #[cfg(unix)]
    {
        let (read, write) = tokio::net::UnixStream::connect(path).await?.into_split();
        Ok((Box::new(read), Box::new(write)))
    }
    #[cfg(windows)]
    {
        let pipe = tokio::net::windows::named_pipe::ClientOptions::new().open(path)?;
        let (read, write) = tokio::io::split(pipe);
        Ok((Box::new(read), Box::new(write)))
    }
}

/// Stops players a previous Sumo left behind when it was killed (Linux ends them with
/// the app; on Windows a job object does).
pub fn stop_orphans() {
    // Older versions put sockets straight into /tmp.
    #[cfg(unix)]
    for dir in [ipc_dir(), PathBuf::from("/tmp")] {
        use std::os::unix::fs::MetadataExt;
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            // Only our own: other users' sockets are none of our business.
            // SAFETY: geteuid has no preconditions.
            let uid = unsafe { libc::geteuid() };
            if !entry.metadata().is_ok_and(|m| m.uid() == uid) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(pid) = name
                .strip_prefix(IPC_PREFIX)
                .and_then(|rest| rest.split('-').next())
                .and_then(|pid| pid.parse::<i32>().ok())
            else {
                continue;
            };
            // SAFETY: signal 0 only checks whether the process exists. EPERM means it does
            // (someone else's).
            let alive = pid == std::process::id() as i32
                || unsafe { libc::kill(pid, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
            if alive {
                continue;
            }
            if let Ok(mut socket) = std::os::unix::net::UnixStream::connect(entry.path()) {
                use std::io::Write;
                let _ = socket.write_all(b"{\"command\":[\"quit\"]}\n");
            }
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Output devices mpv can use, as (id, description). The id goes into `--audio-device`.
pub async fn list_audio_devices(binary: &str) -> Vec<(String, String)> {
    let output = crate::tools::command(binary)
        .arg("--no-config")
        .arg("--audio-device=help")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;
    match output {
        Ok(out) => parse_device_list(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => Vec::new(),
    }
}

fn parse_device_list(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix('\'')?;
            let (id, desc) = rest.split_once('\'')?;
            let desc = desc.trim().trim_start_matches('(').trim_end_matches(')').to_string();
            Some((id.to_string(), desc))
        })
        .collect()
}

const LEAN_FLAGS: &[&str] = &[
    "--osc=no",
    "--load-scripts=no",
    "--load-stats-overlay=no",
    "--load-console=no",
    "--load-osd-console=no",
    "--load-auto-profiles=no",
    "--load-select=no",
    "--load-positioning=no",
    "--load-commands=no",
    "--load-context-menu=no",
    "--input-default-bindings=no",
    "--osd-level=0",
    "--sub-auto=no",
    "--audio-file-auto=no",
    "--cover-art-auto=no",
];

/// Option names (`--foo`) the given mpv binary understands. Asked once per binary, so a
/// second instance (for crossfades) starts quickly.
async fn supported_options(binary: &str) -> Vec<String> {
    static KNOWN: Mutex<Option<HashMap<String, Vec<String>>>> = Mutex::new(None);
    if let Some(list) = KNOWN.lock().unwrap().as_ref().and_then(|m| m.get(binary)) {
        return list.clone();
    }
    let output = crate::tools::command(binary)
        .arg("--no-config")
        .arg("--list-options")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;
    let list = match output {
        Ok(out) => parse_option_list(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => return Vec::new(),
    };
    KNOWN
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(binary.to_string(), list.clone());
    list
}

fn parse_option_list(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|w| w.starts_with("--"))
        .map(str::to_string)
        .collect()
}

fn parse_event(msg: &Value) -> Option<MpvEvent> {
    match msg.get("event")?.as_str()? {
        "start-file" => Some(MpvEvent::StartFile {
            playlist_entry_id: msg.get("playlist_entry_id").and_then(Value::as_i64).unwrap_or(0),
        }),
        "file-loaded" => Some(MpvEvent::FileLoaded),
        "end-file" => Some(MpvEvent::EndFile {
            reason: msg.get("reason").and_then(Value::as_str).unwrap_or("").to_string(),
            error: msg.get("file_error").and_then(Value::as_str).map(str::to_string),
            playlist_entry_id: msg.get("playlist_entry_id").and_then(Value::as_i64).unwrap_or(0),
        }),
        "property-change" => match msg.get("name")?.as_str()? {
            "pause" => Some(MpvEvent::Pause(msg.get("data")?.as_bool()?)),
            "duration" => Some(MpvEvent::Duration(msg.get("data")?.as_f64()?)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn parses_ipc_events() {
        let ev = |s: &str| parse_event(&serde_json::from_str(s).unwrap());
        assert_eq!(
            ev(r#"{"event":"end-file","reason":"eof","playlist_entry_id":1}"#),
            Some(MpvEvent::EndFile {
                reason: "eof".into(),
                error: None,
                playlist_entry_id: 1
            })
        );
        assert_eq!(
            ev(r#"{"event":"end-file","reason":"error","file_error":"unrecognized file format"}"#),
            Some(MpvEvent::EndFile {
                reason: "error".into(),
                error: Some("unrecognized file format".into()),
                playlist_entry_id: 0
            })
        );
        assert_eq!(
            ev(r#"{"event":"property-change","id":1,"name":"pause","data":true}"#),
            Some(MpvEvent::Pause(true))
        );
        assert_eq!(
            ev(r#"{"event":"property-change","id":2,"name":"duration","data":201.5}"#),
            Some(MpvEvent::Duration(201.5))
        );
        assert_eq!(ev(r#"{"event":"property-change","id":2,"name":"duration"}"#), None);
        assert_eq!(
            ev(r#"{"event":"start-file","playlist_entry_id":3}"#),
            Some(MpvEvent::StartFile { playlist_entry_id: 3 })
        );
    }

    #[test]
    fn parses_device_list() {
        let text = "List of detected audio devices:\n  'auto' (Autoselect device)\n  'pipewire' (Default (pipewire))\n  'alsa/hw:CARD=DAC,DEV=0' (USB DAC, USB Audio/Direct hardware device without any conversions)\n";
        let devices = parse_device_list(text);
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[1], ("pipewire".into(), "Default (pipewire".into()));
        assert_eq!(devices[2].0, "alsa/hw:CARD=DAC,DEV=0");
        assert!(devices[2].1.starts_with("USB DAC"));
    }

    #[test]
    fn parses_option_list() {
        let text = "Options:\n\n --osc                            Flag (default: yes)\n --load-scripts                   Flag (default: yes)\n\nTotal: 2 options\n";
        assert_eq!(
            parse_option_list(text),
            vec!["--osc".to_string(), "--load-scripts".to_string()]
        );
    }

    /// Talks to a real mpv if one is installed: two instances (as during a crossfade) play a
    /// generated WAV file at the same time, each reporting under its own id.
    #[tokio::test]
    async fn real_mpv_roundtrip() {
        if std::process::Command::new("mpv").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("multimusic-mpv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        write_test_wav(&wav, 2.0);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let opts = MpvOptions {
            binary: "mpv".into(),
            volume: 0.0,
            replaygain: false,
            gapless: true,
            audio_device: String::new(),
            exclusive: false,
        };
        let a = Mpv::spawn(&opts, 7, tx.clone()).await.unwrap();
        let b = Mpv::spawn(&opts, 8, tx).await.unwrap();
        assert_eq!((a.id(), b.id()), (7, 8));
        for mpv in [&a, &b] {
            // Use the null audio output so the test needs no sound card.
            mpv.command(json!(["set_property", "ao", "null"])).await.unwrap();
            mpv.load(&wav.to_string_lossy(), 0.0).await.unwrap();
        }
        let mut loaded = HashSet::new();
        let mut ended = HashSet::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while let Ok(Some((id, ev))) = tokio::time::timeout_at(deadline, rx.recv()).await {
            match ev {
                MpvEvent::FileLoaded => {
                    loaded.insert(id);
                }
                MpvEvent::EndFile { reason, .. } if reason == "eof" => {
                    ended.insert(id);
                    if ended.len() == 2 {
                        break;
                    }
                }
                _ => {}
            }
        }
        a.quit().await;
        b.quit().await;
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(loaded, HashSet::from([7, 8]));
        assert_eq!(ended, HashSet::from([7, 8]));
    }

    /// mpv's sockets go where only this user can reach them.
    #[cfg(unix)]
    #[test]
    fn sockets_live_in_a_private_folder() {
        use std::os::unix::fs::PermissionsExt;
        assert!(is_private_dir(&ipc_dir()), "{}", ipc_dir().display());
        assert!(ipc_path(3).starts_with(ipc_dir()));
        assert!(ipc_path(3).as_os_str().len() <= 104);
        let dir = std::env::temp_dir().join(format!("multimusic-private-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(private_dir(&dir));
        assert!(private_dir(&dir));
        // Open to others, or a link to somewhere else: not used.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!private_dir(&dir));
        let link = dir.with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(ipc_dir(), &link).unwrap();
        assert!(!is_private_dir(&link));
        std::fs::remove_file(link).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    /// Renders a generated WAV file (no ReplayGain tags) through a real mpv, if one is
    /// installed, and checks how loud it comes out: the volume is cubic (50% is 1/8, as the
    /// Spotify player does it) and levelling lowers untagged songs by 6 dB.
    #[tokio::test]
    async fn real_mpv_levels() {
        if std::process::Command::new("mpv").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("multimusic-mpv-levels-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        write_test_wav(&wav, 1.0);
        let opts = MpvOptions {
            binary: "mpv".into(),
            volume: 50.0,
            replaygain: true,
            gapless: true,
            audio_device: String::new(),
            exclusive: false,
        };
        let mut peaks = Vec::new();
        for levelling in [true, false] {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mpv = Mpv::spawn(&opts, 20 + levelling as u64, tx).await.unwrap();
            if !levelling {
                mpv.set_replaygain(false).await.unwrap();
            }
            let out = dir.join(format!("out-{levelling}.wav"));
            for (name, value) in [
                ("ao-pcm-file", json!(out.to_string_lossy())),
                ("audio-format", json!("s16")),
                ("ao", json!("pcm")),
            ] {
                mpv.command(json!(["set_property", name, value])).await.unwrap();
            }
            mpv.load(&wav.to_string_lossy(), 0.0).await.unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while let Ok(Some((_, ev))) = tokio::time::timeout_at(deadline, rx.recv()).await {
                if matches!(ev, MpvEvent::EndFile { .. }) {
                    break;
                }
            }
            mpv.quit().await;
            let data = std::fs::read(&out).unwrap();
            let start = data.windows(4).position(|w| w == b"data").unwrap() + 8;
            let peak = data[start..]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
                .max()
                .unwrap();
            peaks.push(peak as f32);
        }
        std::fs::remove_dir_all(dir).unwrap();
        // The test tone peaks at 8000.
        let close = |got: f32, want: f32| (got - want).abs() < want * 0.03;
        assert!(close(peaks[0], 8000.0 * 0.125 * 0.501), "levelling on: {}", peaks[0]);
        assert!(close(peaks[1], 8000.0 * 0.125), "levelling off: {}", peaks[1]);
    }

    fn write_test_wav(path: &std::path::Path, secs: f32) {
        let rate = 8000u32;
        let n = (rate as f32 * secs) as u32;
        let mut data = Vec::with_capacity(44 + n as usize * 2);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + n * 2).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&rate.to_le_bytes());
        data.extend_from_slice(&(rate * 2).to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&(n * 2).to_le_bytes());
        for i in 0..n {
            let s = ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 8000.0) as i16;
            data.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, data).unwrap();
    }
}
