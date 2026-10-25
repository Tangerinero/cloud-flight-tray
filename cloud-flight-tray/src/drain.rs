use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_AGE_SECS: i64 = 24 * 60 * 60;
const FIT_WINDOW_SECS: i64 = 8 * 60 * 60;
const MIN_WINDOW_SECS: i64 = 10 * 60;
/// Cloud Flight pack: 1500 mAh, rated about 30 h, so 3.33%/h until real drain shows.
const NOMINAL_PERCENT_PER_HOUR: f32 = 100.0 / 30.0;
/// Net drop required before the measured rate fully replaces the 30 h rating.
const FULL_TRUST_DROP: f32 = 2.0;
/// A jump larger than sensor noise starts a new discharge segment.
const RECHARGE_JUMP: f32 = 4.0;
/// Wall-clock silence longer than this is the headset or PC being off, not drain.
const MAX_IDLE_GAP_SECS: i64 = 15 * 60;
/// Per new sample. 0.02 keeps prior sessions for hours instead of washing them out in minutes.
const LEARN_ALPHA: f32 = 0.02;
const MAX_SAMPLES: usize = 4_000;

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
    pub estimated_health_pct: Option<f32>,
}

#[derive(Default, Serialize, Deserialize)]
struct HistoryFile {
    samples: Vec<Sample>,
    #[serde(default)]
    learned_nominal_rate: Option<f32>,
    #[serde(default)]
    was_charging: bool,
}

pub struct DrainTracker {
    samples: Vec<Sample>,
    session_start: Option<Sample>,
    learned_nominal_rate: Option<f32>,
    was_charging: bool,
    path: PathBuf,
}

impl DrainTracker {
    pub fn load() -> Self {
        let path = history_path();
        let loaded = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<HistoryFile>(&raw).ok())
            .unwrap_or_default();
        let mut tracker = Self {
            samples: loaded.samples,
            session_start: None,
            learned_nominal_rate: loaded.learned_nominal_rate,
            was_charging: loaded.was_charging,
            path,
        };
        tracker.prune(now_secs());
        tracker
    }

    pub fn record(&mut self, percent: f32, charging: bool, connected: bool) -> DrainStats {
        let t = now_secs();
        if !connected || charging {
            if charging {
                self.was_charging = true;
                self.session_start = None;
            }
            self.prune(t);
            self.persist();
            let current = self.samples.last().map(|s| s.p);
            return self.stats(current, t);
        }

        if self.was_charging {
            self.samples.clear();
            self.was_charging = false;
            self.session_start = None;
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
            self.maybe_learn(t);
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
            learned_nominal_rate: self.learned_nominal_rate,
            was_charging: self.was_charging,
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

        let window_start = now - FIT_WINDOW_SECS;
        let window: Vec<_> = self
            .samples
            .iter()
            .copied()
            .filter(|s| s.t >= window_start)
            .collect();
        let segment = discharge_segment(&window);
        let measured = net_rate(segment);
        let drop = net_drop(segment);
        let nominal = self
            .learned_nominal_rate
            .unwrap_or(NOMINAL_PERCENT_PER_HOUR);
        let rate = blend_rate(measured, drop, nominal);
        if let Some(pct) = current {
            if rate > 0.05 {
                stats.percent_per_hour = Some(rate);
                let hours = pct / rate;
                stats.remaining_secs = Some((hours * 3600.0).max(0.0) as u64);
            }
        }
        stats.estimated_health_pct = health_rate(measured, drop, self.learned_nominal_rate)
            .map(health_pct);
        stats
    }

    fn maybe_learn(&mut self, now: i64) {
        let window_start = now - FIT_WINDOW_SECS;
        let window: Vec<_> = self
            .samples
            .iter()
            .copied()
            .filter(|s| s.t >= window_start)
            .collect();
        let segment = discharge_segment(&window);
        let Some(measured) = net_rate(segment) else {
            return;
        };
        if net_drop(segment) < FULL_TRUST_DROP || measured <= 0.5 {
            return;
        }
        self.learned_nominal_rate = Some(learn_rate(self.learned_nominal_rate, measured));
    }
}

fn discharge_segment(samples: &[Sample]) -> &[Sample] {
    let mut start = 0;
    for i in 1..samples.len() {
        let jumped = samples[i].p - samples[i - 1].p > RECHARGE_JUMP;
        let idle = samples[i].t - samples[i - 1].t > MAX_IDLE_GAP_SECS;
        if jumped || idle {
            start = i;
        }
    }
    &samples[start..]
}

