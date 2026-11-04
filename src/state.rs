//! Per-check history and the UP/DOWN state machine that decides when to alert.

use serde::Serialize;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// No verdict yet (just started, or failing but below the threshold from start).
    Unknown,
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub at: i64,
    pub ok: bool,
    pub latency_ms: u64,
    pub detail: String,
}

/// What a new sample changed, so the caller knows whether to notify.
#[derive(Debug, PartialEq, Eq)]
pub enum Transition {
    None,
    WentDown,
    Recovered,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckState {
    pub name: String,
    pub group: String,
    pub kind: &'static str,
    pub target: String,
    pub status: Status,
    /// Unix time of the last status change.
    pub since: Option<i64>,
    pub consecutive_failures: u32,
    pub threshold: u32,
    #[serde(skip)]
    pub capacity: usize,
    pub history: VecDeque<Sample>,
    /// (unix hour, ok samples, all samples) for the whole retention window.
    #[serde(skip)]
    pub hourly: VecDeque<(i64, u32, u32)>,
    #[serde(skip)]
    pub hourly_capacity: usize,
}

impl CheckState {
    #[cfg(test)]
    pub fn new(name: String, group: String, kind: &'static str, target: String, threshold: u32, capacity: usize) -> Self {
        Self::with_retention(name, group, kind, target, threshold, capacity, 7)
    }

    pub fn with_retention(
        name: String,
        group: String,
        kind: &'static str,
        target: String,
        threshold: u32,
        capacity: usize,
        retention_days: u32,
    ) -> Self {
        CheckState {
            name,
            group,
            kind,
            target,
            status: Status::Unknown,
            since: None,
            consecutive_failures: 0,
            threshold,
            capacity,
            history: VecDeque::with_capacity(capacity.min(4096)),
            hourly: VecDeque::new(),
            hourly_capacity: retention_days.max(1) as usize * 24,
        }
    }

    pub fn last(&self) -> Option<&Sample> {
        self.history.back()
    }

    /// p50 / p95 / p99 of the probe's own latency over the kept history (ms).
    pub fn percentiles(&self) -> Option<(u64, u64, u64)> {
        let mut v: Vec<u64> = self.history.iter().map(|s| s.latency_ms).collect();
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        let at = |q: f64| v[((q * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
        Some((at(0.50), at(0.95), at(0.99)))
    }

    /// Fraction of OK samples over the whole retention window (hourly counts).
    pub fn uptime_retention(&self) -> Option<f64> {
        let (ok, all) = self.hourly.iter().fold((0u64, 0u64), |(a, b), h| (a + h.1 as u64, b + h.2 as u64));
        if all == 0 { None } else { Some(ok as f64 / all as f64) }
    }

    /// Fraction of OK samples in the kept raw history (None if empty).
    pub fn uptime(&self) -> Option<f64> {
        if self.history.is_empty() {
            return None;
        }
        Some(self.history.iter().filter(|s| s.ok).count() as f64 / self.history.len() as f64)
    }

    pub fn record(&mut self, s: Sample) -> Transition {
        let at = s.at;
        let ok = s.ok;
        if self.history.len() >= self.capacity {
            self.history.pop_front();
        }
        self.history.push_back(s);
        let hour = at.div_euclid(3600);
        match self.hourly.back_mut() {
            Some(h) if h.0 == hour => {
                h.1 += ok as u32;
                h.2 += 1;
            }
            _ => {
                self.hourly.push_back((hour, ok as u32, 1));
                while self.hourly.len() > self.hourly_capacity {
                    self.hourly.pop_front();
                }
            }
        }

        if ok {
            self.consecutive_failures = 0;
            let was = self.status;
            if was != Status::Up {
                self.status = Status::Up;
                self.since = Some(at);
            }
            if was == Status::Down { Transition::Recovered } else { Transition::None }
        } else {
            self.consecutive_failures += 1;
            if self.consecutive_failures >= self.threshold && self.status != Status::Down {
                self.status = Status::Down;
                self.since = Some(at);
                Transition::WentDown
            } else {
                Transition::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(at: i64, ok: bool) -> Sample {
        Sample { at, ok, latency_ms: 1, detail: String::new() }
    }

    #[test]
    fn alerts_only_after_threshold_and_once() {
        let mut c = CheckState::new("a".into(), "".into(), "tcp", "x".into(), 3, 10);
        assert_eq!(c.record(s(1, true)), Transition::None);
        assert_eq!(c.status, Status::Up);
        assert_eq!(c.record(s(2, false)), Transition::None);
        assert_eq!(c.record(s(3, false)), Transition::None);
        assert_eq!(c.status, Status::Up, "two failures < threshold: still up");
        assert_eq!(c.record(s(4, false)), Transition::WentDown);
        assert_eq!(c.record(s(5, false)), Transition::None, "no repeat alert");
        assert_eq!(c.since, Some(4));
        assert_eq!(c.record(s(6, true)), Transition::Recovered);
        assert_eq!(c.record(s(7, true)), Transition::None);
    }

    #[test]
    fn flapping_below_threshold_never_alerts() {
        let mut c = CheckState::new("a".into(), "".into(), "tcp", "x".into(), 3, 10);
        for i in 0..20 {
            assert_eq!(c.record(s(i, i % 3 != 0)), Transition::None);
        }
    }

    #[test]
    fn latency_percentiles() {
        let mut c = CheckState::new("a".into(), "".into(), "tcp", "x".into(), 3, 200);
        assert_eq!(c.percentiles(), None);
        for i in 1..=100 {
            c.record(Sample { at: i, ok: true, latency_ms: i as u64, detail: String::new() });
        }
        assert_eq!(c.percentiles(), Some((50, 95, 99)));
    }

    #[test]
    fn down_from_start_alerts_and_history_is_bounded() {
        let mut c = CheckState::new("a".into(), "".into(), "tcp", "x".into(), 2, 5);
        assert_eq!(c.record(s(1, false)), Transition::None);
        assert_eq!(c.status, Status::Unknown);
        assert_eq!(c.record(s(2, false)), Transition::WentDown);
        for i in 3..20 {
            c.record(s(i, true));
        }
        assert_eq!(c.history.len(), 5);
        assert_eq!(c.uptime(), Some(1.0));
        // hourly summary keeps everything that fell out of the raw window
        assert_eq!(c.hourly.len(), 1);
        assert_eq!(c.hourly[0], (0, 17, 19));
    }
}
