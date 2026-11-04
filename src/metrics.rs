//! Machine-level (USE) metrics from /proc, statvfs and nvidia-smi, and per-site
//! RED metrics from Caddy's Prometheus endpoint. Rate metrics are computed as
//! deltas since the previous run of the same check (the first run measures over 1 s).

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    CpuPercent,
    IowaitPercent,
    StealPercent,
    LoadPerCore,
    MemAvailablePercent,
    SwapPagesPerSec,
    OomKills,
    DiskFreePercent,
    InodesFreePercent,
    MountWritable,
    DiskLatencyMs,
    DiskQueue,
    NetDropsPerSec,
    NetErrorsPerSec,
    TcpRetransPercent,
    TcpConnections,
    FdUsedPercent,
    GpuUtilPercent,
    GpuMemPercent,
    GpuTempC,
    GpuThrottle,
}

/// (name, metric, needs: "path" | "device" | "iface" | "", default bound, bound is a minimum)
const TABLE: &[(&str, Metric, &str, f64, bool)] = &[
    ("cpu_percent", Metric::CpuPercent, "", 95.0, false),
    ("iowait_percent", Metric::IowaitPercent, "", 30.0, false),
    ("steal_percent", Metric::StealPercent, "", 20.0, false),
    ("load_per_core", Metric::LoadPerCore, "", 2.0, false),
    ("mem_available_percent", Metric::MemAvailablePercent, "", 10.0, true),
    ("swap_pages_per_sec", Metric::SwapPagesPerSec, "", 1000.0, false),
    ("oom_kills", Metric::OomKills, "", 0.0, false),
    ("disk_free_percent", Metric::DiskFreePercent, "path", 10.0, true),
    ("inodes_free_percent", Metric::InodesFreePercent, "path", 10.0, true),
    ("mount_writable", Metric::MountWritable, "path", 0.0, false),
    ("disk_latency_ms", Metric::DiskLatencyMs, "device", 100.0, false),
    ("disk_queue", Metric::DiskQueue, "device", 16.0, false),
    ("net_drops_per_sec", Metric::NetDropsPerSec, "iface", 100.0, false),
    ("net_errors_per_sec", Metric::NetErrorsPerSec, "iface", 10.0, false),
    ("tcp_retrans_percent", Metric::TcpRetransPercent, "", 5.0, false),
    ("tcp_connections", Metric::TcpConnections, "", 10000.0, false),
    ("fd_used_percent", Metric::FdUsedPercent, "", 80.0, false),
    ("gpu_util_percent", Metric::GpuUtilPercent, "", 100.0, false),
    ("gpu_mem_percent", Metric::GpuMemPercent, "", 95.0, false),
    ("gpu_temp_c", Metric::GpuTempC, "", 87.0, false),
    ("gpu_throttle", Metric::GpuThrottle, "", 0.0, false),
];

pub fn names() -> Vec<&'static str> {
    TABLE.iter().map(|t| t.0).collect()
}

pub struct MetricProbe {
    metric: Metric,
    arg: String,
    bound: f64,
    is_min: bool,
    prev: Mutex<Option<(Instant, Vec<f64>)>>,
}

fn read(p: &str) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))
}

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| l.strip_prefix(key)).map(str::trim)
}

impl MetricProbe {
    pub fn new(name: &str, path: Option<&str>, device: Option<&str>, iface: Option<&str>, max: Option<f64>, min: Option<f64>) -> Result<Self, String> {
        let &(n, metric, needs, default, default_is_min) =
            TABLE.iter().find(|t| t.0 == name).ok_or_else(|| format!("unknown metric {name:?}; known: {}", names().join(", ")))?;
        let arg = match needs {
            "path" => path.ok_or(format!("metric {n} needs `path`"))?.to_string(),
            "device" => device.ok_or(format!("metric {n} needs `device` (e.g. nvme0n1)"))?.to_string(),
            "iface" => iface.ok_or(format!("metric {n} needs `iface` (e.g. eth0)"))?.to_string(),
            _ => String::new(),
        };
        let (bound, is_min) = match (max, min) {
            (Some(_), Some(_)) => return Err("set either max or min, not both".into()),
            (Some(m), None) => (m, false),
            (None, Some(m)) => (m, true),
            (None, None) => (default, default_is_min),
        };
        Ok(MetricProbe { metric, arg, bound, is_min, prev: Mutex::new(None) })
    }

