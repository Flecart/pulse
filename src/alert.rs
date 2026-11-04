//! Notifications (ntfy push and/or email via Resend) and the outbound heartbeat.

use crate::config::{Alerts, Email};
use std::time::Duration;

#[derive(Clone)]
struct Mail {
    from: String,
    to: Vec<String>,
    api_key: String,
    api_url: String,
}

#[derive(Clone)]
pub struct Alerter {
    client: reqwest::Client,
    ntfy_url: Option<String>,
    ntfy_token: Option<String>,
    mail: Option<Mail>,
    title: String,
}

/// Expand a leading `$CREDENTIALS_DIRECTORY` (systemd credentials) and read the key.
fn read_key(path: &str) -> Result<String, String> {
    let p = match path.strip_prefix("$CREDENTIALS_DIRECTORY") {
        Some(rest) => {
            let dir = std::env::var("CREDENTIALS_DIRECTORY")
                .map_err(|_| format!("{path}: $CREDENTIALS_DIRECTORY is not set (not started by systemd?)"))?;
            format!("{dir}{rest}")
        }
        None => path.to_string(),
    };
    let key = std::fs::read_to_string(&p).map_err(|e| format!("{p}: {e}"))?.trim().to_string();
    if key.is_empty() {
        return Err(format!("{p}: empty API key"));
    }
    Ok(key)
}

impl Alerter {
    pub fn new(cfg: &Alerts, title: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("pulse/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        let mail = match &cfg.email {
            Some(Email { from, to, api_key_file, api_url }) => Some(Mail {
                from: from.clone(),
                to: to.clone(),
                api_key: read_key(api_key_file)?,
                api_url: api_url.clone(),
            }),
            None => None,
        };
        Ok(Alerter { client, ntfy_url: cfg.ntfy_url.clone(), ntfy_token: cfg.ntfy_token.clone(), mail, title: title.into() })
    }

    pub fn channels(&self) -> String {
        let mut v = Vec::new();
        if self.ntfy_url.is_some() {
            v.push("ntfy");
        }
        if self.mail.is_some() {
            v.push("email");
        }
        if v.is_empty() { "off".into() } else { v.join(" + ") }
    }

    /// Fire-and-forget: a slow or broken channel never stalls the checks.
    pub fn send(&self, down: bool, check: &str, detail: &str) {
        let me = self.clone();
        let (check, detail) = (check.to_string(), detail.to_string());
        tokio::spawn(async move {
            for e in me.deliver(down, &check, &detail).await {
                eprintln!("pulse: alert failed: {e}");
            }
        });
    }

    /// Send on every configured channel; returns the errors (empty = all delivered).
    pub async fn deliver(&self, down: bool, check: &str, detail: &str) -> Vec<String> {
        let mut errors = Vec::new();
        let subject = if down {
            format!("{}: {check} is DOWN", self.title)
        } else {
            format!("{}: {check} recovered", self.title)
        };

        if let Some(url) = &self.ntfy_url {
            let mut req = self
                .client
                .post(url)
                .header("Title", subject.as_str())
                .header("Priority", if down { "high" } else { "default" })
                .header("Tags", if down { "rotating_light" } else { "white_check_mark" })
                .body(detail.to_string());
            if let Some(t) = &self.ntfy_token {
                req = req.bearer_auth(t);
            }
            match req.send().await {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => errors.push(format!("ntfy: HTTP {}", r.status())),
                Err(e) => errors.push(format!("ntfy: {e}")),
            }
        }

        if let Some(m) = &self.mail {
            let text = format!(
                "{subject}\n\n{detail}\n\nState changes are sent once; see the status page for history.\n-- pulse"
            );
            let body = serde_json::json!({ "from": m.from, "to": m.to, "subject": subject, "text": text });
            match self.client.post(&m.api_url).bearer_auth(&m.api_key).json(&body).send().await {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => {
                    let st = r.status();
                    let msg = r.text().await.unwrap_or_default();
                    errors.push(format!("email: HTTP {st}: {}", msg.chars().take(300).collect::<String>()));
                }
                Err(e) => errors.push(format!("email: {e}")),
            }
        }
        errors
    }
}

/// Ping `url` every `every` while pulse is alive.
pub async fn heartbeat(url: String, every: Duration) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("pulse/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client");
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        if let Err(e) = client.get(&url).send().await {
            eprintln!("pulse: heartbeat failed: {e}");
        }
    }
}
