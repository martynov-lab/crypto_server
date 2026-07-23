//! Per-instrument rolling raw-spread tape that drives the real-time chart.
//!
//! Decoupled from the alert engine: a fixed-cadence sampler pushes one
//! [`VenueSample`] per instrument per tick via [`SpreadTape::record`] — a snapshot
//! of every venue's VWAP quotes, so the chart can derive In/Out for **any** fixed
//! pair (including on backfill), not just the best pair at each tick. Each tape
//! keeps a ring buffer of the last `window` of samples and a broadcast channel
//! that fans live samples out to watchers; per-pair transformation happens at the
//! delivery layer.

use domain::{Instrument, SpreadBucket, SpreadPoint, VenueSample};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::broadcast;

/// Handle a watcher holds: the buffered samples within the requested window plus
/// a live receiver for subsequent samples.
pub struct Watch {
    pub backfill: Vec<VenueSample>,
    pub live: broadcast::Receiver<VenueSample>,
    /// The cadence the server actually samples at (echoed to the client).
    pub resolution_ms: u64,
}

struct InstrTape {
    buffer: VecDeque<VenueSample>,
    tx: Option<broadcast::Sender<VenueSample>>,
}

impl InstrTape {
    fn new() -> Self {
        InstrTape {
            buffer: VecDeque::new(),
            tx: None,
        }
    }
}

/// Per-instrument long history: closed minute-buckets plus the one being built.
struct InstrHistory {
    buckets: VecDeque<SpreadBucket>,
    current: Option<SpreadBucket>,
}

impl InstrHistory {
    fn new() -> Self {
        InstrHistory {
            buckets: VecDeque::new(),
            current: None,
        }
    }
}

/// Concurrent map of per-instrument tapes.
pub struct SpreadTape {
    tapes: dashmap::DashMap<Instrument, InstrTape>,
    resolution: Duration,
    capacity: usize,
    broadcast_capacity: usize,
    /// Long-history tier: coarse best-pair aggregates, small enough to retain
    /// for days where the fine venue-sample ring can only afford minutes.
    history: dashmap::DashMap<Instrument, InstrHistory>,
    history_resolution_ms: i64,
    history_capacity: usize,
}

impl SpreadTape {
    pub fn new(window: Duration, resolution: Duration, broadcast_capacity: usize) -> Self {
        // Long-history defaults: 3 days of 1-minute buckets.
        Self::with_history(
            window,
            resolution,
            broadcast_capacity,
            Duration::from_secs(3 * 24 * 3600),
            Duration::from_secs(60),
        )
    }

    pub fn with_history(
        window: Duration,
        resolution: Duration,
        broadcast_capacity: usize,
        history_window: Duration,
        history_resolution: Duration,
    ) -> Self {
        let res_ms = resolution.as_millis().max(1) as usize;
        let capacity = (window.as_millis() as usize / res_ms).max(1) + 4;
        let hist_res_ms = history_resolution.as_millis().max(1) as i64;
        let history_capacity =
            (history_window.as_millis() as usize / hist_res_ms as usize).max(1) + 1;
        SpreadTape {
            tapes: dashmap::DashMap::new(),
            resolution,
            capacity,
            broadcast_capacity,
            history: dashmap::DashMap::new(),
            history_resolution_ms: hist_res_ms,
            history_capacity,
        }
    }

    pub fn resolution_ms(&self) -> u64 {
        self.resolution.as_millis() as u64
    }

    pub fn history_resolution_ms(&self) -> u64 {
        self.history_resolution_ms as u64
    }

    /// Longest window the long-history tier can ever answer for (ms).
    pub fn history_window_ms(&self) -> u64 {
        self.history_capacity as u64 * self.history_resolution_ms as u64
    }

    /// Append a sampled venue snapshot: buffer it (trim to capacity) and, if
    /// anyone is watching, broadcast it live.
    pub fn record(&self, instrument: &Instrument, sample: VenueSample) {
        let mut tape = self.tapes.entry(instrument.clone()).or_insert_with(InstrTape::new);
        tape.buffer.push_back(sample.clone());
        while tape.buffer.len() > self.capacity {
            tape.buffer.pop_front();
        }
        if let Some(tx) = &tape.tx {
            let _ = tx.send(sample);
        }
    }

