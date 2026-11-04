//! The four check kinds. Each run returns an [`Outcome`]; it never panics.

use crate::config::{Check, Probe};
use hickory_resolver::{
    TokioAsyncResolver,
    config::{NameServerConfigGroup, ResolverConfig, ResolverOpts},
};
use std::{
    error::Error,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt, net::TcpStream, process::Command, time::timeout};

#[derive(Debug, Clone)]
pub struct Outcome {
    pub ok: bool,
    pub latency_ms: u64,
    pub detail: String,
}

/// A check, prepared once (HTTP clients are built up front and reused).
pub enum Runner {
    Http {
        client: reqwest::Client,
        url: String,
        expect_status: Option<Vec<u16>>,
        contains: Option<String>,
        cert_min_hours: Option<u64>,
    },
    Tcp {
        addr: String,
        banner: Option<String>,
    },
    Dns {
        server: SocketAddr,
        query: String,
        expect: Option<IpAddr>,
    },
    Systemd {
        unit: String,
        restarts: std::sync::Mutex<Option<u64>>,
    },
    Metric(crate::metrics::MetricProbe),
    Caddy(crate::metrics::CaddyProbe),
}

pub fn parse_dns_server(s: &str) -> Result<SocketAddr, String> {
    s.parse::<SocketAddr>()
        .or_else(|_| s.parse::<IpAddr>().map(|ip| SocketAddr::new(ip, 53)))
        .map_err(|_| format!("bad DNS server {s:?} (want IP or IP:port)"))
}

/// Error plus its whole source chain — reqwest's top-level message alone is useless.
fn chain(e: &dyn Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let m = c.to_string();
        if !s.contains(&m) {
            s.push_str(": ");
            s.push_str(&m);
        }
        cur = c.source();
    }
    s
}

impl Runner {
    pub fn new(check: &Check, roots: &[reqwest::Certificate], t: Duration) -> Result<Self, String> {
        Ok(match &check.probe {
            Probe::Http { url, resolve, expect_status, contains, cert_min_hours } => {
                let mut b = reqwest::Client::builder()
                    .timeout(t)
                    .connect_timeout(t)
                    .redirect(reqwest::redirect::Policy::none())
                    .user_agent(concat!("pulse/", env!("CARGO_PKG_VERSION")))
                    .tls_info(true)
                    .pool_max_idle_per_host(0); // fresh connection each time = real check
                for r in roots {
                    b = b.add_root_certificate(r.clone());
                }
                if let Some(ip) = resolve {
                    let u = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
                    let host = u.host_str().ok_or("url has no host")?.to_string();
                    let port = u.port_or_known_default().unwrap_or(443);
                    let ip: IpAddr = ip.parse().map_err(|e| format!("{e}"))?;
                    b = b.resolve(&host, SocketAddr::new(ip, port));
                }
                Runner::Http {
                    client: b.build().map_err(|e| chain(&e))?,
                    url: url.clone(),
                    expect_status: expect_status.clone(),
                    contains: contains.clone(),
                    cert_min_hours: *cert_min_hours,
                }
            }
            Probe::Tcp { addr, banner } => Runner::Tcp { addr: addr.clone(), banner: banner.clone() },
            Probe::Dns { server, query, expect } => Runner::Dns {
                server: parse_dns_server(server)?,
                query: query.clone(),
                expect: match expect {
                    Some(e) => Some(e.parse().map_err(|_| format!("bad expect IP {e:?}"))?),
                    None => None,
                },
            },
            Probe::Systemd { unit } => Runner::Systemd { unit: unit.clone(), restarts: std::sync::Mutex::new(None) },
            Probe::Metric { metric, path, device, iface, max, min } => Runner::Metric(crate::metrics::MetricProbe::new(
                metric,
                path.as_deref(),
                device.as_deref(),
                iface.as_deref(),
                *max,
                *min,
            )?),
            Probe::Caddy { host, metrics_url, max_error_percent, max_p95_ms, min_rate } => {
                let client = reqwest::Client::builder().timeout(t).build().map_err(|e| chain(&e))?;
                Runner::Caddy(crate::metrics::CaddyProbe::new(
                    client,
                    metrics_url.clone(),
                    host.clone(),
                    *max_error_percent,
                    *max_p95_ms,
                    *min_rate,
                ))
            }
        })
    }

    /// Run once, bounded by `t` (plus a little slack for the inner timeouts).
    pub async fn run(&self, t: Duration) -> Outcome {
        let start = Instant::now();
        let res = match timeout(t + Duration::from_secs(1), self.run_inner(t)).await {
            Ok(r) => r,
            Err(_) => Err(format!("timed out after {}s", t.as_secs())),
        };
        let latency_ms = start.elapsed().as_millis() as u64;
        match res {
            Ok(detail) => Outcome { ok: true, latency_ms, detail },
            Err(detail) => Outcome { ok: false, latency_ms, detail },
        }
    }

