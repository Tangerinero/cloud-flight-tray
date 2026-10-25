use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_AGE_SECS: i64 = 24 * 60 * 60;
const MIN_WINDOW_SECS: i64 = 10 * 60;
const TARGET_WINDOW_SECS: i64 = 90 * 60;
const MAX_SAMPLES: usize = 500;
const RATE_EPSILON: f32 = 0.05;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Sample {
    t: i64,
    p: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DrainStats {
    pub percent_per_hour: Option<f32>,
    pub remaining_secs: Option<u64>,
    pub session_drop: Option<f32>,
    pub session_secs: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct HistoryFile {
    samples: Vec<Sample>,
}

pub struct DrainTracker {
    samples: Vec<Sample>,
    session_start: Option<Sample>,
    path: PathBuf,
}

impl DrainTracker {
    pub fn load() -> Self {
        let path = history_path();
        let samples = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<HistoryFile>(&raw).ok())
            .map(|h| h.samples)
            .unwrap_or_default();
        let mut tracker = Self {
            samples,
            session_start: None,
            path,
        };
        tracker.prune(now_secs());
        tracker
    }

    pub fn record(&mut self, percent: f32, charging: bool, connected: bool) -> DrainStats {
        let t = now_secs();
        if !connected || charging {
            self.prune(t);
            let current = self.samples.last().map(|s| s.p);
            return self.stats(current, t);
        }

        let sample = Sample { t, p: percent };
        if self.session_start.is_none() {
            self.session_start = Some(sample);
        }
        let should_push = self.samples.last().map_or(true, |s| {
            t > s.t && ((percent - s.p).abs() >= 0.01 || t - s.t >= 50)
        });
        if should_push {
            self.samples.push(sample);
        }
        self.prune(t);
        self.persist();
        self.stats(Some(percent), t)
    }

    pub fn latest_stats(&self) -> DrainStats {
        let now = now_secs();
        let current = self.samples.last().map(|s| s.p);
        self.stats(current, now)
    }

    fn prune(&mut self, now: i64) {
        let cutoff = now - MAX_AGE_SECS;
        self.samples.retain(|s| s.t >= cutoff);
        if self.samples.len() > MAX_SAMPLES {
            let extra = self.samples.len() - MAX_SAMPLES;
            self.samples.drain(..extra);
        }
    }

    fn persist(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let payload = HistoryFile {
            samples: self.samples.clone(),
        };
        if let Ok(json) = serde_json::to_string(&payload) {
            let _ = fs::write(&self.path, json);
        }
    }

    fn stats(&self, current: Option<f32>, now: i64) -> DrainStats {
        let mut stats = DrainStats::default();
        if let Some(start) = self.session_start {
            if let Some(pct) = current {
                stats.session_drop = Some((start.p - pct).max(0.0));
                stats.session_secs = Some((now - start.t).max(0) as u64);
            }
        }

        let window_start = now - TARGET_WINDOW_SECS;
        let window: Vec<_> = self
            .samples
            .iter()
            .copied()
            .filter(|s| s.t >= window_start)
            .collect();
        if window.len() < 2 {
            return stats;
        }
        let span = window.last().unwrap().t - window.first().unwrap().t;
        if span < MIN_WINDOW_SECS {
            return stats;
        }

        let rate = slope_percent_per_hour(&window);
        if rate > RATE_EPSILON {
            stats.percent_per_hour = Some(rate);
            if let Some(pct) = current {
                let hours = pct / rate;
                stats.remaining_secs = Some((hours * 3600.0).max(0.0) as u64);
            }
        }
        stats
    }
}

fn slope_percent_per_hour(samples: &[Sample]) -> f32 {
    let n = samples.len() as f32;
    let mut sum_t = 0.0;
    let mut sum_p = 0.0;
    let mut sum_tp = 0.0;
    let mut sum_tt = 0.0;
    let t0 = samples[0].t as f32;
    for s in samples {
        let t = (s.t as f32 - t0) / 3600.0;
        sum_t += t;
        sum_p += s.p;
        sum_tp += t * s.p;
        sum_tt += t * t;
    }
    let denom = n * sum_tt - sum_t * sum_t;
    if denom.abs() < f32::EPSILON {
        return 0.0;
    }
    let slope = (n * sum_tp - sum_t * sum_p) / denom;
    (-slope).max(0.0)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn history_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cloud-flight-tray").join("history.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slope_detects_drain() {
        let samples = [
            Sample { t: 0, p: 80.0 },
            Sample {
                t: 3600,
                p: 76.0,
            },
        ];
        let rate = slope_percent_per_hour(&samples);
        assert!((rate - 4.0).abs() < 0.01, "got {rate}");
    }
}