    /// Start watching `instrument`: returns the backfill within `window_ms` plus a
    /// live receiver. `None` if the instrument has no samples yet.
    pub fn watch(&self, instrument: &Instrument, window_ms: u64) -> Option<Watch> {
        let mut tape = self.tapes.get_mut(instrument)?;
        if tape.buffer.is_empty() {
            return None;
        }
        let cutoff = tape.buffer.back().map(|s| s.ts_ms).unwrap_or(0) - window_ms as i64;
        let backfill: Vec<VenueSample> = tape
            .buffer
            .iter()
            .filter(|s| s.ts_ms >= cutoff)
            .cloned()
            .collect();
        let tx = tape
            .tx
            .get_or_insert_with(|| broadcast::channel(self.broadcast_capacity).0);
        Some(Watch {
            backfill,
            live: tx.subscribe(),
            resolution_ms: self.resolution_ms(),
        })
    }

    /// Fold one best-pair sample into the instrument's long history. Called by
    /// the same fixed-cadence sampler that feeds [`record`](Self::record); the
    /// caller applies the sanity/anomaly cap first so data errors never enter
    /// the multi-day view.
    pub fn record_point(&self, instrument: &Instrument, point: &SpreadPoint) {
        let bucket_ts = point.ts_ms - point.ts_ms.rem_euclid(self.history_resolution_ms);
        let mut h = self
            .history
            .entry(instrument.clone())
            .or_insert_with(InstrHistory::new);

        match &mut h.current {
            Some(cur) if cur.ts_ms == bucket_ts => {
                cur.min_net_pct = cur.min_net_pct.min(point.net_pct);
                if point.net_pct > cur.max_net_pct {
                    cur.max_net_pct = point.net_pct;
                    cur.buy_exchange = point.buy_exchange;
                    cur.sell_exchange = point.sell_exchange;
                }
                cur.close_net_pct = point.net_pct;
                cur.samples += 1;
            }
            _ => {
                // Close the previous bucket (if any) and start a new one.
                if let Some(done) = h.current.take() {
                    h.buckets.push_back(done);
                    while h.buckets.len() > self.history_capacity {
                        h.buckets.pop_front();
                    }
                }
                h.current = Some(SpreadBucket {
                    ts_ms: bucket_ts,
                    min_net_pct: point.net_pct,
                    max_net_pct: point.net_pct,
                    close_net_pct: point.net_pct,
                    buy_exchange: point.buy_exchange,
                    sell_exchange: point.sell_exchange,
                    samples: 1,
                });
            }
        }
    }

    /// Long-history buckets within `window_ms` (oldest first), including the
    /// bucket currently being built. `None` if the instrument has no history.
    pub fn long_history(
        &self,
        instrument: &Instrument,
        window_ms: u64,
    ) -> Option<Vec<SpreadBucket>> {
        let h = self.history.get(instrument)?;
        let newest = h
            .current
            .as_ref()
            .map(|c| c.ts_ms)
            .or_else(|| h.buckets.back().map(|b| b.ts_ms))?;
        let cutoff = newest - window_ms as i64;
        let mut out: Vec<SpreadBucket> = h
            .buckets
            .iter()
            .filter(|b| b.ts_ms >= cutoff)
            .cloned()
            .collect();
        if let Some(cur) = &h.current {
            out.push(cur.clone());
        }
        Some(out)
    }

