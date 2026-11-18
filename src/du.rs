//! Directory sizes for `dir_size_gb` checks.
//!
//! A walk is one `getdents` + `statx` per entry, staying on one filesystem and
//! never following symlinks; hard links are counted once. Size = allocated blocks
//! (what actually uses the disk), not apparent length, so sparse files don't lie.
//!
//! Service data dirs are usually unreadable for the monitor's own user, so the
//! walk can run elsewhere: `pulse --du-scan` (e.g. a systemd timer with
//! CAP_DAC_READ_SEARCH and no network) writes every `dir_size_gb` path to the
//! config's `du_file`, and the checks then just read that file.

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    os::unix::fs::MetadataExt,
    sync::OnceLock,
    time::Instant,
};

/// Walks stop (with an error) after this many entries, so a runaway dir
/// (millions of tiny files) can't turn a check into a long disk scan.
const MAX_ENTRIES: u64 = 2_000_000;
/// A `du_file` older than this is reported as an error (the scanner died).
pub const MAX_CACHE_AGE_SECS: i64 = 30 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub bytes: u64,
    pub files: u64,
    /// Entries that could not be read (permissions, vanished mid-walk).
    pub unreadable: u64,
    pub scan_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cache {
    /// Unix time of the scan.
    pub at: i64,
    pub paths: BTreeMap<String, Result<Usage, String>>,
}

static CACHE_FILE: OnceLock<String> = OnceLock::new();

pub fn set_cache_file(path: String) {
    let _ = CACHE_FILE.set(path);
}

pub fn walk(root: &str) -> Result<Usage, String> {
    let t = Instant::now();
    let top = std::fs::symlink_metadata(root).map_err(|e| format!("{root}: {e}"))?;
    let dev = top.dev();
    let mut u = Usage { bytes: top.blocks() * 512, files: 0, unreadable: 0, scan_ms: 0 };
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    let mut n = 0u64;
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if n == 0 => return Err(format!("{root}: {e}")), // the top dir itself
            Err(_) => {
                u.unreadable += 1;
                continue;
            }
        };
        for e in rd {
            n += 1;
            if n > MAX_ENTRIES {
                return Err(format!("{root}: more than {MAX_ENTRIES} entries, gave up"));
            }
            let Ok(e) = e else {
                u.unreadable += 1;
                continue;
            };
            let Ok(m) = e.metadata() else {
                // DirEntry::metadata is lstat: symlinks are not followed
                u.unreadable += 1;
                continue;
            };
            if m.dev() != dev {
                continue; // another mount: not this directory's disk
            }
            if m.nlink() > 1 && !m.is_dir() && !seen.insert((m.dev(), m.ino())) {
                continue;
            }
            u.bytes += m.blocks() * 512;
            if m.is_dir() {
                stack.push(e.path());
            } else {
                u.files += 1;
            }
        }
    }
    u.scan_ms = t.elapsed().as_millis() as u64;
    Ok(u)
}

/// Size of `path`: from the scanner's `du_file` if configured, else a walk.
pub async fn usage(path: &str) -> Result<Usage, String> {
    if let Some(f) = CACHE_FILE.get() {
        let text = std::fs::read_to_string(f).map_err(|e| format!("{f}: {e} (is `pulse --du-scan` running?)"))?;
        let c: Cache = serde_json::from_str(&text).map_err(|e| format!("{f}: {e}"))?;
        let age = crate::web::now() - c.at;
        if age > MAX_CACHE_AGE_SECS {
            return Err(format!("{f} is {} min old: `pulse --du-scan` stopped running", age / 60));
        }
        return c.paths.get(path).cloned().ok_or(format!("{path} not in {f} yet (next scan will add it)"))?;
    }
    let p = path.to_string();
    tokio::task::spawn_blocking(move || walk(&p)).await.map_err(|e| e.to_string())?
}

/// `pulse --du-scan`: walk every path and atomically replace `out`.
pub fn scan_to_file(paths: &[String], out: &str) -> Result<Cache, String> {
    let mut c = Cache { at: crate::web::now(), paths: BTreeMap::new() };
    for p in paths {
        c.paths.insert(p.clone(), walk(p));
    }
    let tmp = format!("{out}.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&c).map_err(|e| e.to_string())?).map_err(|e| format!("{tmp}: {e}"))?;
    std::fs::rename(&tmp, out).map_err(|e| format!("{out}: {e}"))?;
    Ok(c)
}

pub fn human(bytes: f64) -> String {
    let (v, u) = match bytes.abs() {
        b if b >= 1e12 => (bytes / 1e12, "TB"),
        b if b >= 1e9 => (bytes / 1e9, "GB"),
        b if b >= 1e6 => (bytes / 1e6, "MB"),
        b if b >= 1e3 => (bytes / 1e3, "kB"),
        _ => (bytes, "B"),
    };
    if v.abs() >= 100.0 || u == "B" { format!("{v:.0} {u}") } else { format!("{v:.1} {u}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_and_counts_hard_links_once() {
        let d = std::env::temp_dir().join(format!("pulse-du-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a"), vec![1u8; 64 * 1024]).unwrap();
        std::fs::write(d.join("sub/b"), vec![1u8; 64 * 1024]).unwrap();
        let one = walk(d.to_str().unwrap()).unwrap();
        std::fs::hard_link(d.join("a"), d.join("a-link")).unwrap();
        std::os::unix::fs::symlink("/usr", d.join("usr-link")).unwrap();
        let two = walk(d.to_str().unwrap()).unwrap();
        assert_eq!(one.files, 2);
        assert!(one.bytes >= 128 * 1024, "{one:?}");
        assert_eq!(two.files, 3); // the symlink counts as a file, its target doesn't
        assert!(two.bytes - one.bytes < 16 * 1024, "hard link counted twice: {one:?} vs {two:?}");
        let out = d.join("du.json");
        let c = scan_to_file(&[d.to_string_lossy().into(), "/nonexistent-pulse".into()], out.to_str().unwrap()).unwrap();
        assert!(c.paths["/nonexistent-pulse"].is_err());
        let back: Cache = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(back.paths.len(), 2);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(512.0), "512 B");
        assert_eq!(human(1.5e9), "1.5 GB");
        assert_eq!(human(-2.5e6), "-2.5 MB");
    }
}
