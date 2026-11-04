//! pulse — a tiny health monitor.
//!
//!   pulse [--config /etc/pulse/pulse.toml]
//!   pulse --check-config FILE     validate and exit (non-zero on error)
//!   pulse --test-alert            send a test notification on every channel and exit

mod alert;
mod config;
mod probe;
mod state;
mod web;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

fn usage() -> ! {
    eprintln!("usage: pulse [--config FILE] [--test-alert] | --check-config FILE | --version");
    std::process::exit(2)
}

fn load_roots(files: &[String]) -> Result<Vec<reqwest::Certificate>, String> {
    let mut out = Vec::new();
    for f in files {
        let pem = std::fs::read(f).map_err(|e| format!("{f}: {e}"))?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| format!("{f}: {e}"))?;
        if certs.is_empty() {
            return Err(format!("{f}: no certificates found"));
        }
        out.extend(certs);
    }
    Ok(out)
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
    let roots = load_roots(&cfg.ca_files).unwrap_or_else(|e| {
        eprintln!("pulse: {e}");
        std::process::exit(1)
    });

    // Build every runner up front so config mistakes fail at start, not at 3 a.m.
    let mut runners = Vec::new();
    for c in &cfg.checks {
        let t = Duration::from_secs(c.timeout_secs.unwrap_or(cfg.timeout_secs));
        match probe::Runner::new(c, &roots, t) {
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
                Mutex::new(state::CheckState::new(
                    c.name.clone(),
                    c.group.clone(),
                    c.probe.kind(),
                    c.probe.target(),
                    c.failure_threshold.unwrap_or(cfg.failure_threshold),
                    cfg.history,
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

    for (i, (c, runner)) in cfg.checks.iter().zip(runners).enumerate() {
        let app = app.clone();
        let alerter = alerter.clone();
        let every = Duration::from_secs(c.interval_secs.unwrap_or(cfg.interval_secs));
        let t = Duration::from_secs(c.timeout_secs.unwrap_or(cfg.timeout_secs));
        let name = c.name.clone();
        tokio::spawn(async move {
            // Stagger starts so checks don't all fire in the same instant.
            tokio::time::sleep(Duration::from_millis(250 * i as u64 % 15_000)).await;
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let o = runner.run(t).await;
                let detail = o.detail.clone();
                let tr = app.checks[i].lock().unwrap().record(state::Sample {
                    at: web::now(),
                    ok: o.ok,
                    latency_ms: o.latency_ms,
                    detail: o.detail,
                });
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