fn net_drop(samples: &[Sample]) -> f32 {
    let Some((start, end)) = end_medians(samples) else {
        return 0.0;
    };
    (start - end).max(0.0)
}

fn net_rate(samples: &[Sample]) -> Option<f32> {
    let (start, end) = end_medians(samples)?;
    let span = samples.last()?.t - samples.first()?.t;
    if span < MIN_WINDOW_SECS {
        return None;
    }
    let hours = span as f32 / 3600.0;
    Some(((start - end) / hours).max(0.0))
}

fn end_medians(samples: &[Sample]) -> Option<(f32, f32)> {
    if samples.len() < 2 {
        return None;
    }
    let k = 5.min(samples.len() / 2).max(1);
    if k * 2 > samples.len() {
        return None;
    }
    Some((
        median_percent(&samples[..k]),
        median_percent(&samples[samples.len() - k..]),
    ))
}

fn median_percent(samples: &[Sample]) -> f32 {
    let mut vals: Vec<f32> = samples.iter().map(|s| s.p).collect();
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = vals.len() / 2;
    if vals.len() % 2 == 0 {
        (vals[mid - 1] + vals[mid]) / 2.0
    } else {
        vals[mid]
    }
}

/// Trust the measured session rate once the level has fallen enough to beat sensor noise.
/// Until then, lean on the 30 h / 1500 mAh rating (3.33%/h).
fn blend_rate(measured: Option<f32>, drop: f32, nominal: f32) -> f32 {
    match measured {
        Some(measured) if measured > 0.05 && drop >= FULL_TRUST_DROP => measured,
        Some(measured) if measured > 0.05 => {
            let weight = (drop / FULL_TRUST_DROP).clamp(0.0, 1.0);
            weight * measured + (1.0 - weight) * nominal
        }
        _ => nominal,
    }
}

fn learn_rate(old: Option<f32>, measured: f32) -> f32 {
    let next = match old {
        Some(old) => (1.0 - LEARN_ALPHA) * old + LEARN_ALPHA * measured,
        None => measured,
    };
    next.clamp(2.5, 40.0)
}

fn health_rate(measured: Option<f32>, drop: f32, learned: Option<f32>) -> Option<f32> {
    if drop >= FULL_TRUST_DROP && measured.is_some_and(|rate| rate > 0.5) {
        return measured;
    }
    learned
}

fn health_pct(active_rate: f32) -> f32 {
    (NOMINAL_PERCENT_PER_HOUR / active_rate * 100.0).clamp(5.0, 100.0)
}

