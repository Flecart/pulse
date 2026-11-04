//! `pulse.toml` — everything pulse does is declared here.

use serde::Deserialize;
use std::{collections::HashSet, path::Path};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address of the status page. Keep it on loopback and put a TLS proxy in front.
    #[serde(default = "d_listen")]
    pub listen: String,
    #[serde(default = "d_title")]
    pub title: String,
    /// Default seconds between runs of a check.
    #[serde(default = "d_interval")]
    pub interval_secs: u64,
    /// Default per-check timeout.
    #[serde(default = "d_timeout")]
    pub timeout_secs: u64,
    /// Consecutive failures before a check is DOWN (and an alert is sent).
    #[serde(default = "d_threshold")]
    pub failure_threshold: u32,
    /// Raw samples kept per check in memory (1440 x 60 s = 24 h). Older data is
    /// kept as hourly up/down counts for `retention_days`.
    #[serde(default = "d_history")]
    pub history: usize,
    /// If set, every sample is also appended to <data_dir>/<YYYY-MM-DD>.jsonl and
    /// reloaded on start, so restarts keep history.
    pub data_dir: Option<String>,
    /// Days of history kept (hourly summary in memory, raw lines on disk).
    #[serde(default = "d_retention")]
    pub retention_days: u32,
    /// Extra PEM root certificates trusted by HTTP checks (e.g. a Caddy local CA).
    #[serde(default)]
    pub ca_files: Vec<String>,
    #[serde(default)]
    pub alerts: Alerts,
    #[serde(default, rename = "check")]
    pub checks: Vec<Check>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alerts {
    /// e.g. https://ntfy.sh/<secret-topic>. Unset = no notifications.
    pub ntfy_url: Option<String>,
    /// Optional ntfy access token (Authorization: Bearer).
    pub ntfy_token: Option<String>,
    /// Dead man's switch, e.g. https://hc-ping.com/<uuid>: pinged while pulse runs,
    /// so an external service can tell you when this whole machine is gone.
    pub heartbeat_url: Option<String>,
    #[serde(default = "d_heartbeat")]
    pub heartbeat_secs: u64,
    pub email: Option<Email>,
}

/// Email via the Resend API (https://resend.com/docs/api-reference/emails/send-email).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Email {
    /// e.g. "pulse <alerts@your-domain>" (a Resend-verified domain), or
    /// "pulse <onboarding@resend.dev>", which can only send to your Resend account's address.
    pub from: String,
    pub to: Vec<String>,
    /// File holding the API key. A leading `$CREDENTIALS_DIRECTORY` is expanded,
    /// so the key can come from systemd `LoadCredential[Encrypted]=`.
    pub api_key_file: String,
    #[serde(default = "d_resend")]
    pub api_url: String,
}