    /// Backfill-only history (for the REST fallback) within `window_ms`.
    pub fn history(&self, instrument: &Instrument, window_ms: u64) -> Option<Vec<VenueSample>> {
        let tape = self.tapes.get(instrument)?;
        if tape.buffer.is_empty() {
            return None;
        }
        let cutoff = tape.buffer.back().map(|s| s.ts_ms).unwrap_or(0) - window_ms as i64;
        Some(
            tape.buffer
                .iter()
                .filter(|s| s.ts_ms >= cutoff)
                .cloned()
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{ExchangeId, VenueQuote};
    use rust_decimal_macros::dec;

    fn sample(ts: i64) -> VenueSample {
        VenueSample {
            ts_ms: ts,
            baseline_pct: None,
            venues: vec![VenueQuote {
                exchange: ExchangeId::Gate,
                vwap_ask: dec!(100),
                vwap_bid: dec!(99.9),
                ask_notional: dec!(2000),
                bid_notional: dec!(2000),
                ask_capped: false,
                bid_capped: false,
                funding_rate: None,
                funding_interval_hours: None,
                next_funding_ms: None,
            }],
        }
    }

    #[test]
    fn buffer_trims_to_capacity() {
        let tape = SpreadTape::new(Duration::from_secs(5), Duration::from_secs(1), 16);
        let inst = Instrument::perp("XYZ", "USDT");
        for i in 0..100 {
            tape.record(&inst, sample(i));
        }
        let hist = tape.history(&inst, 60_000).unwrap();
        assert!(hist.len() <= 16, "buffer should be bounded, got {}", hist.len());
        assert_eq!(hist.last().unwrap().ts_ms, 99);
    }

    #[test]
    fn watch_backfills_within_window_and_streams() {
        let tape = SpreadTape::new(Duration::from_secs(60), Duration::from_secs(1), 16);
        let inst = Instrument::perp("XYZ", "USDT");
        for i in 0..10 {
            tape.record(&inst, sample(i * 1000));
        }
        let mut w = tape.watch(&inst, 5000).unwrap();
        assert_eq!(w.backfill.len(), 6); // ts 4000..9000
        tape.record(&inst, sample(10_000));
        assert_eq!(w.live.try_recv().unwrap().ts_ms, 10_000);
    }

    fn point(ts: i64, net: rust_decimal::Decimal) -> domain::SpreadPoint {
        domain::SpreadPoint {
            ts_ms: ts,
            net_pct: net,
            gross_pct: net,
            baseline_pct: None,
            buy_exchange: ExchangeId::Gate,
            sell_exchange: ExchangeId::Bybit,
            executable_notional: dec!(2000),
            capped_by_depth: false,
        }
    }

    #[test]
    fn long_history_aggregates_minute_buckets() {
        let tape = SpreadTape::with_history(
            Duration::from_secs(60),
            Duration::from_secs(1),
            16,
            Duration::from_secs(3600),
            Duration::from_secs(60),
        );
        let inst = Instrument::perp("XYZ", "USDT");
        // Minute 0: three samples, max in the middle. Minute 1: one sample.
        tape.record_point(&inst, &point(1_000, dec!(0.004)));
        tape.record_point(&inst, &point(30_000, dec!(0.012)));
        tape.record_point(&inst, &point(59_000, dec!(0.006)));
        tape.record_point(&inst, &point(61_000, dec!(0.002)));

        let h = tape.long_history(&inst, 3_600_000).unwrap();
        assert_eq!(h.len(), 2);
        let m0 = &h[0];
        assert_eq!(m0.ts_ms, 0);
        assert_eq!(m0.min_net_pct, dec!(0.004));
        assert_eq!(m0.max_net_pct, dec!(0.012));
        assert_eq!(m0.close_net_pct, dec!(0.006));
        assert_eq!(m0.samples, 3);
        // The in-progress bucket is included.
        assert_eq!(h[1].ts_ms, 60_000);
        assert_eq!(h[1].samples, 1);
    }

    #[test]
    fn long_history_retention_is_bounded() {
        // 5-minute retention at 1-minute buckets.
        let tape = SpreadTape::with_history(
            Duration::from_secs(60),
            Duration::from_secs(1),
            16,
            Duration::from_secs(300),
            Duration::from_secs(60),
        );
        let inst = Instrument::perp("XYZ", "USDT");
        for m in 0..60 {
            tape.record_point(&inst, &point(m * 60_000, dec!(0.005)));
        }
        let h = tape.long_history(&inst, u64::MAX / 2).unwrap();
        assert!(h.len() <= 7, "retention must be bounded, got {}", h.len());
        assert_eq!(h.last().unwrap().ts_ms, 59 * 60_000);
    }

    #[test]
    fn watch_none_for_unknown_instrument() {
        let tape = SpreadTape::new(Duration::from_secs(60), Duration::from_secs(1), 16);
        assert!(tape.watch(&Instrument::perp("NOPE", "USDT"), 5000).is_none());
    }
}