    /// Counters for delta metrics, or None for gauges.
    fn counters(&self) -> Result<Option<Vec<f64>>, String> {
        Ok(Some(match self.metric {
            Metric::CpuPercent | Metric::IowaitPercent | Metric::StealPercent => {
                let s = read("/proc/stat")?;
                let line = s.lines().find(|l| l.starts_with("cpu ")).ok_or("no cpu line")?;
                let v: Vec<f64> = line.split_whitespace().skip(1).take(8).filter_map(|x| x.parse().ok()).collect();
                if v.len() < 8 {
                    return Err("short cpu line".into());
                }
                // [total, idle, iowait, steal]
                vec![v.iter().sum(), v[3], v[4], v[7]]
            }
            Metric::SwapPagesPerSec | Metric::OomKills => {
                let s = read("/proc/vmstat")?;
                let g = |k: &str| -> f64 {
                    s.lines().find_map(|l| l.strip_prefix(k).and_then(|r| r.trim().parse().ok())).unwrap_or(0.0)
                };
                vec![g("pswpin ") + g("pswpout "), g("oom_kill ")]
            }
            Metric::DiskLatencyMs | Metric::DiskQueue => {
                let s = read("/proc/diskstats")?;
                let f: Vec<f64> = s
                    .lines()
                    .map(|l| l.split_whitespace().collect::<Vec<_>>())
                    .find(|w| w.len() > 13 && w[2] == self.arg)
                    .ok_or(format!("device {} not in /proc/diskstats", self.arg))?[3..]
                    .iter()
                    .filter_map(|x| x.parse().ok())
                    .collect();
                // [ios, io_ms, weighted_ms]
                vec![f[0] + f[4], f[3] + f[7], f[10]]
            }
            Metric::NetDropsPerSec | Metric::NetErrorsPerSec => {
                let s = read("/proc/net/dev")?;
                let rest = s
                    .lines()
                    .find_map(|l| l.trim_start().strip_prefix(&format!("{}:", self.arg)))
                    .ok_or(format!("interface {} not found", self.arg))?;
                let f: Vec<f64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if f.len() < 12 {
                    return Err("short /proc/net/dev line".into());
                }
                // [drops, errors]
                vec![f[3] + f[11], f[2] + f[10]]
            }
            Metric::TcpRetransPercent => {
                let s = read("/proc/net/snmp")?;
                let mut tcp = s.lines().filter(|l| l.starts_with("Tcp:"));
                let (h, v) = (tcp.next().ok_or("no Tcp header")?, tcp.next().ok_or("no Tcp values")?);
                let m: HashMap<&str, f64> =
                    h.split_whitespace().zip(v.split_whitespace()).filter_map(|(k, x)| Some((k, x.parse().ok()?))).collect();
                vec![m.get("OutSegs").copied().unwrap_or(0.0), m.get("RetransSegs").copied().unwrap_or(0.0)]
            }
            _ => return Ok(None),
        }))
    }

    /// Value of the metric plus a unit suffix for the detail line.
    async fn value(&self) -> Result<(f64, &'static str), String> {
        if let Some(cur) = self.counters()? {
            let now = Instant::now();
            let prev = self.prev.lock().unwrap().replace((now, cur.clone()));
            let (t0, old) = match prev {
                Some(p) => p,
                None => {
                    // first run: measure over one second
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let next = self.counters()?.unwrap_or_default();
                    let t1 = Instant::now();
                    *self.prev.lock().unwrap() = Some((t1, next.clone()));
                    return Ok(self.from_delta(&cur, &next, t1 - now));
                }
            };
            return Ok(self.from_delta(&old, &cur, now - t0));
        }
        self.gauge().await
    }