fn d_resend() -> String {
    "https://api.resend.com/emails".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Check {
    pub name: String,
    #[serde(default)]
    pub group: String,
    pub interval_secs: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub failure_threshold: Option<u32>,
    /// A run slower than this counts as a failure ("slow").
    pub max_latency_ms: Option<u64>,
    #[serde(flatten)]
    pub probe: Probe,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Probe {
    /// GET a URL. OK = 2xx/3xx (or one of `expect_status`), body contains `contains`,
    /// and (if set) the TLS certificate is valid for at least `cert_min_hours`.
    Http {
        url: String,
        /// Connect to this IP instead of resolving the URL's host (SNI/Host unchanged).
        resolve: Option<String>,
        expect_status: Option<Vec<u16>>,
        contains: Option<String>,
        cert_min_hours: Option<u64>,
    },
    /// TCP connect to `addr`; optionally the server's first bytes must contain `banner`.
    Tcp { addr: String, banner: Option<String> },
    /// Resolve `query` (A/AAAA) at `server`; optionally the answer must include `expect`.
    Dns { server: String, query: String, expect: Option<String> },
    /// Unit must be active, and must not have restarted since the previous run
    /// (catches crash-loops that look "up" most of the time).
    Systemd { unit: String },
    /// Machine metric (USE): see `pulse --list-metrics`. Fails above `max` /
    /// below `min` (each metric has a sensible default bound).
    Metric {
        metric: String,
        path: Option<String>,
        device: Option<String>,
        iface: Option<String>,
        max: Option<f64>,
        min: Option<f64>,
    },
    /// Per-site RED metrics from Caddy's Prometheus endpoint (needs the global
    /// option `metrics { per_host }`): request rate, 5xx %, p50/p95/p99.
    Caddy {
        host: String,
        #[serde(default = "d_caddy_metrics")]
        metrics_url: String,
        max_error_percent: Option<f64>,
        max_p95_ms: Option<f64>,
        /// req/min; a drop to zero can be worse than a spike.
        min_rate: Option<f64>,
    },
}

fn d_caddy_metrics() -> String {
    "http://127.0.0.1:2019/metrics".into()
}

impl Probe {
    pub fn kind(&self) -> &'static str {
        match self {
            Probe::Http { .. } => "http",
            Probe::Tcp { .. } => "tcp",
            Probe::Dns { .. } => "dns",
            Probe::Systemd { .. } => "systemd",
            Probe::Metric { .. } => "metric",
            Probe::Caddy { .. } => "caddy",
        }
    }
    pub fn target(&self) -> String {
        match self {
            Probe::Http { url, resolve, .. } => match resolve {
                Some(ip) => format!("{url} @{ip}"),
                None => url.clone(),
            },
            Probe::Tcp { addr, .. } => addr.clone(),
            Probe::Dns { server, query, .. } => format!("{query} @{server}"),
            Probe::Systemd { unit } => unit.clone(),
            Probe::Metric { metric, path, device, iface, .. } => {
                let arg = path.as_deref().or(device.as_deref()).or(iface.as_deref());
                match arg {
                    Some(a) => format!("{metric} {a}"),
                    None => metric.clone(),
                }
            }
            Probe::Caddy { host, .. } => format!("{host} (Caddy metrics)"),
        }
    }
}

fn d_listen() -> String {
    "127.0.0.1:8097".into()
}
fn d_title() -> String {
    "pulse".into()
}
fn d_interval() -> u64 {
    60
}
fn d_timeout() -> u64 {
    10
}
fn d_threshold() -> u32 {
    3
}
fn d_history() -> usize {
    1440
}
fn d_retention() -> u32 {
    7
}
fn d_heartbeat() -> u64 {
    60
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn validate(&self) -> Result<(), String> {
        let mut names = HashSet::new();
        if self.interval_secs == 0 || self.timeout_secs == 0 || self.failure_threshold == 0 || self.history == 0 || self.retention_days == 0 {
            return Err("interval_secs, timeout_secs, failure_threshold, history and retention_days must be > 0".into());
        }
        if let Some(e) = &self.alerts.email {
            if e.to.is_empty() || e.from.trim().is_empty() || e.api_key_file.trim().is_empty() {
                return Err("[alerts.email] needs from, to and api_key_file".into());
            }
        }
        for c in &self.checks {
            if c.name.trim().is_empty() {
                return Err("a check has an empty name".into());
            }
            if !names.insert(c.name.as_str()) {
                return Err(format!("duplicate check name {:?}", c.name));
            }
            if c.interval_secs == Some(0) || c.timeout_secs == Some(0) || c.failure_threshold == Some(0) {
                return Err(format!("check {:?}: overrides must be > 0", c.name));
            }
            match &c.probe {
                Probe::Http { url, resolve, .. } => {
                    reqwest::Url::parse(url).map_err(|e| format!("check {:?}: bad url: {e}", c.name))?;
                    if let Some(ip) = resolve {
                        ip.parse::<std::net::IpAddr>()
                            .map_err(|e| format!("check {:?}: bad resolve IP: {e}", c.name))?;
                    }
                }
                Probe::Dns { server, .. } => {
                    crate::probe::parse_dns_server(server).map_err(|e| format!("check {:?}: {e}", c.name))?;
                }
                Probe::Tcp { addr, .. } if !addr.contains(':') => {
                    return Err(format!("check {:?}: tcp addr needs host:port", c.name));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_kinds() {
        let cfg = Config::parse(
            r#"
            title = "t"
            [alerts]
            ntfy_url = "https://ntfy.sh/x"
            [alerts.email]
            from = "pulse <onboarding@resend.dev>"
            to = ["me@example.com"]
            api_key_file = "$CREDENTIALS_DIRECTORY/resend"
            [[check]]
            name = "web"
            kind = "http"
            url = "https://git.home.arpa/api/healthz"
            resolve = "10.9.0.1"
            contains = "pass"
            cert_min_hours = 2
            [[check]]
            name = "ssh"
            group = "git"
            kind = "tcp"
            addr = "10.9.0.1:22"
            banner = "SSH-"
            [[check]]
            name = "dns"
            kind = "dns"
            server = "10.8.0.1"
            query = "git.home.arpa"
            expect = "10.9.0.1"
            [[check]]
            name = "caddy"
            kind = "systemd"
            unit = "caddy.service"
            interval_secs = 30
            "#,
        )
        .unwrap();
        let kinds: Vec<_> = cfg.checks.iter().map(|c| c.probe.kind()).collect();
        assert_eq!(kinds, ["http", "tcp", "dns", "systemd"]);
        assert_eq!(cfg.checks[1].group, "git");
        assert_eq!(cfg.checks[3].interval_secs, Some(30));
        assert_eq!(cfg.checks[2].probe.target(), "git.home.arpa @10.8.0.1");
        assert_eq!(cfg.listen, "127.0.0.1:8097");
        assert_eq!(cfg.alerts.ntfy_url.as_deref(), Some("https://ntfy.sh/x"));
        let e = cfg.alerts.email.unwrap();
        assert_eq!(e.to, ["me@example.com"]);
        assert_eq!(e.api_url, "https://api.resend.com/emails");
    }

    #[test]
    fn rejects_duplicates_and_bad_values() {
        let dup = r#"
            [[check]]
            name = "a"
            kind = "systemd"
            unit = "x"
            [[check]]
            name = "a"
            kind = "systemd"
            unit = "y"
        "#;
        assert!(Config::parse(dup).unwrap_err().contains("duplicate"));
        assert!(Config::parse("interval_secs = 0").is_err());
        assert!(Config::parse("bogus = 1").is_err());
        let bad_ip = r#"
            [[check]]
            name = "a"
            kind = "http"
            url = "https://x"
            resolve = "not-an-ip"
        "#;
        assert!(Config::parse(bad_ip).is_err());
    }
}
