//! Rolling spread statistics per instrument — the "good arb coin" signature:
//! a **tight baseline** (spread hovers near 0–1%) punctuated by **brief spikes**,
//! never a persistently wide gap (which means a structural break, not an edge).
//!
//! History is recorded from a single representative spread series per instrument
//! (computed by the drain loop with default fees), so the statistics describe
//! the *coin's behavior*, shared across all clients.

use dashmap::DashMap;
use domain::{Decimal, Instrument};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct DynamicsConfig {
    /// How long to retain samples.
    pub window: Duration,
    /// Minimum spacing between recorded samples per instrument (throttle).
    pub min_sample_interval: Duration,
    /// Reference threshold defining an "above-baseline episode" (for duration).
    pub reference_threshold: Decimal,
}

impl Default for DynamicsConfig {
    fn default() -> Self {
        DynamicsConfig {
            window: Duration::from_secs(300),
            min_sample_interval: Duration::from_millis(500),
            reference_threshold: Decimal::new(2, 2), // 0.02
        }
    }
}

/// Computed statistics over the retained window for one instrument.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpreadStats {
    pub baseline_pct: Decimal, // median net spread
    pub stddev_pct: Decimal,
    pub current_pct: Decimal,
    /// (current - mean) / stddev, if stddev > 0.
    pub z_score: Option<Decimal>,
    pub sample_count: usize,
    /// How long the spread has continuously exceeded `reference_threshold`.
    pub episode_ms: u64,
}

struct Series {
    samples: VecDeque<(Instant, Decimal)>,
    episode_open: Option<Instant>,
    last_recorded: Option<Instant>,
}

impl Series {
    fn new() -> Self {
        Series {
            samples: VecDeque::new(),
            episode_open: None,
            last_recorded: None,
        }
    }
}

/// Per-instrument rolling spread history.
pub struct SpreadHistory {
    cfg: DynamicsConfig,
    series: DashMap<Instrument, Series>,
}

impl SpreadHistory {
    pub fn new(cfg: DynamicsConfig) -> Self {
        SpreadHistory {
            cfg,
            series: DashMap::new(),
        }
    }

    /// Record a representative net spread for `instrument`. Throttled by
    /// `min_sample_interval`; updates the above-threshold episode clock.
    pub fn record(&self, instrument: &Instrument, net: Decimal, now: Instant) {
        let mut s = self.series.entry(instrument.clone()).or_insert_with(Series::new);

        if let Some(last) = s.last_recorded {
            if now.saturating_duration_since(last) < self.cfg.min_sample_interval {
                // Still track the episode clock even when not sampling.
                update_episode(&mut s, net, now, self.cfg.reference_threshold);
                return;
            }
        }
        s.last_recorded = Some(now);
        s.samples.push_back((now, net));
        // Trim outside the window.
        let cutoff = self.cfg.window;
        while let Some(&(t, _)) = s.samples.front() {
            if now.saturating_duration_since(t) > cutoff {
                s.samples.pop_front();
            } else {
                break;
            }
        }
        update_episode(&mut s, net, now, self.cfg.reference_threshold);
    }

    /// Compute stats for `instrument`, or `None` if no samples yet.
    pub fn stats(&self, instrument: &Instrument, now: Instant) -> Option<SpreadStats> {
        let s = self.series.get(instrument)?;
        if s.samples.is_empty() {
            return None;
        }
        let mut vals: Vec<f64> = s
            .samples
            .iter()
            .filter_map(|(_, d)| d.to_f64())
            .collect();
        if vals.is_empty() {
            return None;
        }
        let n = vals.len();
        let current = *vals.last().unwrap();

        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if n % 2 == 1 {
            vals[n / 2]
        } else {
            (vals[n / 2 - 1] + vals[n / 2]) / 2.0
        };
        let mean = vals.iter().sum::<f64>() / n as f64;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
        let stddev = variance.sqrt();
        let z = if stddev > 0.0 {
            Decimal::from_f64((current - mean) / stddev)
        } else {
            None
        };

        let episode_ms = s
            .episode_open
            .map(|t| now.saturating_duration_since(t).as_millis() as u64)
            .unwrap_or(0);

        Some(SpreadStats {
            baseline_pct: Decimal::from_f64(median).unwrap_or_default(),
            stddev_pct: Decimal::from_f64(stddev).unwrap_or_default(),
            current_pct: Decimal::from_f64(current).unwrap_or_default(),
            z_score: z,
            sample_count: n,
            episode_ms,
        })
    }
}

fn update_episode(s: &mut Series, net: Decimal, now: Instant, threshold: Decimal) {
    if net > threshold {
        if s.episode_open.is_none() {
            s.episode_open = Some(now);
        }
    } else {
        s.episode_open = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn cfg() -> DynamicsConfig {
        DynamicsConfig {
            window: Duration::from_secs(300),
            min_sample_interval: Duration::from_millis(0),
            reference_threshold: dec!(0.02),
        }
    }

    #[test]
    fn baseline_and_spike_detected() {
        let h = SpreadHistory::new(cfg());
        let inst = Instrument::perp("XYZ", "USDT");
        let t0 = Instant::now();
        // Tight baseline near 0.3%, then a spike to 5%.
        for i in 0..20 {
            h.record(&inst, dec!(0.003), t0 + Duration::from_millis(i));
        }
        h.record(&inst, dec!(0.05), t0 + Duration::from_millis(100));
        let st = h.stats(&inst, t0 + Duration::from_millis(101)).unwrap();
        assert!(st.baseline_pct < dec!(0.01), "baseline should be tight");
        assert_eq!(st.current_pct, dec!(0.05));
        assert!(st.z_score.unwrap() > dec!(2), "spike should be a strong outlier");
        assert!(st.sample_count >= 20);
    }

    #[test]
    fn episode_clock_tracks_persistence() {
        let h = SpreadHistory::new(cfg());
        let inst = Instrument::perp("XYZ", "USDT");
        let t0 = Instant::now();
        h.record(&inst, dec!(0.10), t0); // above threshold -> episode opens
        let st = h.stats(&inst, t0 + Duration::from_millis(5000)).unwrap();
        assert!(st.episode_ms >= 5000);
        // Drops below threshold -> episode resets.
        h.record(&inst, dec!(0.001), t0 + Duration::from_millis(6000));
        let st = h.stats(&inst, t0 + Duration::from_millis(6000)).unwrap();
        assert_eq!(st.episode_ms, 0);
    }
}
