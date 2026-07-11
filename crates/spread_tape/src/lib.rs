//! Per-instrument rolling raw-spread tape that drives the real-time chart.
//!
//! Decoupled from the alert engine: a fixed-cadence sampler pushes one
//! [`SpreadPoint`] per instrument per tick via [`SpreadTape::record`]. Each tape
//! keeps a ring buffer of the last `window` of points (for instant backfill on
//! open) and a broadcast channel that fans live points out to any watchers.

use domain::{Instrument, SpreadPoint};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::broadcast;

/// Handle a watcher holds: the backfill (already-buffered points within the
/// requested window) plus a live receiver for subsequent points.
pub struct Watch {
    pub backfill: Vec<SpreadPoint>,
    pub live: broadcast::Receiver<SpreadPoint>,
    /// The cadence the server actually samples at (echoed to the client).
    pub resolution_ms: u64,
}

struct InstrTape {
    buffer: VecDeque<SpreadPoint>,
    tx: Option<broadcast::Sender<SpreadPoint>>,
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
    /// Fixed sampling cadence (informational; echoed to clients).
    resolution: Duration,
    /// Max points retained per instrument (≈ window / resolution).
    capacity: usize,
    /// Broadcast channel capacity per instrument (live tick buffering).
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

    /// Append a sampled point for `instrument`: buffer it (trimming to capacity)
    /// and, if anyone is watching, broadcast it live.
    pub fn record(&self, instrument: &Instrument, point: SpreadPoint) {
        let mut tape = self.tapes.entry(instrument.clone()).or_insert_with(InstrTape::new);
        tape.buffer.push_back(point.clone());
        while tape.buffer.len() > self.capacity {
            tape.buffer.pop_front();
        }
        if let Some(tx) = &tape.tx {
            // Ignore "no receivers": we keep the sender for the next watcher.
            let _ = tx.send(point);
        }
    }

    /// Start watching `instrument`: returns the backfill within `window_ms` plus
    /// a live receiver. `None` if the instrument has no samples yet (not screened
    /// / < 2 venues).
    pub fn watch(&self, instrument: &Instrument, window_ms: u64) -> Option<Watch> {
        let mut tape = self.tapes.get_mut(instrument)?;
        if tape.buffer.is_empty() {
            return None;
        }
        let latest_ts = tape.buffer.back().map(|p| p.ts_ms).unwrap_or(0);
        let cutoff = latest_ts - window_ms as i64;
        let backfill: Vec<SpreadPoint> = tape
            .buffer
            .iter()
            .filter(|p| p.ts_ms >= cutoff)
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
    pub fn history(&self, instrument: &Instrument, window_ms: u64) -> Option<Vec<SpreadPoint>> {
        let tape = self.tapes.get(instrument)?;
        if tape.buffer.is_empty() {
            return None;
        }
        let latest_ts = tape.buffer.back().map(|p| p.ts_ms).unwrap_or(0);
        let cutoff = latest_ts - window_ms as i64;
        Some(
            tape.buffer
                .iter()
                .filter(|p| p.ts_ms >= cutoff)
                .cloned()
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::ExchangeId;
    use rust_decimal_macros::dec;

    fn point(ts: i64, net: domain::Decimal) -> SpreadPoint {
        SpreadPoint {
            ts_ms: ts,
            net_pct: net,
            gross_pct: net,
            baseline_pct: None,
            buy_exchange: ExchangeId::Mexc,
            sell_exchange: ExchangeId::Kucoin,
            executable_notional: dec!(2000),
            capped_by_depth: false,
        }
    }

    #[test]
    fn buffer_trims_to_capacity() {
        // window 5s @ 1s => capacity ~9.
        let tape = SpreadTape::new(Duration::from_secs(5), Duration::from_secs(1), 16);
        let inst = Instrument::perp("XYZ", "USDT");
        for i in 0..100 {
            tape.record(&inst, point(i, dec!(0.01)));
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
            tape.record(&inst, point(i * 1000, dec!(0.01)));
        }
        // window 5s => points with ts >= 9000-5000 = 4000 => ts 4000..9000 (6 pts)
        let mut w = tape.watch(&inst, 5000).unwrap();
        assert_eq!(w.backfill.len(), 6);
        // A new record is delivered live.
        tape.record(&inst, point(10_000, dec!(0.05)));
        let live = w.live.try_recv().unwrap();
        assert_eq!(live.ts_ms, 10_000);
        assert_eq!(live.net_pct, dec!(0.05));
    }

    #[test]
    fn watch_none_for_unknown_instrument() {
        let tape = SpreadTape::new(Duration::from_secs(60), Duration::from_secs(1), 16);
        assert!(tape.watch(&Instrument::perp("NOPE", "USDT"), 5000).is_none());
    }
}
