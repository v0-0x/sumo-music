//! Updates from GitHub releases. A new version is downloaded, checked against the release's
//! SHA-256 sums and installed: on Windows the installer runs silently and starts Sumo
//! again, on macOS the app is swapped in place, and on Arch Linux the release's package is
//! installed with `sudo pacman -U` after asking for the password.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The file with the checksums of a release's files.
pub const SUMS_FILE: &str = "SHA256SUMS.txt";

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// "0.3.0" (without the tag's "v").
    pub version: String,
    pub notes: String,
    /// The release's web page.
    pub page: String,
    /// This platform's download; `None` on Linux or when the release has none.
    pub asset: Option<Asset>,
    pub sums: Option<Asset>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub name: String,
    /// The public download link (GitHub's API link when there is none).
    pub url: String,
    pub size: u64,
}

/// Whether `name` is what this platform downloads from the release of `version`.
pub fn is_platform_asset(name: &str, version: &str) -> bool {
    if cfg!(windows) {
        name == format!("Sumo-Setup-{version}-x64.exe")
    } else if cfg!(target_os = "macos") {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "intel"
        };
        name == format!("Sumo-{version}-macos-{arch}.zip")
    } else {
        is_arch_package(name, version, std::env::consts::ARCH)
    }
}

/// An Arch Linux package of `version` for `arch`: `multimusic-0.3.2.r130.gabc1234-1-x86_64.pkg.tar.zst`
/// (the version is followed by the commit it was built from).
pub fn is_arch_package(name: &str, version: &str, arch: &str) -> bool {
    let Some(rest) = name.strip_prefix("multimusic-").and_then(|r| r.strip_prefix(version)) else {
        return false;
    };
    rest.starts_with(['.', '-']) && rest.ends_with(&format!("-{arch}.pkg.tar.zst"))
}

/// Whether this computer can install updates by itself: Windows and macOS always; Linux with
/// pacman and sudo (Arch and its relatives).
pub fn can_install() -> bool {
    static CAN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CAN.get_or_init(|| cfg!(any(windows, target_os = "macos")) || (on_path("pacman") && on_path("sudo")))
}

/// Installing asks for the password first (Linux).
pub const NEEDS_PASSWORD: bool = cfg!(not(any(windows, target_os = "macos")));

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// A password, kept out of logs and wiped from memory when dropped.
#[derive(Clone, PartialEq)]
pub struct Secret(String);

impl Secret {
    pub fn new(text: String) -> Secret {
        Secret(text)
    }

