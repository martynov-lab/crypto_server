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
///
/// Baseline and dispersion are **robust** (median / MAD) and computed over the
/// *quiet* part of the window — samples recorded before the currently-open
/// episode. Using the plain mean and stddev over the whole window lets a spike
/// inflate its own dispersion and mask itself, which is exactly backwards for a
/// spike detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpreadStats {
    /// Median net spread of the quiet window — the level a position is expected
    /// to unwind at.
    pub baseline_pct: Decimal,
    /// Median absolute deviation, scaled to be a stddev-equivalent.
    pub mad_pct: Decimal,
    /// Plain stddev over the quiet window (kept for display/compatibility).
    pub stddev_pct: Decimal,
    pub current_pct: Decimal,
    /// Robust z-score of `current_pct` against the quiet baseline.
    pub z_score: Option<Decimal>,
    pub sample_count: usize,
    /// Samples that fed the baseline (excludes the live episode).
    pub baseline_samples: usize,
    /// How long the spread has continuously exceeded `reference_threshold`.
    pub episode_ms: u64,
}

/// Below this many quiet samples the episode-exclusion is abandoned and the
/// whole window is used — a baseline from three points is worse than a slightly
/// contaminated one.
const MIN_QUIET_SAMPLES: usize = 8;

/// Dispersion floor (5 bps) for the z-score denominator. A perfectly flat
/// baseline has zero MAD *and* zero stddev, which would otherwise make every
/// tick either an infinite outlier or an unmeasurable one. Flooring it means a
/// spike is scored against a realistic minimum of quote noise.
const MIN_DISPERSION: Decimal = Decimal::from_parts(5, 0, 0, false, 4); // 0.0005

impl SpreadStats {
    /// Robust z-score of an arbitrary spread against this instrument's quiet
    /// baseline. Callers pass **their own** current spread (per-client fees
    /// differ from the shared history's), rather than trusting `z_score`.
    ///
    /// `None` only when the window is degenerate enough to carry no signal.
    pub fn z_for(&self, value: Decimal) -> Option<Decimal> {
        let scale = self
            .mad_pct
            .max(self.stddev_pct)
            .max(MIN_DISPERSION);
        Some((value - self.baseline_pct) / scale)
    }
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
        let all: Vec<(Instant, f64)> = s
            .samples
            .iter()
            .filter_map(|(t, d)| d.to_f64().map(|v| (*t, v)))
            .collect();
        let (_, current) = *all.last()?;
        let total = all.len();

        // Baseline over the quiet window: drop everything recorded since the
        // current above-threshold episode opened, so a spike cannot raise the
        // baseline it is being measured against. Fall back to the full window
        // when that leaves too little to estimate from.
        let mut quiet: Vec<f64> = match s.episode_open {
            Some(open) => all.iter().filter(|(t, _)| *t < open).map(|(_, v)| *v).collect(),
            None => all.iter().map(|(_, v)| *v).collect(),
        };
        if quiet.len() < MIN_QUIET_SAMPLES {
            quiet = all.iter().map(|(_, v)| *v).collect();
        }
        let baseline_samples = quiet.len();

        quiet.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = median_of_sorted(&quiet);

        // MAD, scaled by 1.4826 so it estimates the same quantity as stddev for
        // normal data but ignores the outliers we are trying to detect.
        let mut deviations: Vec<f64> = quiet.iter().map(|v| (v - median).abs()).collect();
        deviations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mad = median_of_sorted(&deviations) * 1.4826;

        let n = quiet.len() as f64;
        let mean = quiet.iter().sum::<f64>() / n;
        let stddev = (quiet.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();

        let episode_ms = s
            .episode_open
            .map(|t| now.saturating_duration_since(t).as_millis() as u64)
            .unwrap_or(0);

        let stats = SpreadStats {
            baseline_pct: Decimal::from_f64(median).unwrap_or_default(),
            mad_pct: Decimal::from_f64(mad).unwrap_or_default(),
            stddev_pct: Decimal::from_f64(stddev).unwrap_or_default(),
            current_pct: Decimal::from_f64(current).unwrap_or_default(),
            z_score: None,
            sample_count: total,
            baseline_samples,
            episode_ms,
        };
        let z_score = stats.z_for(stats.current_pct);
        Some(SpreadStats { z_score, ..stats })
    }
}

/// Median of an already-sorted slice; 0.0 when empty.
fn median_of_sorted(v: &[f64]) -> f64 {
    match v.len() {
        0 => 0.0,
        n if n % 2 == 1 => v[n / 2],
        n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
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
    fn spike_does_not_inflate_its_own_baseline() {
        // A sustained spike used to raise both the mean and the stddev it was
        // measured against, so a real dislocation scored as "not a spike".
        let h = SpreadHistory::new(cfg());
        let inst = Instrument::perp("XYZ", "USDT");
        let t0 = Instant::now();
        for i in 0..40 {
            h.record(&inst, dec!(0.003), t0 + Duration::from_millis(i));
        }
        // 30 consecutive ticks of a 5% spread — the episode stays open.
        for i in 0..30 {
            h.record(&inst, dec!(0.05), t0 + Duration::from_millis(100 + i));
        }
        let st = h.stats(&inst, t0 + Duration::from_millis(200)).unwrap();
        assert_eq!(st.baseline_pct, dec!(0.003), "baseline must ignore the episode");
        assert_eq!(st.baseline_samples, 40, "only the quiet window feeds it");
        assert!(st.z_score.unwrap() > dec!(3), "sustained spike stays an outlier");
    }

    #[test]
    fn z_for_scores_a_caller_supplied_spread() {
        let h = SpreadHistory::new(cfg());
        let inst = Instrument::perp("XYZ", "USDT");
        let t0 = Instant::now();
        for i in 0..20 {
            h.record(&inst, dec!(0.003), t0 + Duration::from_millis(i));
        }
        let st = h.stats(&inst, t0 + Duration::from_millis(21)).unwrap();
        // A spread at the baseline is not an outlier; one far above it is.
        assert!(st.z_for(dec!(0.003)).unwrap() < dec!(1));
        assert!(st.z_for(dec!(0.05)).unwrap() > dec!(3));
    }

    #[test]
    fn flat_baseline_still_scores() {
        // Zero MAD and zero stddev must not produce an unusable z-score.
        let h = SpreadHistory::new(cfg());
        let inst = Instrument::perp("XYZ", "USDT");
        let t0 = Instant::now();
        for i in 0..10 {
            h.record(&inst, dec!(0.001), t0 + Duration::from_millis(i));
        }
        let st = h.stats(&inst, t0 + Duration::from_millis(11)).unwrap();
        assert_eq!(st.mad_pct, dec!(0));
        assert!(st.z_for(dec!(0.01)).is_some());
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