    fn from_delta(&self, a: &[f64], b: &[f64], dt: Duration) -> (f64, &'static str) {
        let d: Vec<f64> = a.iter().zip(b).map(|(x, y)| (y - x).max(0.0)).collect();
        let secs = dt.as_secs_f64().max(0.001);
        let pct = |n: f64, of: f64| if of > 0.0 { 100.0 * n / of } else { 0.0 };
        match self.metric {
            Metric::CpuPercent => (pct(d[0] - d[1] - d[2], d[0]), "%"),
            Metric::IowaitPercent => (pct(d[2], d[0]), "%"),
            Metric::StealPercent => (pct(d[3], d[0]), "%"),
            Metric::SwapPagesPerSec => (d[0] / secs, " pages/s"),
            Metric::OomKills => (d[1], " kills"),
            Metric::DiskLatencyMs => (if d[0] > 0.0 { d[1] / d[0] } else { 0.0 }, " ms/io"),
            Metric::DiskQueue => (d[2] / (secs * 1000.0), " avg in flight"),
            Metric::NetDropsPerSec => (d[0] / secs, " drops/s"),
            Metric::NetErrorsPerSec => (d[1] / secs, " errors/s"),
            Metric::TcpRetransPercent => (pct(d[1], d[0]), "% retransmitted"),
            _ => (0.0, ""),
        }
    }

    async fn gauge(&self) -> Result<(f64, &'static str), String> {
        Ok(match self.metric {
            Metric::LoadPerCore => {
                let l: f64 = read("/proc/loadavg")?.split_whitespace().next().and_then(|x| x.parse().ok()).ok_or("bad loadavg")?;
                let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
                (l / n, " load/core")
            }
            Metric::MemAvailablePercent => {
                let s = read("/proc/meminfo")?;
                let kb = |k: &str| -> f64 { field(&s, k).and_then(|v| v.split_whitespace().next()?.parse().ok()).unwrap_or(0.0) };
                let (avail, total) = (kb("MemAvailable:"), kb("MemTotal:"));
                (if total > 0.0 { 100.0 * avail / total } else { 0.0 }, "% available")
            }
            Metric::DiskFreePercent | Metric::InodesFreePercent => {
                let st = statvfs(&self.arg)?;
                if self.metric == Metric::DiskFreePercent {
                    (if st.0 > 0.0 { 100.0 * st.1 / st.0 } else { 100.0 }, "% free")
                } else {
                    (if st.2 > 0.0 { 100.0 * st.3 / st.2 } else { 100.0 }, "% inodes free")
                }
            }
            Metric::MountWritable => {
                // PID 1's view: our own sandbox mounts everything read-only.
                let s = read("/proc/1/mounts")?;
                let opts = s
                    .lines()
                    .map(|l| l.split_whitespace().collect::<Vec<_>>())
                    .filter(|w| w.len() >= 4 && w[1] == self.arg)
                    .last()
                    .ok_or(format!("{} is not a mount point", self.arg))?[3]
                    .to_string();
                (if opts.split(',').any(|o| o == "ro") { 1.0 } else { 0.0 }, "")
            }
            Metric::TcpConnections => {
                let mut n = 0.0;
                for f in ["/proc/net/sockstat", "/proc/net/sockstat6"] {
                    if let Ok(s) = read(f) {
                        n += s
                            .lines()
                            .find_map(|l| l.strip_prefix("TCP: inuse ").or_else(|| l.strip_prefix("TCP6: inuse ")))
                            .and_then(|r| r.split_whitespace().next()?.parse::<f64>().ok())
                            .unwrap_or(0.0);
                    }
                }
                (n, " TCP sockets")
            }
            Metric::FdUsedPercent => {
                let s = read("/proc/sys/fs/file-nr")?;
                let v: Vec<f64> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if v.len() < 3 || v[2] <= 0.0 {
                    return Err("bad file-nr".into());
                }
                (100.0 * v[0] / v[2], "% of fs.file-max")
            }
            Metric::GpuUtilPercent | Metric::GpuMemPercent | Metric::GpuTempC | Metric::GpuThrottle => {
                let out = tokio::process::Command::new("nvidia-smi")
                    .args([
                        "--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu,clocks_throttle_reasons.active",
                        "--format=csv,noheader,nounits",
                    ])
                    .output()
                    .await
                    .map_err(|e| format!("nvidia-smi: {e}"))?;
                if !out.status.success() {
                    return Err(format!("nvidia-smi: {}", String::from_utf8_lossy(&out.stderr).trim()));
                }
                let line = String::from_utf8_lossy(&out.stdout);
                let f: Vec<&str> = line.lines().next().unwrap_or("").split(',').map(str::trim).collect();
                if f.len() < 5 {
                    return Err(format!("unexpected nvidia-smi output {line:?}"));
                }
                let num = |i: usize| f[i].parse::<f64>().map_err(|_| format!("bad nvidia-smi field {:?}", f[i]));
                match self.metric {
                    Metric::GpuUtilPercent => (num(0)?, "% GPU busy"),
                    Metric::GpuMemPercent => (100.0 * num(1)? / num(2)?.max(1.0), "% VRAM used"),
                    Metric::GpuTempC => (num(3)?, " °C"),
                    _ => {
                        // Only harmful reasons: HW slowdown, SW/HW thermal, power brake.
                        let mask = u64::from_str_radix(f[4].trim_start_matches("0x"), 16).unwrap_or(0);
                        ((mask & (0x08 | 0x20 | 0x40 | 0x80)).count_ones() as f64, " throttle reasons")
                    }
                }
            }
            _ => return Err("not a gauge".into()),
        })
    }