    fn text(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

/// Overwrites text in memory before it's let go.
pub fn wipe(text: &mut String) {
    // SAFETY: zero bytes are valid UTF-8.
    unsafe { text.as_mut_vec().fill(0) };
    text.clear();
}

/// A client for GitHub's API and downloads: no overall time limit (installers are large), but
/// a stalled connection gives up.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

fn get(http: &reqwest::Client, url: &str, accept: &str) -> reqwest::RequestBuilder {
    http.get(url)
        .header("Accept", accept)
        .header("X-GitHub-Api-Version", "2022-11-28")
}

/// The newest release of `repo` ("owner/name"), which is public: no login needed.
pub async fn latest(http: &reqwest::Client, api: &str, repo: &str) -> Result<Release> {
    let url = format!("{api}/repos/{repo}/releases/latest");
    let resp = get(http, &url, "application/vnd.github+json")
        .send()
        .await
        .context("couldn't reach GitHub")?;
    match resp.status().as_u16() {
        200 => {}
        404 => bail!("no releases found"),
        // GitHub allows 60 checks an hour per address without a login.
        403 | 429 => bail!("GitHub is busy ({}); trying again later", resp.status()),
        _ => bail!("GitHub answered {}", resp.status()),
    }
    let json: Value = resp.json().await.context("unexpected answer from GitHub")?;
    parse_release(&json).ok_or_else(|| anyhow!("unexpected answer from GitHub"))
}

pub fn parse_release(json: &Value) -> Option<Release> {
    let tag = json.get("tag_name")?.as_str()?;
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    let assets: Vec<Asset> = json
        .get("assets")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    // Public download links come from GitHub's file servers, which don't count
                    // against the API's hourly limit.
                    let url = a.get("browser_download_url").or_else(|| a.get("url"))?;
                    let name = a.get("name")?.as_str()?;
                    if !is_plain_file_name(name) {
                        return None;
                    }
                    Some(Asset {
                        name: name.to_string(),
                        url: url.as_str()?.to_string(),
                        size: a.get("size").and_then(Value::as_u64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let find = |name: &str| assets.iter().find(|a| a.name == name).cloned();
    Some(Release {
        asset: assets.iter().find(|a| is_platform_asset(&a.name, &version)).cloned(),
        sums: find(SUMS_FILE),
        notes: json.get("body").and_then(Value::as_str).unwrap_or_default().to_string(),
        page: json
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        version,
    })
}

/// Whether `candidate` ("1.2.10") is newer than `current` ("1.2.9"). A pre-release
/// ("1.3.0-beta") comes before its release.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> (Vec<u64>, bool) {
        let (numbers, pre) = match v.trim().split_once('-') {
            Some((n, _)) => (n, true),
            None => (v.trim(), false),
        };
        (numbers.split('.').map(|p| p.parse().unwrap_or(0)).collect(), pre)
    }
    let (a, a_pre) = parts(candidate);
    let (b, b_pre) = parts(current);
    let len = a.len().max(b.len());
    for i in 0..len {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    // Same numbers: a release beats its pre-release.
    !a_pre && b_pre
}

/// Downloads an asset to `dest`, reporting progress (0.0..=1.0).
pub async fn download(http: &reqwest::Client, asset: &Asset, dest: &Path, progress: impl Fn(f32)) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut resp = get(http, &asset.url, "application/octet-stream")
        .send()
        .await
        .context("couldn't start the download")?
        .error_for_status()
        .context("the download was refused")?;
    let total = resp.content_length().unwrap_or(asset.size).max(1);
    // A new file only: never write through something already there.
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .await
        .with_context(|| format!("couldn't write {}", dest.display()))?;
    let mut done = 0u64;
    while let Some(chunk) = resp.chunk().await.context("the download broke off")? {
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        progress((done as f32 / total as f32).min(1.0));
    }
    file.flush().await?;
    Ok(())
}

/// Checks `file` against its line in the release's SHA-256 sums.
pub fn verify(file: &Path, name: &str, sums: &str) -> Result<()> {
    let expected = sums
        .lines()
        .find_map(|line| {
            let (hash, file) = line.trim().split_once(char::is_whitespace)?;
            (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
        })
        .ok_or_else(|| anyhow!("the release has no checksum for {name}"))?;
    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(file)?;
    std::io::copy(&mut f, &mut hasher)?;
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        bail!("the download is damaged (checksum mismatch); try again");
    }
    Ok(())
}

/// A new, empty folder for an update's download inside `base` (Sumo's own cache folder, which
/// only this user can write to), readable by this user only. It is made afresh each time, so
/// nobody else can have put files or links in it, and the checked download can't be swapped
/// before it is installed (as root, on Linux).
pub fn fresh_download_dir(base: &Path) -> Result<PathBuf> {
    let dir = base.join("update");
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&dir)?,
        // A file or a link: remove it, not what it points to.
        Ok(_) => std::fs::remove_file(&dir)?,
        Err(_) => {}
    }
    std::fs::create_dir_all(base)?;
    #[cfg(unix)]
    let folder = {
        let mut folder = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut folder, 0o700);
        folder
    };
    #[cfg(not(unix))]
    let folder = std::fs::DirBuilder::new();
    // Fails if something appeared there in the meantime.
    folder
        .create(&dir)
        .with_context(|| format!("couldn't make {}", dir.display()))?;
    Ok(dir)
}

/// Whether a release's file name can be used as a file name as it is (no folders, no "..").
pub fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':', '\0'])
        && name != "."
        && name != ".."
        && Path::new(name).file_name() == Some(std::ffi::OsStr::new(name))
}

/// Starts installing a downloaded update. Sumo has to quit right after; the new
/// version starts by itself.
pub fn install(file: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        // The installer closes what is left of Sumo, installs over it and starts it.
        std::process::Command::new(file)
            .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"])
            .spawn()
            .context("couldn't start the installer")?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        install_app(file)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = file;
        bail!("installing needs your password")
    }
}

