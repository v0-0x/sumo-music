//! YouTube through yt-dlp: finds the official audio of a song and saves it. Used to download
//! Spotify and Apple Music songs, whose own streams are DRM-protected.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::model::{normalize_artist, normalize_title};

/// A search result.
#[derive(Debug, Clone, PartialEq)]
pub struct Video {
    pub id: String,
    pub title: String,
    pub channel: String,
    /// Seconds.
    pub duration: Option<f64>,
    pub description: String,
}

/// yt-dlp isn't installed (or not where the settings say).
#[derive(Debug)]
pub struct NotInstalled;

impl std::fmt::Display for NotInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("yt-dlp is not installed")
    }
}

impl std::error::Error for NotInstalled {}

/// Versions of a song nobody wants instead of the song (unless the song is one).
const UNWANTED: &[&str] = &[
    "live",
    "remix",
    "cover",
    "karaoke",
    "instrumental",
    "sped up",
    "slowed",
    "nightcore",
    "8d",
    "acapella",
    "a cappella",
    "reverb",
    "bass boosted",
    "1 hour",
    "extended",
    "reaction",
    "tutorial",
];

/// The yt-dlp program and the user's extra options (e.g. `--cookies-from-browser firefox`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct YtDlp {
    pub program: String,
    pub args: Vec<String>,
    /// Convert downloads to MP3 instead of keeping YouTube's Opus / AAC.
    pub mp3: bool,
}

impl YtDlp {
    /// From the settings: the program and its extra options as typed (quotes allowed).
    pub fn new(program: &str, args: &str) -> YtDlp {
        YtDlp {
            // The yt-dlp shipped with Sumo on Windows and macOS, unless a path is set.
            program: crate::tools::resolve(program, "yt-dlp"),
            args: split_args(args),
            mp3: false,
        }
    }

    pub fn with_mp3(mut self, mp3: bool) -> YtDlp {
        self.mp3 = mp3;
        self
    }

