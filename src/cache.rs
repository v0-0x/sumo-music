//! Keeps the caches on disk (covers, lyrics) from growing without end.

use std::path::Path;
use std::time::SystemTime;

/// Deletes the least recently used files in `dir` until it holds at most `max_bytes`.
/// Returns how many bytes were freed.
pub fn trim(dir: &Path, max_bytes: u64) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut files: Vec<(SystemTime, u64, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            // Read or written, whichever was last.
            let used = match (meta.accessed(), meta.modified()) {
                (Ok(a), Ok(m)) => a.max(m),
                (Ok(t), _) | (_, Ok(t)) => t,
                _ => SystemTime::UNIX_EPOCH,
            };
            Some((used, meta.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= max_bytes {
        return 0;
    }
    files.sort_by_key(|f| f.0);
    let mut freed = 0;
    for (_, len, path) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
            freed += len;
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oldest_files_go_first() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for (i, name) in ["old", "middle", "new"].iter().enumerate() {
            let path = dir.join(name);
            std::fs::write(&path, [0u8; 100]).unwrap();
            let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 * (i as u64 + 1));
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_times(std::fs::FileTimes::new().set_accessed(when).set_modified(when))
                .unwrap();
        }
        assert_eq!(trim(dir, 1000), 0);
        assert_eq!(trim(dir, 150), 200);
        assert!(!dir.join("old").exists() && !dir.join("middle").exists());
        assert!(dir.join("new").exists());
    }
}
