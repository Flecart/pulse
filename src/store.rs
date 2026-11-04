//! Optional on-disk history: one JSON-lines file per UTC day in `data_dir`,
//! files older than `retention_days` are deleted. A line is
//!   {"c":"<check>","t":<unix>,"ok":true,"ms":12,"d":"HTTP 200"}
//! and `d` is omitted when it equals the check's previous detail (most lines).

use crate::state::Sample;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::Mutex,
};

#[derive(Serialize, Deserialize)]
struct Line<'a> {
    c: std::borrow::Cow<'a, str>,
    t: i64,
    ok: bool,
    ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    d: Option<std::borrow::Cow<'a, str>>,
}

pub struct Store {
    dir: PathBuf,
    retention_days: i64,
    inner: Mutex<Inner>,
}

struct Inner {
    day: i64,
    file: Option<File>,
    last_detail: HashMap<String, String>,
    warned: bool,
}

/// Days since 1970-01-01 -> "YYYY-MM-DD" (proleptic Gregorian, H. Hinnant's algorithm).
pub fn date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

impl Store {
    pub fn open(dir: &str, retention_days: u32) -> Result<Self, String> {
        let dir = PathBuf::from(dir);
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Store {
            dir,
            retention_days: retention_days.max(1) as i64,
            inner: Mutex::new(Inner { day: i64::MIN, file: None, last_detail: HashMap::new(), warned: false }),
        })
    }

    fn path(&self, day: i64) -> PathBuf {
        self.dir.join(format!("{}.jsonl", date(day)))
    }

    /// Delete day files that fell out of the retention window.
    fn prune(&self, today: i64) {
        let Ok(rd) = fs::read_dir(&self.dir) else { return };
        let keep: Vec<String> = (0..self.retention_days).map(|i| format!("{}.jsonl", date(today - i))).collect();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".jsonl") && name.len() == 16 && !keep.contains(&name) {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    /// All kept samples, oldest first, as (check name, sample).
    pub fn load(&self, now: i64) -> Vec<(String, Sample)> {
        let today = now.div_euclid(86_400);
        self.prune(today);
        let cutoff = now - self.retention_days * 86_400;
        let mut out = Vec::new();
        let mut last: HashMap<String, String> = HashMap::new();
        for day in (today - self.retention_days + 1)..=today {
            let Ok(f) = File::open(self.path(day)) else { continue };
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                let Ok(l) = serde_json::from_str::<Line>(&line) else { continue }; // torn last line etc.
                let name = l.c.to_string();
                let detail = match l.d {
                    Some(d) => {
                        last.insert(name.clone(), d.to_string());
                        d.to_string()
                    }
                    None => last.get(&name).cloned().unwrap_or_default(),
                };
                if l.t >= cutoff {
                    out.push((name, Sample { at: l.t, ok: l.ok, latency_ms: l.ms, detail }));
                }
            }
        }
        out.sort_by_key(|(_, s)| s.at);
        out
    }

    pub fn append(&self, check: &str, s: &Sample) {
        let mut g = self.inner.lock().unwrap();
        let day = s.at.div_euclid(86_400);
        if day != g.day || g.file.is_none() {
            g.file = OpenOptions::new().create(true).append(true).open(self.path(day)).ok();
            g.day = day;
            g.last_detail.clear(); // each day file is self-contained
            self.prune(day);
        }
        let same = g.last_detail.get(check).is_some_and(|d| *d == s.detail);
        let line = Line {
            c: check.into(),
            t: s.at,
            ok: s.ok,
            ms: s.latency_ms,
            d: if same { None } else { Some(s.detail.as_str().into()) },
        };
        let mut buf = serde_json::to_string(&line).unwrap_or_default();
        buf.push('\n');
        let res = match g.file.as_mut() {
            Some(f) => f.write_all(buf.as_bytes()),
            None => Err(std::io::Error::other("cannot open day file")),
        };
        match res {
            Ok(()) => {
                if !same {
                    g.last_detail.insert(check.to_string(), s.detail.clone());
                }
            }
            Err(e) if !g.warned => {
                eprintln!("pulse: cannot write history to {}: {e}", self.dir.display());
                g.warned = true;
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(20_761), "2026-11-04");
        assert_eq!(date(11_016), "2000-02-29");
    }

    #[test]
    fn roundtrip_dedupe_and_retention() {
        let dir = std::env::temp_dir().join(format!("pulse-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let st = Store::open(dir.to_str().unwrap(), 7).unwrap();
        let now = 20_759 * 86_400 + 3600;
        // an old file that must be pruned
        fs::write(st.path(20_759 - 10), "{}\n").unwrap();
        for i in 0..5 {
            let s = Sample { at: now + i, ok: i != 3, latency_ms: 5, detail: if i == 3 { "boom".into() } else { "HTTP 200".into() } };
            st.append("web", &s);
        }
        let text = fs::read_to_string(st.path(20_759)).unwrap();
        assert_eq!(text.matches("\"d\"").count(), 3, "detail only on change: {text}");
        let loaded = st.load(now + 10);
        assert_eq!(loaded.len(), 5);
        assert!(loaded.iter().all(|(c, _)| c == "web"));
        assert_eq!(loaded[2].1.detail, "HTTP 200");
        assert_eq!(loaded[3].1.detail, "boom");
        assert!(!loaded[3].1.ok);
        assert!(!st.path(20_759 - 10).exists(), "old file pruned");
        let _ = fs::remove_dir_all(&dir);
    }
}