    pub async fn run(&self) -> Result<String, String> {
        let (v, unit) = self.value().await?;
        if self.metric == Metric::MountWritable {
            return if v > 0.0 { Err(format!("{} is mounted read-only", self.arg)) } else { Ok(format!("{} writable", self.arg)) };
        }
        let shown = if v.abs() >= 100.0 || v.fract() == 0.0 {
            format!("{v:.0}")
        } else if v.abs() < 1.0 {
            format!("{v:.2}")
        } else {
            format!("{v:.1}")
        };
        let what = if self.arg.is_empty() { String::new() } else { format!(" ({})", self.arg) };
        let bad = if self.is_min { v < self.bound } else { v > self.bound };
        let limit = format!("{} {}", if self.is_min { "min" } else { "max" }, self.bound);
        if bad {
            Err(format!("{shown}{unit}{what} — {limit}"))
        } else {
            Ok(format!("{shown}{unit}{what}"))
        }
    }
}

/// (blocks, blocks available, inodes, inodes available)
fn statvfs(path: &str) -> Result<(f64, f64, f64, f64), String> {
    let c = std::ffi::CString::new(path).map_err(|_| "bad path".to_string())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(format!("{path}: {}", std::io::Error::last_os_error()));
    }
    Ok((st.f_blocks as f64, st.f_bavail as f64, st.f_files as f64, st.f_favail as f64))
}

// ─── Caddy RED metrics ───────────────────────────────────────────────────────

pub struct CaddyProbe {
    pub client: reqwest::Client,
    pub url: String,
    pub host: String,
    pub max_error_percent: f64,
    pub max_p95_ms: Option<f64>,
    pub min_rate: Option<f64>,
    /// previous (instant, per-code request counts, cumulative buckets le->count)
    prev: Mutex<Option<(Instant, HashMap<String, f64>, Vec<(f64, f64)>)>>,
}

fn labels(s: &str) -> HashMap<&str, &str> {
    // k="v",k2="v2"  (Caddy's label values never contain quotes or commas)
    s.split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim(), v.trim().trim_matches('"')))
        })
        .collect()
}