    async fn run_inner(&self, t: Duration) -> Result<String, String> {
        match self {
            Runner::Http { client, url, expect_status, contains, cert_min_hours } => {
                let resp = client.get(url).send().await.map_err(|e| chain(&e))?;
                let status = resp.status();
                let status_ok = match expect_status {
                    Some(list) => list.contains(&status.as_u16()),
                    None => status.is_success() || status.is_redirection(),
                };
                if !status_ok {
                    return Err(format!("HTTP {status}"));
                }
                let mut detail = format!("HTTP {}", status.as_u16());
                if let Some(min) = cert_min_hours {
                    let hours = cert_hours_left(&resp)?;
                    if hours < *min as i64 {
                        return Err(format!("certificate expires in {hours}h (< {min}h)"));
                    }
                    detail.push_str(&format!(", cert {hours}h left"));
                }
                if let Some(needle) = contains {
                    let body = resp.text().await.map_err(|e| chain(&e))?;
                    if !body.contains(needle.as_str()) {
                        return Err(format!("HTTP {}, body lacks {needle:?}", status.as_u16()));
                    }
                }
                Ok(detail)
            }

            Runner::Tcp { addr, banner } => {
                let mut s = timeout(t, TcpStream::connect(addr.as_str()))
                    .await
                    .map_err(|_| "connect timed out".to_string())?
                    .map_err(|e| e.to_string())?;
                let Some(want) = banner else { return Ok("connected".into()) };
                let mut buf = [0u8; 256];
                let n = timeout(t, s.read(&mut buf))
                    .await
                    .map_err(|_| "no banner (timed out)".to_string())?
                    .map_err(|e| e.to_string())?;
                let got = String::from_utf8_lossy(&buf[..n]);
                let first = got.lines().next().unwrap_or("").trim().to_string();
                if got.contains(want.as_str()) { Ok(first) } else { Err(format!("unexpected banner {first:?}")) }
            }

            Runner::Dns { server, query, expect } => {
                // A fresh resolver per run: no cache, so an outage shows up immediately.
                let mut opts = ResolverOpts::default();
                opts.timeout = t;
                opts.attempts = 1;
                opts.use_hosts_file = false;
                let group = NameServerConfigGroup::from_ips_clear(&[server.ip()], server.port(), true);
                let r = TokioAsyncResolver::tokio(ResolverConfig::from_parts(None, vec![], group), opts);
                let ans = r.lookup_ip(query.as_str()).await.map_err(|e| e.to_string())?;
                let ips: Vec<IpAddr> = ans.iter().collect();
                let list = ips.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
                match expect {
                    Some(e) if !ips.contains(e) => Err(format!("got [{list}], want {e}")),
                    _ => Ok(list),
                }
            }

            Runner::Systemd { unit, restarts } => {
                let out = timeout(
                    t,
                    Command::new("systemctl")
                        .args(["show", "--property=ActiveState,SubState,NRestarts,LoadState", "--", unit])
                        .output(),
                )
                .await
                .map_err(|_| "systemctl timed out".to_string())?
                .map_err(|e| format!("systemctl: {e}"))?;
                let text = String::from_utf8_lossy(&out.stdout);
                let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)).unwrap_or("").trim().to_string();
                let (active, sub, load) = (get("ActiveState="), get("SubState="), get("LoadState="));
                let n: Option<u64> = get("NRestarts=").parse().ok();
                if load == "not-found" {
                    return Err("unit not found".into());
                }
                let before = std::mem::replace(&mut *restarts.lock().unwrap(), n);
                let state = format!("{active} ({sub})");
                if active != "active" {
                    return Err(state);
                }
                match (before, n) {
                    (Some(b), Some(n)) if n > b => Err(format!("{state}, restarted {}x since last check", n - b)),
                    (_, Some(n)) if n > 0 => Ok(format!("{state}, {n} restarts since boot")),
                    _ => Ok(state),
                }
            }

            Runner::Metric(m) => m.run().await,
            Runner::Caddy(c) => c.run().await,
        }
    }
}

fn cert_hours_left(resp: &reqwest::Response) -> Result<i64, String> {
    let der = resp
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(|i| i.peer_certificate())
        .ok_or("no TLS certificate (plain http?)")?;
    let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|e| format!("bad certificate: {e}"))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    Ok((cert.validity().not_after.timestamp() - now) / 3600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_server_forms() {
        assert_eq!(parse_dns_server("10.8.0.1").unwrap().port(), 53);
        assert_eq!(parse_dns_server("10.8.0.1:5353").unwrap().port(), 5353);
        assert!(parse_dns_server("dns.example").is_err());
    }

    #[tokio::test]
    async fn tcp_banner_and_refused() {
        use tokio::io::AsyncWriteExt;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let _ = s.write_all(b"SSH-2.0-test\r\n").await;
            }
        });
        let t = Duration::from_secs(2);
        let ok = Runner::Tcp { addr: addr.clone(), banner: Some("SSH-".into()) }.run(t).await;
        assert!(ok.ok, "{ok:?}");
        assert_eq!(ok.detail, "SSH-2.0-test");
        let bad = Runner::Tcp { addr, banner: Some("HTTP".into()) }.run(t).await;
        assert!(!bad.ok);
        // port 1 on loopback: refused
        let refused = Runner::Tcp { addr: "127.0.0.1:1".into(), banner: None }.run(t).await;
        assert!(!refused.ok);
    }
}
