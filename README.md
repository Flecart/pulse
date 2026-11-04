# pulse

A tiny self-hosted health monitor. It is one ~3 MB binary that idles at a few MB of RAM, configured
with one TOML file. Each check runs on its own timer, and you get one status page plus alerts when a
check's state changes.

- **Checks:**
  - `http`: status code, body substring, TLS certificate lifetime, optional fixed IP;
  - `tcp`: connect, optional banner match (e.g. `SSH-`);
  - `dns`: query a specific server, optional expected IP;
  - `systemd`: the unit is active and hasn't restarted since the previous run, which catches
    crash-loops;
  - `metric`: machine health in the USE style (CPU busy/iowait/steal, load per core, available
    memory, swap activity, OOM kills, disk and inode free space, read-only remounts, disk latency
    and queue depth, network drops and errors, TCP retransmits and connections, file-descriptor
    use, GPU utilization/memory/temperature/throttling). `pulse --list-metrics` lists them;
  - `caddy`: per-site RED metrics from Caddy's Prometheus endpoint: request rate, 5xx %,
    p50/p95/p99 latency, and an optional minimum traffic rate.
- **Latency percentiles:** every check shows the p50/p95/p99 of its own run time, and
  `max_latency_ms` turns slow runs into failures.
- **No alert spam:** a check goes DOWN only after `failure_threshold` consecutive failures. You get
  one alert when it goes down and one when it recovers.
- **Alerts:**
  - [ntfy](https://ntfy.sh) push;
  - email through [Resend](https://resend.com);
  - an outbound **heartbeat** ping, e.g. to healthchecks.io, so you also hear about it when the
    whole machine is gone.
- **Status page:** server-rendered HTML, no JavaScript, light and dark themes, the last 60 runs as
  bars, plus uptime and latency. JSON is at `/api/status` and liveness at `/health`.
- **systemd-native:** `Type=notify` with watchdog support, and the API key can come from
  `LoadCredentialEncrypted=`.

History lives in memory only, about 24 h per check by default, and is lost on restart.

## Use

```bash
cargo build --release
./target/release/pulse --check-config pulse.example.toml   # validate
./target/release/pulse -c pulse.toml                       # run (default: /etc/pulse/pulse.toml)
./target/release/pulse -c pulse.toml --test-alert          # send a test alert on every channel
```

See [`pulse.example.toml`](pulse.example.toml) for every option.

### systemd

```ini
[Service]
Type=notify
WatchdogSec=60
ExecStart=/usr/local/bin/pulse -c /etc/pulse/pulse.toml
LoadCredentialEncrypted=resend:/etc/pulse/resend.cred   # optional, for email
DynamicUser=yes
ProtectSystem=strict
NoNewPrivileges=yes
```

Store the Resend key encrypted with:
`systemd-creds encrypt --name=resend - /etc/pulse/resend.cred` (paste the key, then Ctrl-D).

## Notes

- Within `[alerts.email]`, `from = "… <onboarding@resend.dev>"` works without a verified domain,
  but it only delivers to your Resend account's own address.
- `http` checks never follow redirects and never reuse connections, so each run does a full
  connect and TLS handshake.
- `systemd` checks need read access to the system D-Bus, which every user has by default.

MIT licensed.