impl CaddyProbe {
    pub fn new(client: reqwest::Client, url: String, host: String, max_error_percent: Option<f64>, max_p95_ms: Option<f64>, min_rate: Option<f64>) -> Self {
        CaddyProbe { client, url, host, max_error_percent: max_error_percent.unwrap_or(5.0), max_p95_ms, min_rate, prev: Mutex::new(None) }
    }

    /// Parse the duration histogram for our host (reverse_proxy handler only, so
    /// each request is counted once): per-code counts and cumulative buckets.
    fn parse(&self, text: &str) -> (bool, HashMap<String, f64>, Vec<(f64, f64)>) {
        let mut any_host_label = false;
        let mut codes: HashMap<String, f64> = HashMap::new();
        let mut buckets: HashMap<String, f64> = HashMap::new();
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("caddy_http_request_duration_seconds_") else { continue };
            let Some((name, rest)) = rest.split_once('{') else { continue };
            let Some((lab, val)) = rest.rsplit_once('}') else { continue };
            let l = labels(lab);
            if l.contains_key("host") {
                any_host_label = true;
            }
            if l.get("host") != Some(&self.host.as_str()) || l.get("handler") != Some(&"reverse_proxy") {
                continue;
            }
            let Ok(v) = val.trim().parse::<f64>() else { continue };
            match name {
                "count" => *codes.entry(l.get("code").unwrap_or(&"?").to_string()).or_default() += v,
                "bucket" => *buckets.entry(l.get("le").unwrap_or(&"+Inf").to_string()).or_default() += v,
                _ => {}
            }
        }
        let mut b: Vec<(f64, f64)> =
            buckets.into_iter().map(|(le, c)| (if le == "+Inf" { f64::INFINITY } else { le.parse().unwrap_or(f64::INFINITY) }, c)).collect();
        b.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap());
        (any_host_label, codes, b)
    }

    pub async fn run(&self) -> Result<String, String> {
        let text = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| format!("metrics: {e}"))?
            .text()
            .await
            .map_err(|e| format!("metrics: {e}"))?;
        let (has_host, codes, buckets) = self.parse(&text);
        if !has_host && !text.contains("caddy_http_request_duration_seconds") {
            return Err("Caddy HTTP metrics are off (global option: metrics { per_host })".into());
        }
        let now = Instant::now();
        let prev = self.prev.lock().unwrap().replace((now, codes.clone(), buckets.clone()));
        let Some((t0, old_codes, old_buckets)) = prev else { return Ok("baseline taken".into()) };
        let mins = (now - t0).as_secs_f64().max(1.0) / 60.0;

        let total: f64 = codes.iter().map(|(c, v)| (v - old_codes.get(c).unwrap_or(&0.0)).max(0.0)).sum();
        let errors: f64 = codes
            .iter()
            .filter(|(c, _)| c.starts_with('5'))
            .map(|(c, v)| (v - old_codes.get(c).unwrap_or(&0.0)).max(0.0))
            .sum();
        let rate = total / mins;
        let err_pct = if total > 0.0 { 100.0 * errors / total } else { 0.0 };
        let delta: Vec<(f64, f64)> = buckets
            .iter()
            .map(|(le, c)| (*le, (c - old_buckets.iter().find(|(l, _)| l == le).map(|x| x.1).unwrap_or(0.0)).max(0.0)))
            .collect();
        let p = |q| quantile(&delta, q).map(|s| s * 1000.0);
        let (p50, p95, p99) = (p(0.50), p(0.95), p(0.99));
        let ms = |v: Option<f64>| v.map(|x| format!("{x:.0}")).unwrap_or_else(|| "–".into());
        let detail = format!(
            "{rate:.1} req/min · {err_pct:.1}% 5xx · p50 {} / p95 {} / p99 {} ms",
            ms(p50),
            ms(p95),
            ms(p99)
        );
        if total >= 1.0 && err_pct > self.max_error_percent {
            return Err(format!("{detail} — 5xx above {}%", self.max_error_percent));
        }
        if let (Some(max), Some(v)) = (self.max_p95_ms, p95) {
            if v > max {
                return Err(format!("{detail} — p95 above {max} ms"));
            }
        }
        if let Some(min) = self.min_rate {
            if rate < min {
                return Err(format!("{detail} — traffic below {min} req/min"));
            }
        }
        Ok(detail)
    }
}

