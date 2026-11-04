//! pulse — a tiny health monitor.
//!
//!   pulse [--config /etc/pulse/pulse.toml]
//!   pulse --check-config FILE     validate and exit (non-zero on error)
//!   pulse --test-alert            send a test notification on every channel and exit

mod alert;
mod config;
mod metrics;
mod probe;
mod state;
mod store;
mod web;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

fn usage() -> ! {
    eprintln!("usage: pulse [--config FILE] [--test-alert] | --check-config FILE | --list-metrics | --version");
    std::process::exit(2)
}

/// TLS config for HTTP checks: public roots + `ca_files`, and NO session resumption.
/// A resumed TLS session skips the certificate, so a monitor would keep seeing (and
/// counting down) the first certificate it saw, never the renewed one, and would never
/// notice a real expiry. Every check must do a full handshake.
fn tls_config(files: &[String]) -> Result<Arc<rustls::ClientConfig>, String> {
    use rustls_pki_types::{CertificateDer, pem::PemObject};
    let mut roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    for f in files {
        let pem = std::fs::read(f).map_err(|e| format!("{f}: {e}"))?;
        let mut n = 0;
        for c in CertificateDer::pem_slice_iter(&pem) {
            let c = c.map_err(|e| format!("{f}: {e}"))?;
            roots.add(c).map_err(|e| format!("{f}: {e}"))?;
            n += 1;
        }
        if n == 0 {
            return Err(format!("{f}: no certificates found"));
        }
    }
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.resumption = rustls::client::Resumption::disabled();
    Ok(Arc::new(cfg))
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let mut path = PathBuf::from(std::env::var("PULSE_CONFIG").unwrap_or_else(|_| "/etc/pulse/pulse.toml".into()));
    let mut check_only = false;
    let mut test_alert = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" | "-c" => path = args.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--check-config" => {
                path = args.next().map(PathBuf::from).unwrap_or_else(|| usage());
                check_only = true;
            }
            "--test-alert" => test_alert = true,
            "--list-metrics" => {
                for m in metrics::names() {
                    println!("{m}");
                }
                return;
            }
            "--version" | "-V" => {
                println!("pulse {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => usage(),
        }
    }

    let cfg = match config::Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pulse: {e}");
            std::process::exit(1);
        }
    };
    let tls = tls_config(&cfg.ca_files).unwrap_or_else(|e| {
        eprintln!("pulse: {e}");
        std::process::exit(1)
    });

    // Build every runner up front so config mistakes fail at start, not at 3 a.m.
    let mut runners = Vec::new();
    for c in &cfg.checks {
        let t = Duration::from_secs(c.timeout_secs.unwrap_or(cfg.timeout_secs));
        match probe::Runner::new(c, &tls, t) {
            Ok(r) => runners.push(r),
            Err(e) => {
                eprintln!("pulse: check {:?}: {e}", c.name);
                std::process::exit(1);
            }
        }
    }
    if check_only {
        println!("{}: OK ({} checks)", path.display(), cfg.checks.len());
        return;
    }

    let app = Arc::new(web::App {
        title: cfg.title.clone(),
        checks: cfg
            .checks
            .iter()
            .map(|c| {
                Mutex::new(state::CheckState::with_retention(
                    c.name.clone(),
                    c.group.clone(),
                    c.probe.kind(),
                    c.probe.target(),
                    c.failure_threshold.unwrap_or(cfg.failure_threshold),
                    cfg.history,
                    cfg.retention_days,
                ))
            })
            .collect(),
    });
    let alerter = alert::Alerter::new(&cfg.alerts, &cfg.title).unwrap_or_else(|e| {
        eprintln!("pulse: alerts: {e}");
        std::process::exit(1)
    });
    if test_alert {
        let errs = alerter.deliver(true, "test alert", "If you can read this, pulse alerts work.").await;
        for e in &errs {
            eprintln!("pulse: {e}");
        }
        println!("test alert via {}: {}", alerter.channels(), if errs.is_empty() { "sent" } else { "FAILED" });
        std::process::exit(if errs.is_empty() { 0 } else { 1 });
    }

    // Persistent history: replay the kept days through the state machine (no alerts),
    // so uptime and UP/DOWN state survive restarts.
    let store = match &cfg.data_dir {
        Some(dir) => match store::Store::open(dir, cfg.retention_days) {
            Ok(st) => {
                let samples = st.load(web::now());
                let n = samples.len();
                for (name, s) in samples {
                    if let Some(i) = cfg.checks.iter().position(|c| c.name == name) {
                        app.checks[i].lock().unwrap().record(s);
                    }
                }
                eprintln!("pulse: loaded {n} samples from {dir} (keeping {} days)", cfg.retention_days);
                Some(Arc::new(st))
            }
            Err(e) => {
                eprintln!("pulse: history disabled: {e}");
                None
            }
        },
        None => None,
    };

    for (i, (c, runner)) in cfg.checks.iter().zip(runners).enumerate() {
        let app = app.clone();
        let store = store.clone();
        let alerter = alerter.clone();
        let every = Duration::from_secs(c.interval_secs.unwrap_or(cfg.interval_secs));
        let t = Duration::from_secs(c.timeout_secs.unwrap_or(cfg.timeout_secs));
        let name = c.name.clone();
        let max_latency = c.max_latency_ms;
        tokio::spawn(async move {
            // Stagger starts so checks don't all fire in the same instant.
            tokio::time::sleep(Duration::from_millis(250 * i as u64 % 15_000)).await;
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let mut o = runner.run(t).await;
                if let Some(max) = max_latency {
                    if o.ok && o.latency_ms > max {
                        o.ok = false;
                        o.detail = format!("slow: {} ms > {max} ms ({})", o.latency_ms, o.detail);
                    }
                }
                let detail = o.detail.clone();
                let sample = state::Sample { at: web::now(), ok: o.ok, latency_ms: o.latency_ms, detail: o.detail };
                if let Some(st) = &store {
                    st.append(&name, &sample);
                }
                let tr = app.checks[i].lock().unwrap().record(sample);
                match tr {
                    state::Transition::WentDown => {
                        eprintln!("pulse: DOWN {name}: {detail}");
                        alerter.send(true, &name, &detail);
                    }
                    state::Transition::Recovered => {
                        eprintln!("pulse: UP {name}: {detail}");
                        alerter.send(false, &name, &detail);
                    }
                    state::Transition::None => {}
                }
            }
        });
    }

    if let Some(url) = cfg.alerts.heartbeat_url.clone() {
        tokio::spawn(alert::heartbeat(url, Duration::from_secs(cfg.alerts.heartbeat_secs.max(10))));
    }

    let listener = match tokio::net::TcpListener::bind(&cfg.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("pulse: cannot listen on {}: {e}", cfg.listen);
            std::process::exit(1);
        }
    };
    eprintln!(
        "pulse {}: {} checks, status page on http://{}, alerts {}",
        env!("CARGO_PKG_VERSION"),
        cfg.checks.len(),
        cfg.listen,
        alerter.channels()
    );

    // systemd: Type=notify readiness + watchdog keep-alive (no-ops outside systemd).
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);
    let mut usec = 0u64;
    if sd_notify::watchdog_enabled(false, &mut usec) {
        let half = Duration::from_micros(usec / 2);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(half).await;
                let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
            }
        });
    }

    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    };
    if let Err(e) = axum::serve(listener, web::router(app)).with_graceful_shutdown(shutdown).await {
        eprintln!("pulse: server error: {e}");
        std::process::exit(1);
    }
}
