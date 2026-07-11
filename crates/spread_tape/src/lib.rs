//! Per-instrument rolling raw-spread tape that drives the real-time chart.
//!
//! Decoupled from the alert engine: a fixed-cadence sampler pushes one
//! [`VenueSample`] per instrument per tick via [`SpreadTape::record`] — a snapshot
//! of every venue's VWAP quotes, so the chart can derive In/Out for **any** fixed
//! pair (including on backfill), not just the best pair at each tick. Each tape
//! keeps a ring buffer of the last `window` of samples and a broadcast channel
//! that fans live samples out to watchers; per-pair transformation happens at the
//! delivery layer.

use domain::{Instrument, VenueSample};
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

/// Concurrent map of per-instrument tapes.
pub struct SpreadTape {
    tapes: dashmap::DashMap<Instrument, InstrTape>,
    resolution: Duration,
    capacity: usize,
    broadcast_capacity: usize,
}

impl SpreadTape {
    pub fn new(window: Duration, resolution: Duration, broadcast_capacity: usize) -> Self {
        let res_ms = resolution.as_millis().max(1) as usize;
        let capacity = (window.as_millis() as usize / res_ms).max(1) + 4;
        SpreadTape {
            tapes: dashmap::DashMap::new(),
            resolution,
            capacity,
            broadcast_capacity,
        }
    }

    pub fn resolution_ms(&self) -> u64 {
        self.resolution.as_millis() as u64
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

    #[test]
    fn watch_none_for_unknown_instrument() {
        let tape = SpreadTape::new(Duration::from_secs(60), Duration::from_secs(1), 16);
        assert!(tape.watch(&Instrument::perp("NOPE", "USDT"), 5000).is_none());
    }
}