    /// Options for saving the audio of a video.
    fn download_args(&self) -> Vec<&'static str> {
        let mut args = vec![
            "--no-playlist",
            "--newline",
            "--progress",
            "--no-mtime",
            // The best audio YouTube has, by bitrate: 256 kbps AAC when it offers it (YouTube
            // Music Premium, with --cookies-from-browser), else ~160 kbps Opus.
            "--format",
            "bestaudio/best",
            "--format-sort",
            "abr,asr",
            "--extract-audio",
        ];
        if self.mp3 {
            // Re-encoded at the highest VBR setting (~245 kbps); it can't add quality, but MP3
            // plays everywhere.
            args.extend(["--audio-format", "mp3", "--audio-quality", "0"]);
        }
        // Otherwise the codec is kept and only moved into an .opus / .m4a file (needs ffmpeg).
        args
    }

    fn command(&self) -> Command {
        let mut cmd = crate::tools::command(&self.program);
        // The user's yt-dlp config file could change file names or formats; their extra
        // options go first so the ones Sumo needs win.
        cmd.args(["--ignore-config", "--no-warnings"]);
        // The ffmpeg that comes with Sumo isn't on the PATH.
        if let Some(dir) = crate::tools::bundled_ffmpeg_dir() {
            cmd.arg("--ffmpeg-location").arg(dir);
        }
        cmd.args(&self.args).stdin(Stdio::null()).kill_on_drop(true);
        cmd
    }

    /// yt-dlp's version, i.e. whether it is installed and runs.
    pub async fn version(&self) -> Result<String> {
        let mut cmd = crate::tools::command(&self.program);
        cmd.arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
            .await
            .map_err(|_| anyhow!("yt-dlp didn't answer"))?
            .map_err(spawn_error)?;
        if !out.status.success() {
            bail!("{}", last_error(&String::from_utf8_lossy(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// Splits options typed in a text field: by spaces, keeping "quoted parts" together.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(current);
    }
    out
}

/// Turns YouTube's "Sign in to confirm you're not a bot" check, which browser cookies get
/// past, into what to do about it.
fn explain(reason: &str) -> String {
    let lower = reason.to_lowercase();
    if lower.contains("not a bot") {
        format!(
            "YouTube wants a signed-in browser: add --cookies-from-browser firefox (or chrome) under \
             Settings → Downloads → Extra yt-dlp options ({reason})"
        )
    } else {
        reason.to_string()
    }
}

fn spawn_error(e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow::Error::new(NotInstalled)
    } else {
        anyhow!(e).context("couldn't run yt-dlp")
    }
}

/// Searches YouTube for `query`.
pub async fn search(ytdlp: &YtDlp, query: &str, limit: usize) -> Result<Vec<Video>> {
    let mut cmd = ytdlp.command();
    cmd.args(["--flat-playlist", "--dump-single-json"])
        .arg(format!("ytsearch{limit}:{query}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .map_err(|_| anyhow!("YouTube search timed out"))?
        .map_err(spawn_error)?;
    if !out.status.success() {
        bail!(
            "YouTube search failed: {}",
            explain(&last_error(&String::from_utf8_lossy(&out.stderr)))
        );
    }
    parse_search(&out.stdout)
}

pub fn parse_search(json: &[u8]) -> Result<Vec<Video>> {
    let v: Value = serde_json::from_slice(json).context("unexpected output from yt-dlp")?;
    let text = |e: &Value, key: &str| e.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let entries = v.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(entries
        .iter()
        .filter_map(|e| {
            let id = text(e, "id");
            // Channels and playlists show up in searches too; videos have 11 character ids.
            let live = matches!(
                e.get("live_status").and_then(Value::as_str),
                Some("is_live" | "is_upcoming")
            );
            if id.len() != 11 || live {
                return None;
            }
            let channel = Some(text(e, "channel"))
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| text(e, "uploader"));
            Some(Video {
                id,
                title: text(e, "title"),
                channel,
                duration: e.get("duration").and_then(Value::as_f64),
                description: text(e, "description"),
            })
        })
        .collect())
}

/// Lowercase words only, padded with spaces so `has(" x ")` finds whole words.
fn words(s: &str) -> String {
    let cleaned: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    format!(" {} ", cleaned.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn has(padded: &str, phrase: &str) -> bool {
    padded.contains(&format!(" {phrase} "))
}

/// Videos that are this song, best first: the artist's auto-generated "Topic" upload (the
/// studio recording) beats official videos, and the length has to match.
pub fn rank<'a>(videos: &'a [Video], title: &str, artist: &str, duration: Option<f64>) -> Vec<&'a Video> {
    let want = normalize_title(title);
    let title_words: Vec<&str> = want.split_whitespace().collect();
    let main_artist = normalize_artist(artist);
    let song = words(title);
    if title_words.is_empty() || main_artist.is_empty() {
        return Vec::new();
    }
    let squashed_artist = main_artist.replace(' ', "");
    let mut scored: Vec<(f64, &Video)> = videos
        .iter()
        .filter_map(|v| {
            let vt = words(&v.title);
            let channel = words(&v.channel);
            let found = title_words.iter().filter(|w| has(&vt, w)).count();
            if found * 5 < title_words.len() * 4 {
                return None;
            }
            if UNWANTED.iter().any(|u| has(&vt, u) && !has(&song, u)) {
                return None;
            }
            let topic = channel.trim() == format!("{main_artist} topic");
            let by_artist = has(&channel, &main_artist)
                || channel.replace(' ', "").contains(&squashed_artist)
                || has(&vt, &main_artist);
            if !topic && !by_artist {
                return None;
            }
            let mut score = 0.0;
            match (duration.filter(|d| *d > 0.0), v.duration) {
                (Some(want), Some(got)) => {
                    let diff = (want - got).abs();
                    if diff > (want * 0.05).max(6.0) {
                        return None;
                    }
                    score += 30.0 - diff * 3.0;
                }
                _ => score -= 15.0,
            }
            if topic {
                score += 40.0;
            }
            if v.description.to_lowercase().contains("provided to youtube by") {
                score += 10.0;
            }
            if has(&vt, "audio") {
                score += 5.0;
            }
            Some((score, v))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.into_iter().map(|(_, v)| v).collect()
}

/// Saves the audio of video `id` into `dir` (Opus or AAC as YouTube has it, no re-encoding).
/// Returns the file.
pub async fn download(ytdlp: &YtDlp, id: &str, dir: &Path, progress: &(dyn Fn(f32) + Send + Sync)) -> Result<PathBuf> {
    let stem = format!(".multimusic-yt-{id}");
    let result = tokio::time::timeout(
        Duration::from_secs(30 * 60),
        run_download(ytdlp, id, dir, &stem, progress),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow!("the YouTube download took too long")));
    if result.is_err() {
        remove_leftovers(dir, &stem).await;
    }
    result
}

async fn run_download(
    ytdlp: &YtDlp,
    id: &str,
    dir: &Path,
    stem: &str,
    progress: &(dyn Fn(f32) + Send + Sync),
) -> Result<PathBuf> {
    let mut cmd = ytdlp.command();
    cmd.args(ytdlp.download_args())
        .args([
            "--progress-template",
            "download:mmprog %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s",
            "--print",
            "after_move:mmfile %(filepath)s",
            "--paths",
        ])
        .arg(dir)
        .arg("--output")
        .arg(format!("{stem}.%(ext)s"))
        .arg(format!("https://www.youtube.com/watch?v={id}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(spawn_error)?;
    // Progress, the saved path and errors come on both stdout and stderr.
    let (tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    forward_lines(child.stdout.take().context("no stdout")?, tx.clone());
    forward_lines(child.stderr.take().context("no stderr")?, tx);
    let mut file = None;
    let mut log = String::new();
    while let Some(line) = lines.recv().await {
        handle_line(&line, &mut file, &mut log, progress);
    }
    let status = child.wait().await.context("yt-dlp didn't finish")?;
    if !status.success() {
        let reason = last_error(&log);
        if reason.contains("ffmpeg") || reason.contains("ffprobe") {
            bail!(
                "yt-dlp needs ffmpeg to save audio. {}",
                crate::tools::install_hint("ffmpeg")
            );
        }
        bail!("YouTube: {}", explain(&reason));
    }
    let path = match file.map(PathBuf::from).filter(|p| p.exists()) {
        Some(p) => p,
        None => find_output(dir, stem)
            .await
            .context("yt-dlp finished but the file isn't there")?,
    };
    progress(1.0);
    Ok(path)
}

fn forward_lines(
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

fn handle_line(line: &str, file: &mut Option<String>, log: &mut String, progress: &(dyn Fn(f32) + Send + Sync)) {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("mmprog ") {
        let n: Vec<Option<f64>> = rest.split_whitespace().map(|x| x.parse().ok()).collect();
        let done = n.first().copied().flatten();
        let total = n.get(1).copied().flatten().or(n.get(2).copied().flatten());
        if let (Some(done), Some(total)) = (done, total.filter(|t| *t > 0.0)) {
            // The last few percent are ffmpeg moving the audio into its file.
            progress(((done / total) as f32 * 0.97).min(0.97));
        }
    } else if let Some(path) = line.strip_prefix("mmfile ") {
        *file = Some(path.to_string());
    } else if !line.is_empty() {
        log.push_str(line);
        log.push('\n');
    }
}

/// The most useful line of yt-dlp's output: its last error, without the "[youtube] id:" part.
fn last_error(output: &str) -> String {
    let line = output
        .lines()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .or_else(|| output.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("yt-dlp failed");
    let line = line.trim_start_matches("ERROR:").trim();
    // "[youtube] dQw4w9WgXcQ: Sign in to confirm your age" → "Sign in to confirm your age"
    let line = match line.strip_prefix('[').and_then(|r| r.split_once("] ")) {
        Some((_, rest)) => rest.split_once(": ").map_or(rest, |(_, msg)| msg),
        None => line,
    };
    line.chars().take(200).collect()
}

async fn find_output(dir: &Path, stem: &str) -> Option<PathBuf> {
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let temporary = [".part", ".ytdl", ".temp", ".webm"].iter().any(|t| name.ends_with(t));
        if name.starts_with(&format!("{stem}.")) && !temporary {
            return Some(entry.path());
        }
    }
    None
}

async fn remove_leftovers(dir: &Path, stem: &str) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_string_lossy().starts_with(stem) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(id: &str, title: &str, channel: &str, duration: f64, description: &str) -> Video {
        Video {
            id: id.into(),
            title: title.into(),
            channel: channel.into(),
            duration: Some(duration),
            description: description.into(),
        }
    }

    #[test]
    fn parses_search_results() {
        let json = br#"{"_type":"playlist","entries":[
            {"_type":"url","id":"dQw4w9WgXcQ","title":"Never Gonna Give You Up","channel":"Rick Astley - Topic","duration":213.0,"description":"Provided to YouTube by Sony Music"},
            {"_type":"url","id":"UCuAXFkgsw1L7xaCfnd5JJOw","title":"Rick Astley","uploader":"Rick Astley"},
            {"_type":"url","id":"abcdefghijk","title":"Live now","channel":"x","live_status":"is_live"},
            {"_type":"url","id":"lYBUbBu4W08","title":"Rick Astley - Never Gonna Give You Up (Official Video)","uploader":"Rick Astley","duration":212}
        ]}"#;
        let found = parse_search(json).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].channel, "Rick Astley - Topic");
        assert_eq!(found[0].duration, Some(213.0));
        assert_eq!(found[1].channel, "Rick Astley");
        assert!(parse_search(b"not json").is_err());
    }

    #[test]
    fn picks_the_studio_recording() {
        let videos = vec![
            video(
                "aaaaaaaaaaa",
                "Rick Astley - Never Gonna Give You Up (Official Music Video)",
                "Rick Astley",
                213.0,
                "",
            ),
            video(
                "bbbbbbbbbbb",
                "Never Gonna Give You Up",
                "Rick Astley - Topic",
                214.0,
                "Provided to YouTube by Sony",
            ),
            video(
                "ccccccccccc",
                "Never Gonna Give You Up (Live at Glastonbury)",
                "Rick Astley",
                215.0,
                "",
            ),
            video(
                "ddddddddddd",
                "Never Gonna Give You Up - Karaoke",
                "Sing King",
                213.0,
                "",
            ),
            video("eeeeeeeeeee", "Never Gonna Give You Up (cover)", "Some Band", 213.0, ""),
            video(
                "fffffffffff",
                "Rick Astley - Never Gonna Give You Up (Extended Mix)",
                "RickAstleyVEVO",
                360.0,
                "",
            ),
            video(
                "ggggggggggg",
                "Rick Astley - Together Forever",
                "Rick Astley",
                213.0,
                "",
            ),
        ];
        let ranked: Vec<&str> = rank(&videos, "Never Gonna Give You Up", "Rick Astley", Some(213.6))
            .iter()
            .map(|v| v.id.as_str())
            .collect();
        assert_eq!(ranked, vec!["bbbbbbbbbbb", "aaaaaaaaaaa"]);

        // Wanted versions are allowed when the song is one; "feat." credits don't matter.
        let live = rank(
            &videos,
            "Never Gonna Give You Up - Live at Glastonbury",
            "Rick Astley",
            Some(215.0),
        );
        assert_eq!(live.first().map(|v| v.id.as_str()), Some("ccccccccccc"));
        let feat = rank(
            &videos,
            "Never Gonna Give You Up (feat. Nobody)",
            "Rick Astley, Nobody",
            Some(214.0),
        );
        assert_eq!(feat.first().map(|v| v.id.as_str()), Some("bbbbbbbbbbb"));
        // Channels named after the artist without spaces count as the artist.
        let vevo = [video(
            "hhhhhhhhhhh",
            "Never Gonna Give You Up",
            "RickAstleyVEVO",
            213.0,
            "",
        )];
        assert_eq!(
            rank(&vevo, "Never Gonna Give You Up", "Rick Astley", Some(213.0)).len(),
            1
        );
        // Nothing for a different song or length.
        assert!(rank(&videos, "Something Else", "Rick Astley", Some(213.0)).is_empty());
        assert!(rank(&videos[1..2], "Never Gonna Give You Up", "Rick Astley", Some(250.0)).is_empty());
    }

    #[test]
    fn download_quality_options() {
        let keep = YtDlp::new("yt-dlp", "").download_args();
        assert!(keep.windows(2).any(|w| w == ["--format-sort", "abr,asr"]));
        assert!(!keep.contains(&"--audio-format"));
        let mp3 = YtDlp::new("yt-dlp", "").with_mp3(true).download_args();
        assert!(mp3.windows(2).any(|w| w == ["--audio-format", "mp3"]));
        assert!(mp3.windows(2).any(|w| w == ["--audio-quality", "0"]));
    }

    #[test]
    fn options_from_the_settings() {
        assert_eq!(
            split_args("  --cookies-from-browser   firefox "),
            vec!["--cookies-from-browser", "firefox"]
        );
        assert_eq!(
            split_args(r#"--cookies "/home/me/my cookies.txt" -4 --proxy ''"#),
            vec!["--cookies", "/home/me/my cookies.txt", "-4", "--proxy", ""]
        );
        assert!(split_args("").is_empty());
    }

    #[test]
    fn readable_errors() {
        let out = "[youtube] Extracting URL\nERROR: [youtube] dQw4w9WgXcQ: Sign in to confirm your age. This video may be inappropriate\n";
        assert_eq!(
            last_error(out),
            "Sign in to confirm your age. This video may be inappropriate"
        );
        assert_eq!(
            last_error("ERROR: Postprocessing: ffprobe and ffmpeg not found."),
            "Postprocessing: ffprobe and ffmpeg not found."
        );
        assert_eq!(last_error(""), "yt-dlp failed");
        assert!(explain("Sign in to confirm you're not a bot. Use --cookies-from-browser").contains("Settings"));
        assert_eq!(
            explain("Sign in to confirm your age. This video may be inappropriate"),
            "Sign in to confirm your age. This video may be inappropriate"
        );
    }

    /// Writes an executable stand-in for yt-dlp.
    fn fake_ytdlp(dir: &Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n[ \"$1\" = --probe ] && exit 0\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Another test starting a process while this file was open for writing can leave the
        // child holding it for a moment, and running it then fails with "Text file busy".
        for _ in 0..100 {
            match std::process::Command::new(&path).arg("--probe").status() {
                Ok(_) => break,
                Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("can't run {}: {e}", path.display()),
            }
        }
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn runs_ytdlp() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        // Search: the JSON comes back parsed.
        let search_tool = fake_ytdlp(
            &dir,
            "search",
            r#"echo '{"entries":[{"id":"dQw4w9WgXcQ","title":"Never Gonna Give You Up","channel":"Rick Astley - Topic","duration":213}]}'"#,
        );
        let found = search(
            &YtDlp::new(&search_tool, ""),
            "Rick Astley - Never Gonna Give You Up",
            10,
        )
        .await
        .unwrap();
        assert_eq!(found[0].id, "dQw4w9WgXcQ");

        // Download: progress on both streams, then the path of the saved file.
        let download_tool = fake_ytdlp(
            &dir,
            "download",
            r#"dir=""; out=""
while [ $# -gt 0 ]; do
  case "$1" in
    --paths) dir="$2"; shift;;
    --output) out="$2"; shift;;
  esac
  shift
done
file="$dir/$(echo "$out" | sed 's/%(ext)s/opus/')"
echo "mmprog 50 NA 100"
echo "mmprog 100 100 NA" >&2
printf 'OggS' > "$file"
echo "mmfile $file""#,
        );
        let seen = std::sync::Mutex::new(Vec::new());
        let tool = YtDlp::new(&download_tool, "--cookies-from-browser firefox");
        let path = download(&tool, "dQw4w9WgXcQ", &out, &|p| seen.lock().unwrap().push(p))
            .await
            .unwrap();
        assert_eq!(path, out.join(".multimusic-yt-dQw4w9WgXcQ.opus"));
        assert_eq!(std::fs::read(&path).unwrap(), b"OggS");
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.first(), Some(&0.485));
        assert_eq!(seen.last(), Some(&1.0));
        std::fs::remove_file(&path).unwrap();

        // Failures say why and leave nothing behind.
        let failing = fake_ytdlp(
            &dir,
            "failing",
            r#"while [ $# -gt 0 ]; do [ "$1" = --paths ] && dir="$2"; shift; done
touch "$dir/.multimusic-yt-abcdefghijk.webm.part"
echo "ERROR: [youtube] abcdefghijk: Video unavailable" >&2
exit 1"#,
        );
        let err = download(&YtDlp::new(&failing, ""), "abcdefghijk", &out, &|_| {})
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "YouTube: Video unavailable");
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);

        // Not installed.
        let missing = YtDlp::new("/nonexistent/yt-dlp", "");
        let err = search(&missing, "x", 1).await.unwrap_err();
        assert!(err.is::<NotInstalled>());
        assert!(missing.version().await.unwrap_err().is::<NotInstalled>());
        let version = fake_ytdlp(&dir, "version", "echo 2026.09.30");
        assert_eq!(YtDlp::new(&version, "").version().await.unwrap(), "2026.09.30");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
