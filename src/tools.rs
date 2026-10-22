//! The helper programs Sumo runs: mpv (local files and SoundCloud), yt-dlp and ffmpeg
//! (downloads). The Windows installer and the macOS app ship them next to Sumo; otherwise
//! they come from the PATH, and on macOS also from Homebrew's folders, which apps started from
//! the Finder don't have on their PATH.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The program to run for a setting such as "mpv" or "/opt/mpv/bin/mpv": a path is used as it
/// is, a bare name is looked up in the bundled tools first.
pub fn resolve(configured: &str, default_name: &str) -> String {
    let configured = configured.trim();
    let name = if configured.is_empty() {
        default_name
    } else {
        configured
    };
    if name.contains('/') || name.contains('\\') {
        return name.to_string();
    }
    candidates(name)
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string())
}

/// Where a bundled (or Homebrew) copy of `name` may be.
fn candidates(name: &str) -> Vec<PathBuf> {
    let file = if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut out = Vec::new();
    if let Some(dir) = exe_dir() {
        // Windows: C:\…\Sumo\tools\mpv.exe
        out.push(dir.join("tools").join(&file));
        if cfg!(target_os = "macos") {
            // Sumo.app/Contents/MacOS/multimusic → Contents/Resources
            let resources = dir.join("..").join("Resources");
            if name == "mpv" {
                out.push(resources.join("mpv.app/Contents/MacOS/mpv"));
            }
            out.push(resources.join("tools").join(&file));
        }
    }
    if cfg!(target_os = "macos") {
        out.push(PathBuf::from("/opt/homebrew/bin").join(name));
        out.push(PathBuf::from("/usr/local/bin").join(name));
    }
    out
}

/// How to get a missing helper program on this system.
pub fn install_hint(program: &str) -> String {
    if cfg!(target_os = "linux") {
        format!("Install it with: sudo pacman -S {program}")
    } else {
        format!("It comes with Sumo: reinstall Sumo, or set the path to your own {program}")
    }
}

/// The folder Sumo's executable is in.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// The folder of the ffmpeg shipped with Sumo (or Homebrew's), for yt-dlp's
/// `--ffmpeg-location`; `None` where ffmpeg simply comes from the PATH.
pub fn bundled_ffmpeg_dir() -> Option<PathBuf> {
    if !cfg!(any(windows, target_os = "macos")) {
        return None;
    }
    let ffmpeg = PathBuf::from(resolve("ffmpeg", "ffmpeg"));
    ffmpeg
        .is_absolute()
        .then(|| ffmpeg.parent().map(Path::to_path_buf))
        .flatten()
}

/// A command for a helper program; on Windows it runs without flashing a console window.
pub fn command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    #[allow(unused_mut)]
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Makes a child process end when Sumo does, even if Sumo crashes. (Linux uses
/// `PR_SET_PDEATHSIG` when spawning; elsewhere stale players are stopped at the next start.)
pub fn end_with_us(child: &tokio::process::Child) {
    #[cfg(windows)]
    {
        use std::sync::OnceLock;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        // The job lives as long as the process: when Sumo exits, Windows closes the
        // handle and ends every process in the job.
        static JOB: OnceLock<usize> = OnceLock::new();
        let job = *JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return 0;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            job as usize
        });
        if let (true, Some(handle)) = (job != 0, child.raw_handle()) {
            unsafe {
                AssignProcessToJobObject(job as *mut std::ffi::c_void, handle);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = child;
}

/// Resident memory of this process in MB, for the About section.
pub fn memory_mb() -> f32 {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let kb: f32 = status
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
        kb / 1024.0
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        counters.cb = size;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) == 0 {
            return 0.0;
        }
        counters.WorkingSetSize as f32 / (1024.0 * 1024.0)
    }
    #[cfg(target_os = "macos")]
    {
        macos_resident_mb()
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    0.0
}

/// macOS: the task's resident size from the Mach kernel.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn macos_resident_mb() -> f32 {
    // SAFETY: task_info fills `info` (of the size given in `count`) for this task.
    unsafe {
        let mut info: libc::mach_task_basic_info = std::mem::zeroed();
        let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
        let ok = libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut _ as libc::task_info_t,
            &mut count,
        );
        if ok != libc::KERN_SUCCESS {
            return 0.0;
        }
        let resident = info.resident_size;
        resident as f32 / (1024.0 * 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_missing_tools() {
        assert_eq!(resolve("/opt/mpv/bin/mpv", "mpv"), "/opt/mpv/bin/mpv");
        assert_eq!(resolve("C:\\mpv\\mpv.exe", "mpv"), "C:\\mpv\\mpv.exe");
        assert_eq!(resolve("", "no-such-tool-here"), "no-such-tool-here");
        assert_eq!(resolve("  no-such-tool-here ", "mpv"), "no-such-tool-here");
    }

    #[test]
    fn memory_is_measured() {
        assert!(memory_mb() > 0.0);
    }
}