/// Why installing a package didn't work.
#[derive(Debug)]
pub enum InstallError {
    WrongPassword,
    Failed(String),
}

/// Linux: installs a downloaded package with `sudo pacman -U`, giving sudo the password.
pub fn install_package(file: &Path, password: &Secret) -> std::result::Result<(), InstallError> {
    let failed = |e: std::io::Error| InstallError::Failed(format!("couldn't run sudo: {e}"));
    // Check the password on its own first, so a wrong one is told apart from pacman failing.
    let check = sudo(password, &["-v".as_ref()]).map_err(failed)?;
    if !check.status.success() {
        return Err(sudo_refused(&String::from_utf8_lossy(&check.stderr)));
    }
    let install = sudo(
        password,
        &[
            "/usr/bin/pacman".as_ref(),
            "-U".as_ref(),
            "--noconfirm".as_ref(),
            file.as_os_str(),
        ],
    )
    .map_err(failed)?;
    if install.status.success() {
        return Ok(());
    }
    let said = format!(
        "{}\n{}",
        String::from_utf8_lossy(&install.stdout),
        String::from_utf8_lossy(&install.stderr)
    );
    Err(InstallError::Failed(
        last_line(&said).unwrap_or("pacman failed").to_string(),
    ))
}

/// Runs `sudo <args>` with the password on its standard input (in English, so the answer can
/// be understood).
fn sudo(password: &Secret, args: &[&std::ffi::OsStr]) -> std::io::Result<std::process::Output> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    // The system's sudo, not whatever is first on the PATH.
    let program = ["/usr/bin/sudo", "/bin/sudo"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
        .unwrap_or("sudo");
    let mut child = Command::new(program)
        .args(["-S", "-p", ""])
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // sudo doesn't read it while it remembers the password from a moment ago.
        let _ = stdin.write_all(password.text().as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    child.wait_with_output()
}

/// What sudo turning the password down means.
fn sudo_refused(stderr: &str) -> InstallError {
    let lower = stderr.to_lowercase();
    if lower.contains("incorrect password")
        || lower.contains("sorry, try again")
        || lower.contains("no password was provided")
        || lower.contains("authentication failure")
    {
        InstallError::WrongPassword
    } else if lower.contains("not in the sudoers") || lower.contains("not allowed") {
        InstallError::Failed("your account isn't allowed to install software (sudo)".into())
    } else {
        InstallError::Failed(last_line(stderr).unwrap_or("sudo refused").to_string())
    }
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty())
}

/// Linux: starts this program again once the running one has quit (after an update replaced it).
#[cfg(unix)]
pub fn relaunch() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    // The running program's file was just replaced: Linux reports it as "… (deleted)".
    let exe = exe
        .to_str()
        .and_then(|p| p.strip_suffix(" (deleted)"))
        .map(PathBuf::from)
        .unwrap_or(exe);
    std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "while kill -0 \"$0\" 2>/dev/null; do sleep 0.2; done; exec \"$1\"",
        ])
        .arg(std::process::id().to_string())
        .arg(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .context("couldn't start Sumo again")?;
    Ok(())
}