#[cfg(test)]
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
            Sample { t: 3600, p: 76.0 },
        ];
        let rate = slope_percent_per_hour(&samples);
        assert!((rate - 4.0).abs() < 0.01, "got {rate}");
    }

    #[test]
    fn flat_full_charge_uses_thirty_hour_rating() {
        let samples: Vec<_> = (0..12)
            .map(|i| Sample {
                t: i * 60,
                p: 100.0,
            })
            .collect();
        assert!(net_rate(&samples).is_none() || net_rate(&samples) == Some(0.0));
        let rate = blend_rate(
            net_rate(&samples),
            net_drop(&samples),
            NOMINAL_PERCENT_PER_HOUR,
        );
        assert!((rate - NOMINAL_PERCENT_PER_HOUR).abs() < 0.01, "got {rate}");
        let hours = 97.0 / rate;
        assert!((hours - 29.1).abs() < 0.2, "got {hours}");
    }

    #[test]
    fn noisy_session_matches_net_drop_not_shallow_fit() {
        let mut samples = Vec::new();
        for i in 0..20 {
            samples.push(Sample {
                t: i * 60,
                p: 100.0,
            });
        }
        let noisy = [
            97.5, 100.0, 97.5, 97.0, 99.0, 94.75, 97.5, 95.0, 94.25, 93.5, 92.0, 90.5, 89.5,
            88.5, 87.5, 86.5, 85.7,
        ];
        for (i, p) in noisy.iter().enumerate() {
            samples.push(Sample {
                t: 20 * 60 + i as i64 * 600,
                p: *p,
            });
        }
        let measured = net_rate(&samples).unwrap();
        let drop = net_drop(&samples);
        let rate = blend_rate(Some(measured), drop, NOMINAL_PERCENT_PER_HOUR);
        assert!(drop > 10.0, "drop {drop}");
        assert!(
            (rate - measured).abs() < 0.01,
            "expected full trust, rate {rate} measured {measured}"
        );
        assert!(
            measured > 3.0 && measured < 8.0,
            "net rate {measured} should track the real drop, not ~2%/h"
        );
        let remaining_h = 85.7 / rate;
        assert!(
            remaining_h < 30.0 && remaining_h > 10.0,
            "remaining {remaining_h}"
        );
    }

    #[test]
    fn learned_rate_and_health() {
        assert!((learn_rate(None, 8.5) - 8.5).abs() < 0.01);
        let blended = learn_rate(Some(NOMINAL_PERCENT_PER_HOUR), 8.5);
        let expected = (1.0 - LEARN_ALPHA) * NOMINAL_PERCENT_PER_HOUR + LEARN_ALPHA * 8.5;
        assert!((blended - expected).abs() < 0.01, "got {blended}");
        let mut slow = 4.0;
        for _ in 0..10 {
            slow = learn_rate(Some(slow), 10.0);
        }
        assert!(
            slow < 6.0,
            "ten samples must not erase history, got {slow}"
        );
        assert_eq!(learn_rate(None, 80.0), 40.0);
        let health = health_pct(8.5);
        assert!((health - 39.2).abs() < 0.2, "got {health}");
        assert!(health_rate(Some(8.5), 3.0, None).is_some());
        assert!(health_rate(Some(8.5), 1.0, None).is_none());
    }

    #[test]
    fn charge_gap_starts_a_new_discharge() {
        let path = std::env::temp_dir().join(format!(
            "cloud-flight-tray-test-{}-{}.json",
            std::process::id(),
            now_secs()
        ));
        let mut tracker = DrainTracker {
            samples: vec![
                Sample { t: 0, p: 80.0 },
                Sample { t: 600, p: 78.0 },
            ],
            session_start: Some(Sample { t: 0, p: 80.0 }),
            learned_nominal_rate: Some(4.0),
            was_charging: false,
            path: path.clone(),
        };
        let learned_before = tracker.learned_nominal_rate;
        tracker.record(78.0, true, true);
        assert!(tracker.was_charging);
        assert!(tracker.session_start.is_none());
        assert_eq!(tracker.learned_nominal_rate, learned_before);

        tracker.record(77.2, false, true);
        assert!(!tracker.was_charging);
        assert_eq!(tracker.samples.len(), 1);
        assert!((tracker.samples[0].p - 77.2).abs() < 0.01);
        assert!(tracker.session_start.is_some());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn duplicate_poll_does_not_learn_again() {
        let path = std::env::temp_dir().join(format!(
            "cloud-flight-tray-test-dup-{}-{}.json",
            std::process::id(),
            now_secs()
        ));
        let now = now_secs();
        let mut tracker = DrainTracker {
            samples: vec![
                Sample {
                    t: now - 3600,
                    p: 90.0,
                },
                Sample {
                    t: now - 60,
                    p: 86.0,
                },
            ],
            session_start: None,
            learned_nominal_rate: Some(4.0),
            was_charging: false,
            path: path.clone(),
        };
        tracker.record(86.0, false, true);
        let once = tracker.learned_nominal_rate;
        tracker.record(86.0, false, true);
        assert_eq!(tracker.learned_nominal_rate, once);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn idle_gap_starts_a_fresh_segment() {
        let gap = MAX_IDLE_GAP_SECS + 1;
        let samples = vec![
            Sample { t: 0, p: 80.0 },
            Sample { t: 3600, p: 72.0 },
            Sample { t: 3600 + gap, p: 71.5 },
            Sample {
                t: 3600 + gap + 600,
                p: 70.0,
            },
        ];
        let segment = discharge_segment(&samples);
        assert_eq!(segment.len(), 2);
        assert_eq!(segment[0].t, 3600 + gap);
        let rate = net_rate(segment).unwrap();
        assert!((rate - 9.0).abs() < 0.01, "got {rate}");

        let continuous = vec![
            Sample { t: 0, p: 80.0 },
            Sample {
                t: MAX_IDLE_GAP_SECS,
                p: 78.0,
            },
        ];
        assert_eq!(discharge_segment(&continuous).len(), 2);
    }
}
