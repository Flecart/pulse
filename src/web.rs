//! One server-rendered HTML page (no JavaScript), plus `/api/status` and `/health`.

use crate::state::{CheckState, Status};
use axum::{
    Json, Router,
    extract::State,
    http::header,
    response::{Html, IntoResponse},
    routing::get,
};
use std::{
    fmt::Write,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct App {
    pub title: String,
    pub checks: Vec<Mutex<CheckState>>,
}

pub type Shared = Arc<App>;

pub fn router(app: Shared) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(api))
        .route("/health", get(|| async { "ok" }))
        .with_state(app)
}

fn snapshot(app: &App) -> Vec<CheckState> {
    app.checks.iter().map(|c| c.lock().unwrap().clone()).collect()
}

async fn api(State(app): State<Shared>) -> impl IntoResponse {
    let checks = snapshot(&app);
    let down = checks.iter().filter(|c| c.status == Status::Down).count();
    Json(serde_json::json!({ "title": app.title, "down": down, "checks": checks }))
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

fn ago(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
    }
}

const BARS: usize = 60;

const CSS: &str = r#"
:root{--bg:#f6f7f9;--card:#fff;--fg:#1b1f24;--muted:#6b7280;--line:#e5e7eb;--up:#16a34a;--down:#dc2626;--warn:#d97706;--unk:#9ca3af}
@media (prefers-color-scheme:dark){:root{--bg:#0f1115;--card:#171a21;--fg:#e6e8eb;--muted:#8b93a1;--line:#262b35;--up:#22c55e;--down:#f87171;--warn:#fbbf24;--unk:#4b5563}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.45 system-ui,-apple-system,"Segoe UI",sans-serif}
main{max-width:960px;margin:0 auto;padding:24px 16px 48px}
header{display:flex;align-items:center;justify-content:space-between;gap:12px;flex-wrap:wrap;margin-bottom:20px}
h1{font-size:20px;margin:0;letter-spacing:-.01em}
.pill{padding:4px 12px;border-radius:999px;font-weight:600;font-size:13px;color:#fff}
.pill.ok{background:var(--up)}.pill.bad{background:var(--down)}
.meta{color:var(--muted);font-size:12px}
h2{font-size:12px;text-transform:uppercase;letter-spacing:.06em;color:var(--muted);margin:22px 0 8px}
.card{background:var(--card);border:1px solid var(--line);border-radius:10px;overflow:hidden}
.row{display:grid;grid-template-columns:14px minmax(0,1.3fr) minmax(0,1fr) 7.5rem;gap:4px 12px;align-items:center;padding:10px 14px;border-top:1px solid var(--line)}
.row:first-child{border-top:0}
.dot{width:10px;height:10px;border-radius:50%;background:var(--unk)}
.up .dot{background:var(--up)}.down .dot{background:var(--down)}.warn .dot{background:var(--warn)}
.name{font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.target,.detail{color:var(--muted);font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.down .detail{color:var(--down)}.warn .detail{color:var(--warn)}
.bars{display:flex;gap:2px;height:22px;align-items:stretch}
.bars i{flex:1;min-width:2px;max-width:6px;border-radius:2px;background:var(--line)}
.bars i.o{background:var(--up)}.bars i.f{background:var(--down)}
.nums{text-align:right;font-variant-numeric:tabular-nums;font-size:12px;color:var(--muted);white-space:nowrap}
.nums b{color:var(--fg);font-weight:600}
@media (max-width:640px){.row{grid-template-columns:14px minmax(0,1fr) auto}.bars{grid-column:2/4}}
"#;

async fn index(State(app): State<Shared>) -> impl IntoResponse {
    let checks = snapshot(&app);
    let t = now();
    let down = checks.iter().filter(|c| c.status == Status::Down).count();

    let mut h = String::with_capacity(16 * 1024);
    let _ = write!(
        h,
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta http-equiv="refresh" content="30"><title>{t_} — {state}</title><style>{CSS}</style></head><body><main><header><div><h1>{t_}</h1><div class="meta">{n} checks · refreshes every 30 s</div></div><span class="pill {cls}">{state}</span></header>"#,
        t_ = esc(&app.title),
        n = checks.len(),
        cls = if down == 0 { "ok" } else { "bad" },
        state = if down == 0 { "All systems up".to_string() } else { format!("{down} down") },
    );

    // Groups in first-seen order.
    let mut groups: Vec<&str> = Vec::new();
    for c in &checks {
        if !groups.contains(&c.group.as_str()) {
            groups.push(&c.group);
        }
    }
    for g in groups {
        let _ = write!(h, "<h2>{}</h2><div class=\"card\">", esc(if g.is_empty() { "Checks" } else { g }));
        for c in checks.iter().filter(|c| c.group == g) {
            let last = c.last();
            let failing = c.consecutive_failures > 0;
            let cls = match (c.status, failing) {
                (Status::Down, _) => "down",
                (_, true) => "warn",
                (Status::Up, false) => "up",
                (Status::Unknown, false) => "unk",
            };
            let detail = match (c.status, last) {
                (_, None) => "waiting for first run".to_string(),
                (Status::Down, Some(l)) => format!("down {} · {}", ago(t - c.since.unwrap_or(t)), l.detail),
                (_, Some(l)) if failing => format!("failing {}/{} · {}", c.consecutive_failures, c.threshold, l.detail),
                (_, Some(l)) => format!("up {} · {}", ago(t - c.since.unwrap_or(t)), l.detail),
            };
            let _ = write!(
                h,
                r#"<div class="row {cls}"><span class="dot"></span><div style="min-width:0"><div class="name">{name}</div><div class="target">{kind} · {target}</div></div><div class="bars" title="last {BARS} runs">"#,
                name = esc(&c.name),
                kind = c.kind,
                target = esc(&c.target),
            );
            let start = c.history.len().saturating_sub(BARS);
            for _ in 0..BARS.saturating_sub(c.history.len()) {
                h.push_str("<i></i>");
            }
            for s in c.history.iter().skip(start) {
                let _ = write!(
                    h,
                    r#"<i class="{}" title="{} ago · {} ms · {}"></i>"#,
                    if s.ok { "o" } else { "f" },
                    ago(t - s.at),
                    s.latency_ms,
                    esc(&s.detail)
                );
            }
            let up = c.uptime().map(|u| format!("{:.1}%", u * 100.0)).unwrap_or_else(|| "–".into());
            let lat = last.map(|l| format!("{} ms", l.latency_ms)).unwrap_or_else(|| "–".into());
            let _ = write!(
                h,
                r#"</div><div class="nums"><b>{up}</b> · {lat}</div><span></span><div class="detail" style="grid-column:2/5">{d}</div></div>"#,
                d = esc(&detail)
            );
        }
        h.push_str("</div>");
    }
    if checks.is_empty() {
        h.push_str(r#"<p class="meta">No checks configured.</p>"#);
    }
    h.push_str("</main></body></html>");
    ([(header::CACHE_CONTROL, "no-store")], Html(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_formats() {
        assert_eq!(esc(r#"<a href="x">&'"#), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
        assert_eq!(ago(5), "5s");
        assert_eq!(ago(125), "2m");
        assert_eq!(ago(3720), "1h 2m");
        assert_eq!(ago(90000), "1d 1h");
    }
}