/// macOS: unpacks the new Sumo.app and puts it where the running one is.
#[cfg(target_os = "macos")]
fn install_app(zip: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    // …/Sumo.app/Contents/MacOS/multimusic (MultiMusic.app before the app was renamed)
    let bundle = exe
        .ancestors()
        .nth(3)
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or_else(|| anyhow!("Sumo isn't running from its app bundle"))?
        .to_path_buf();
    let unpack = zip.with_extension("unpacked");
    let _ = std::fs::remove_dir_all(&unpack);
    std::fs::create_dir_all(&unpack)?;
    let status = std::process::Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(zip)
        .arg(&unpack)
        .status()
        .context("couldn't unpack the update")?;
    if !status.success() {
        bail!("couldn't unpack the update");
    }
    let new_app = std::fs::read_dir(&unpack)?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or_else(|| anyhow!("the update has no app in it"))?;
    // The app keeps the name it comes with, so MultiMusic.app becomes Sumo.app.
    let named = new_app
        .file_name()
        .map(|name| bundle.with_file_name(name))
        .unwrap_or_else(|| bundle.clone());
    let target = if named != bundle && named.exists() {
        bundle.clone()
    } else {
        named
    };
    let old = bundle.with_extension("app-old");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(&bundle, &old).with_context(|| {
        format!(
            "couldn't replace {} (is it in a folder you can write to, like Applications?)",
            bundle.display()
        )
    })?;
    let moved = std::fs::rename(&new_app, &target).or_else(|_| {
        // Another volume: copy it over.
        let ok = std::process::Command::new("/usr/bin/ditto")
            .arg(&new_app)
            .arg(&target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err(std::io::Error::other("copy failed"))
        }
    });
    if let Err(e) = moved {
        let _ = std::fs::rename(&old, &bundle);
        bail!("couldn't install the update: {e}");
    }
    let _ = std::fs::remove_dir_all(&old);
    let _ = std::fs::remove_dir_all(&unpack);
    // Start the new version once this one has quit. The path is an argument, never part of
    // the script.
    std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "while kill -0 \"$0\" 2>/dev/null; do sleep 0.2; done; exec /usr/bin/open \"$1\"",
        ])
        .arg(std::process::id().to_string())
        .arg(&target)
        .spawn()
        .context("couldn't restart Sumo")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0", "0.99.1"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.2.0", "0.2.0-beta"));
        assert!(!is_newer("0.2.0-beta", "0.2.0"));
        assert!(is_newer("0.2.1-beta", "0.2.0"));
    }

    #[test]
    fn releases_parse() {
        let json = serde_json::json!({
            "tag_name": "v0.3.0",
            "body": "Fixes",
            "html_url": "https://github.com/o/r/releases/tag/v0.3.0",
            "assets": [
                {"name": "Sumo-Setup-0.3.0-x64.exe", "url": "https://api.github.com/a/1", "size": 10},
                {"name": "Sumo-0.3.0-macos-arm64.zip", "url": "https://api.github.com/a/2", "size": 20},
                {"name": "Sumo-0.3.0-macos-intel.zip", "url": "https://api.github.com/a/4", "size": 20},
                {"name": "multimusic-0.3.0.r130.gabc1234-1-x86_64.pkg.tar.zst", "url": "https://api.github.com/a/5", "size": 9},
                {"name": "multimusic-0.3.0.r130.gabc1234-1-aarch64.pkg.tar.zst", "url": "https://api.github.com/a/6", "size": 9},
                {"name": "SHA256SUMS.txt", "url": "https://api.github.com/a/3", "size": 1,
                 "browser_download_url": "https://github.com/o/r/releases/download/v0.3.0/SHA256SUMS.txt"}
            ]
        });
        let r = parse_release(&json).unwrap();
        assert_eq!(r.version, "0.3.0");
        assert_eq!(r.notes, "Fixes");
        // The public link when there is one.
        assert_eq!(
            r.sums.unwrap().url,
            "https://github.com/o/r/releases/download/v0.3.0/SHA256SUMS.txt"
        );
        let asset = r.asset.expect("a download for this computer");
        assert!(is_platform_asset(&asset.name, "0.3.0"), "{}", asset.name);
    }

    /// The Arch package has the app's version, so the updater finds it in a release.
    #[test]
    fn package_version_matches() {
        let pkgver = include_str!("../PKGBUILD")
            .lines()
            .find_map(|l| l.strip_prefix("pkgver="))
            .expect("pkgver in PKGBUILD");
        assert_eq!(
            pkgver.trim(),
            env!("CARGO_PKG_VERSION"),
            "set pkgver in PKGBUILD to the version in Cargo.toml"
        );
    }

    #[test]
    fn arch_packages() {
        let name = "multimusic-0.3.2.r130.gabc1234-1-x86_64.pkg.tar.zst";
        assert!(is_arch_package(name, "0.3.2", "x86_64"));
        assert!(!is_arch_package(name, "0.3.2", "aarch64"));
        assert!(!is_arch_package(name, "0.3.1", "x86_64"));
        // 0.3.2 isn't 0.3.20.
        assert!(!is_arch_package(
            "multimusic-0.3.20-1-x86_64.pkg.tar.zst",
            "0.3.2",
            "x86_64"
        ));
        assert!(is_arch_package(
            "multimusic-0.3.2-1-x86_64.pkg.tar.zst",
            "0.3.2",
            "x86_64"
        ));
        assert!(!is_arch_package(
            "multimusic-debug-0.3.2-1-x86_64.pkg.tar.zst",
            "0.3.2",
            "x86_64"
        ));
    }

    #[test]
    fn sudo_answers() {
        assert!(matches!(
            sudo_refused("Sorry, try again.\nsudo: no password was provided\nsudo: 1 incorrect password attempt\n"),
            InstallError::WrongPassword
        ));
        match sudo_refused("simo is not in the sudoers file.\n") {
            InstallError::Failed(why) => assert!(why.contains("isn't allowed"), "{why}"),
            other => panic!("{other:?}"),
        }
        match sudo_refused("sudo: unable to resolve host\nsudo: something else\n") {
            InstallError::Failed(why) => assert_eq!(why, "sudo: something else"),
            other => panic!("{other:?}"),
        }
        let secret = Secret::new("hunter2".into());
        assert_eq!(format!("{secret:?}"), "Secret(…)");
        let mut text = String::from("hunter2");
        wipe(&mut text);
        assert!(text.is_empty());
    }

    #[test]
    fn checksums() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("app.zip");
        std::fs::write(&file, b"hello").unwrap();
        let hash = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let sums = format!("{hash}  app.zip\nabc  other.zip\n");
        verify(&file, "app.zip", &sums).unwrap();
        verify(&file, "app.zip", &format!("{}  *app.zip", hash.to_uppercase())).unwrap();
        assert!(verify(&file, "app.zip", "abc  app.zip").is_err());
        assert!(verify(&file, "missing.zip", &sums).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn downloads_get_a_private_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("update-test");
        let _ = std::fs::remove_dir_all(&base);
        let dir = fresh_download_dir(&base).unwrap();
        std::fs::write(dir.join("left-over"), b"x").unwrap();
        // Made afresh each time.
        let dir = fresh_download_dir(&base).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
            // A link put in its place is removed, not followed.
            let elsewhere = base.join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            std::fs::write(elsewhere.join("keep"), b"x").unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
            let dir = fresh_download_dir(&base).unwrap();
            assert!(!std::fs::symlink_metadata(&dir).unwrap().file_type().is_symlink());
            assert!(elsewhere.join("keep").exists());
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn release_file_names_stay_file_names() {
        assert!(is_plain_file_name("Sumo-Setup-0.5.1-x64.exe"));
        assert!(is_plain_file_name("multimusic-0.5.1-1-x86_64.pkg.tar.zst"));
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "C:x", "/etc/passwd"] {
            assert!(!is_plain_file_name(bad), "{bad:?}");
        }
        let json = serde_json::json!({
            "tag_name": "v9.0.0",
            "assets": [{"name": "../SHA256SUMS.txt", "browser_download_url": "https://x/y"}],
        });
        assert!(parse_release(&json).unwrap().sums.is_none());
    }

    /// The whole check against a stand-in for GitHub's API, which needs no login.
    #[tokio::test]
    async fn latest_release_from_github() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let (status, body) = if head.contains("authorization:") {
                    ("401 Unauthorized", "{}".to_string())
                } else if head.starts_with("get /repos/o/r/releases/latest ") {
                    ("200 OK", r#"{"tag_name":"v9.9.9","assets":[]}"#.to_string())
                } else if head.starts_with("get /repos/o/busy/releases/latest ") {
                    ("403 Forbidden", r#"{"message":"API rate limit exceeded"}"#.to_string())
                } else {
                    ("404 Not Found", r#"{"message":"Not Found"}"#.to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let release = latest(&http, &api, "o/r").await.unwrap();
        assert_eq!(release.version, "9.9.9");
        let err = latest(&http, &api, "o/none").await.unwrap_err().to_string();
        assert_eq!(err, "no releases found");
        let err = latest(&http, &api, "o/busy").await.unwrap_err().to_string();
        assert!(err.contains("busy"), "{err}");
    }
}