/// Prometheus-style histogram_quantile over cumulative (le, count) buckets.
pub fn quantile(b: &[(f64, f64)], q: f64) -> Option<f64> {
    let total = b.last()?.1;
    if total <= 0.0 {
        return None;
    }
    let rank = q * total;
    let mut prev = (0.0, 0.0);
    for &(le, c) in b {
        if c >= rank {
            if le.is_infinite() {
                return Some(prev.0);
            }
            let span = c - prev.1;
            let frac = if span > 0.0 { (rank - prev.1) / span } else { 1.0 };
            return Some(prev.0 + (le - prev.0) * frac);
        }
        prev = (le, c);
    }
    Some(prev.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_quantile() {
        // 100 requests: 50 under 10 ms, 90 under 100 ms, all under 1 s
        let b = [(0.01, 50.0), (0.1, 90.0), (1.0, 100.0), (f64::INFINITY, 100.0)];
        assert!((quantile(&b, 0.5).unwrap() - 0.01).abs() < 1e-9);
        let p95 = quantile(&b, 0.95).unwrap();
        assert!(p95 > 0.1 && p95 < 1.0, "{p95}");
        assert_eq!(quantile(&[(1.0, 0.0)], 0.5), None);
    }

    #[test]
    fn parses_caddy_metrics_for_one_host() {
        let p = CaddyProbe::new(reqwest::Client::new(), String::new(), "git.home.arpa".into(), None, None, None);
        let text = r#"
caddy_http_request_duration_seconds_bucket{code="200",handler="reverse_proxy",host="git.home.arpa",method="GET",server="srv2",le="0.05"} 8
caddy_http_request_duration_seconds_bucket{code="200",handler="reverse_proxy",host="git.home.arpa",method="GET",server="srv2",le="+Inf"} 10
caddy_http_request_duration_seconds_count{code="200",handler="reverse_proxy",host="git.home.arpa",method="GET",server="srv2"} 10
caddy_http_request_duration_seconds_count{code="502",handler="reverse_proxy",host="git.home.arpa",method="GET",server="srv2"} 2
caddy_http_request_duration_seconds_count{code="200",handler="encode",host="git.home.arpa",method="GET",server="srv2"} 99
caddy_http_request_duration_seconds_count{code="200",handler="reverse_proxy",host="leaf.home.arpa",method="GET",server="srv2"} 7
"#;
        let (has, codes, buckets) = p.parse(text);
        assert!(has);
        assert_eq!(codes.get("200"), Some(&10.0));
        assert_eq!(codes.get("502"), Some(&2.0));
        assert_eq!(buckets.len(), 2);
    }

    #[tokio::test]
    async fn host_metrics_read_on_linux() {
        for (name, path, dev, iface) in [
            ("cpu_percent", None, None, None),
            ("load_per_core", None, None, None),
            ("mem_available_percent", None, None, None),
            ("disk_free_percent", Some("/"), None, None),
            ("inodes_free_percent", Some("/"), None, None),
            ("net_drops_per_sec", None, None, Some("lo")),
            ("tcp_retrans_percent", None, None, None),
            ("tcp_connections", None, None, None),
            ("fd_used_percent", None, None, None),
        ] {
            let m = MetricProbe::new(name, path, dev, iface, Some(f64::MAX), None).unwrap();
            let r = m.run().await;
            assert!(r.is_ok(), "{name}: {r:?}");
        }
        assert!(MetricProbe::new("nope", None, None, None, None, None).is_err());
        assert!(MetricProbe::new("disk_free_percent", None, None, None, None, None).is_err());
    }
}
