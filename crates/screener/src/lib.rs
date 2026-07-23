//! Screener engine: turns market snapshots into client signals by applying
//! filters (`rules`), executable spread math (`executable`), funding
//! differential (`funding`), and hysteresis/lifetime dedup (`hysteresis`).

pub mod chart;
pub mod config;
pub mod executable;
pub mod funding;
pub mod hysteresis;
pub mod rules;

use dashmap::DashMap;
use domain::{Decimal, Instrument, Spread, SpreadReason};
use hysteresis::{Decision, PeakState};
use market_state::{InstrumentSnapshot, SpreadStats};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::debug;

pub use chart::{best_pair, chart_point, venue_sample};
pub use config::{ClientConfig, MarketPair};
pub use funding::FundingSignal;
pub use rules::{
    best_raw_net, best_spread_point, evaluate, summary_row, Evaluation, NoTransferInfo,
    TransferOracle,
};

/// A signal pushed to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenerEvent {
    pub spread: Spread,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funding: Option<FundingSignal>,
    /// Rolling spread statistics (baseline/spike/episode) for the instrument.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamics: Option<SpreadStats>,
    /// 0–100 arb-quality score.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality_score: Option<Decimal>,
    /// Server wall-clock time of emission (ms since epoch).
    pub ts_ms: i64,
}

/// Static label for a rejection reason, for use as a metric dimension.
fn reason_label(reason: &SpreadReason) -> &'static str {
    match reason {
        SpreadReason::Signal => "signal",
        SpreadReason::BboOnly => "bbo_only",
        SpreadReason::BelowMinSpread => "below_min_spread",
        SpreadReason::AboveMaxSpread => "above_max_spread",
        SpreadReason::InsufficientDepth => "insufficient_depth",
        SpreadReason::NotTransferable => "not_transferable",
        SpreadReason::NoCommonNetwork => "no_common_network",
        SpreadReason::StaleBook => "stale_book",
        SpreadReason::BelowMinVolume => "below_min_volume",
        SpreadReason::AboveMaxVolume => "above_max_volume",
        SpreadReason::BelowMinOpenInterest => "below_min_open_interest",
        SpreadReason::PersistentWide => "persistent_wide",
        SpreadReason::NotASpike => "not_a_spike",
        SpreadReason::TooPersistent => "too_persistent",
        SpreadReason::LegSkew => "leg_skew",
        SpreadReason::NegativeRoundTrip => "negative_round_trip",
        SpreadReason::PriceOutlier => "price_outlier",
    }
}

/// Per-instrument screening state for one client.
#[derive(Debug, Default)]
struct InstrState {
    peak: PeakState,
    /// When the current above-threshold episode began (for lifetime + persistence).
    opened_at: Option<Instant>,
    last_emit: Option<Instant>,
    /// Consecutive non-signal evaluations, for debounced episode close.
    reject_streak: u32,
}

/// Simple rolling per-minute emit counter for the whole engine (per client).
#[derive(Debug)]
struct RateWindow {
    window_start: Instant,
    count: u32,
}

/// One client's screening engine. Cheap to construct per WS session; the market
/// data it reads is shared and passed in per call.
pub struct ScreenerEngine {
    cfg: ClientConfig,
    state: DashMap<Instrument, InstrState>,
    rate: Mutex<RateWindow>,
}

impl ScreenerEngine {
    pub fn new(cfg: ClientConfig) -> Self {
        ScreenerEngine {
            cfg,
            state: DashMap::new(),
            rate: Mutex::new(RateWindow {
                window_start: Instant::now(),
                count: 0,
            }),
        }
    }

    pub fn config(&self) -> &ClientConfig {
        &self.cfg
    }

    /// Evaluate one instrument after its market state changed. Returns an event
    /// only when a fresh, filter-passing, non-duplicate signal should be pushed.
    pub fn on_instrument(
        &self,
        snapshot: &InstrumentSnapshot,
        oracle: &dyn TransferOracle,
        now: Instant,
        ts_ms: i64,
    ) -> Option<ScreenerEvent> {
        let eval = evaluate(snapshot, &self.cfg, oracle)?;
        let mut st = self.state.entry(snapshot.instrument.clone()).or_default();

        if eval.reason != SpreadReason::Signal {
            // Which filter is doing the rejecting is the first thing you need
            // when the screener goes quiet, so it is a labelled counter rather
            // than a log line lost in the per-tick volume.
            metrics::counter!(
                "screener_rejections_total",
                "reason" => reason_label(&eval.reason),
            )
            .increment(1);
            // Debounced episode close: one tick grazing a filter boundary is
            // noise, not the end of the opportunity. Only a sustained run of
            // rejections closes the episode and re-arms hysteresis.
            st.reject_streak += 1;
            if st.reject_streak >= self.cfg.episode_close_ticks.max(1) {
                if let Some(opened) = st.opened_at.take() {
                    let lifetime = now.saturating_duration_since(opened);
                    debug!(
                        instrument = %snapshot.instrument,
                        reason = ?eval.reason,
                        lifetime_ms = lifetime.as_millis() as u64,
                        "spread episode closed"
                    );
                }
                st.peak = PeakState::default();
            }
            return None;
        }
        st.reject_streak = 0;

        // --- Signal path ---
        if st.opened_at.is_none() {
            st.opened_at = Some(now);
        }
        let opened = st.opened_at.unwrap_or(now);

        // Every non-mutating gate runs *before* hysteresis. `PeakState::decide`
        // raises the peak as a side effect, so calling it first would let a
        // suppressed tick consume the crossing: the peak moves up, the emit is
        // then dropped by lifetime/cooldown/rate, and the opportunity can never
        // fire again until it widens by another hysteresis step. A steady
        // in-band spread produced no alert at all because of this.
        if now.saturating_duration_since(opened)
            < Duration::from_millis(self.cfg.min_signal_lifetime_ms)
        {
            return None;
        }
        if let Some(last) = st.last_emit {
            if now.saturating_duration_since(last) < Duration::from_millis(self.cfg.cooldown_ms) {
                return None;
            }
        }

        let previous_peak = st.peak.clone();
        let decision = st.peak.decide(
            eval.spread.net_pct,
            self.cfg.min_net_spread_pct,
            self.cfg.hysteresis_step_pct,
        );
        if decision != Decision::Emit {
            return None;
        }

        // Global per-minute rate cap. Restore the peak when it denies the emit,
        // so a rate-limited tick doesn't silently consume the crossing either.
        if !self.allow_emit(now) {
            st.peak = previous_peak;
            return None;
        }

        st.last_emit = Some(now);
        Some(ScreenerEvent {
            spread: eval.spread,
            funding: eval.funding,
            dynamics: eval.stats,
            quality_score: eval.quality_score,
            ts_ms,
        })
    }

    /// Rolling one-minute rate limit. Returns true if an emission is allowed.
    fn allow_emit(&self, now: Instant) -> bool {
        let Some(max) = self.cfg.max_signals_per_min else {
            return true;
        };
        let mut w = match self.rate.lock() {
            Ok(w) => w,
            Err(_) => return true,
        };
        if now.saturating_duration_since(w.window_start) >= Duration::from_secs(60) {
            w.window_start = now;
            w.count = 0;
        }
        if w.count >= max {
            return false;
        }
        w.count += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{BookLevel, Decimal, ExchangeId, Instrument, TopBook};
    use market_state::{ExchangeQuote, InstrumentSnapshot};
    use rust_decimal_macros::dec;

    fn book(bid: Decimal, ask: Decimal) -> TopBook {
        TopBook {
            bids: vec![BookLevel::new(bid, dec!(1000))],
            asks: vec![BookLevel::new(ask, dec!(1000))],
            recv_ts: Instant::now(),
            exch_ts: None,
        }
    }

    fn quote(ex: ExchangeId, bid: Decimal, ask: Decimal) -> ExchangeQuote {
        ExchangeQuote {
            exchange: ex,
            book: book(bid, ask),
            funding: None,
            quote_volume_24h: None,
            open_interest: None,
            as_of_age_ms: 0,
            stale: false,
            valid: true,
        }
    }

    fn snapshot(cheap_ask: Decimal, rich_bid: Decimal) -> InstrumentSnapshot {
        InstrumentSnapshot {
            instrument: Instrument::perp("XYZ", "USDT"),
            quotes: vec![
                quote(ExchangeId::Bybit, cheap_ask - dec!(1), cheap_ask), // buy here
                quote(ExchangeId::Okx, rich_bid, rich_bid + dec!(1)),     // sell here
            ],
            stats: None,
        }
    }

    fn cfg_no_transfer() -> ClientConfig {
        let mut c = ClientConfig::default();
        c.require_transferable = false;
        c.require_common_network = false;
        c.min_signal_lifetime_ms = 0;
        c.cooldown_ms = 0;
        c
    }

    #[test]
    fn emits_signal_in_band_then_dedups() {
        // buy at 100, sell at 106 => gross ~6%, within 2..20% band.
        let engine = ScreenerEngine::new(cfg_no_transfer());
        let snap = snapshot(dec!(100), dec!(106));
        let now = Instant::now();
        let e1 = engine.on_instrument(&snap, &NoTransferInfo, now, 1);
        assert!(e1.is_some(), "first crossing should emit");
        let ev = e1.unwrap();
        assert_eq!(ev.spread.buy_exchange, ExchangeId::Bybit);
        assert_eq!(ev.spread.sell_exchange, ExchangeId::Okx);

        // Same spread again => hysteresis suppresses.
        let e2 = engine.on_instrument(&snap, &NoTransferInfo, now, 2);
        assert!(e2.is_none(), "duplicate should be suppressed");
    }

    #[test]
    fn above_max_band_is_rejected() {
        let engine = ScreenerEngine::new(cfg_no_transfer());
        // buy 100, sell 150 => 50% gross, above 20% cap => ghost, no emit.
        let snap = snapshot(dec!(100), dec!(150));
        let e = engine.on_instrument(&snap, &NoTransferInfo, Instant::now(), 1);
        assert!(e.is_none());
    }

    /// Regression: hysteresis used to run before the lifetime gate, so the very
    /// first tick raised the peak while its emit was discarded for being too
    /// young — and every later tick at the same spread was then "not a new
    /// peak". A steady, perfectly tradable spread produced zero alerts.
    #[test]
    fn steady_spread_emits_once_it_has_persisted() {
        let mut c = cfg_no_transfer();
        c.min_signal_lifetime_ms = 1500;
        let engine = ScreenerEngine::new(c);
        let snap = snapshot(dec!(100), dec!(106));
        let t0 = Instant::now();

        assert!(
            engine.on_instrument(&snap, &NoTransferInfo, t0, 1).is_none(),
            "episode is younger than min_signal_lifetime_ms"
        );
        let t1 = t0 + Duration::from_millis(2000);
        assert!(
            engine.on_instrument(&snap, &NoTransferInfo, t1, 2).is_some(),
            "same spread must alert once it has persisted"
        );
    }

    /// A tick denied by the global rate cap must not consume the crossing
    /// either — the peak is restored so the next allowed tick can still fire.
    #[test]
    fn rate_limited_tick_does_not_consume_the_crossing() {
        let mut c = cfg_no_transfer();
        c.max_signals_per_min = Some(1);
        let engine = ScreenerEngine::new(c);
        let snap_a = snapshot(dec!(100), dec!(106));
        let mut snap_b = snapshot(dec!(100), dec!(106));
        snap_b.instrument = Instrument::perp("ABC", "USDT");
        let t0 = Instant::now();

        assert!(engine.on_instrument(&snap_a, &NoTransferInfo, t0, 1).is_some());
        assert!(
            engine.on_instrument(&snap_b, &NoTransferInfo, t0, 2).is_none(),
            "second instrument is over the per-minute cap"
        );
        // New rate window: the deferred opportunity is still alertable.
        let t1 = t0 + Duration::from_secs(61);
        assert!(
            engine.on_instrument(&snap_b, &NoTransferInfo, t1, 3).is_some(),
            "rate cap must defer the signal, not destroy it"
        );
    }

    /// One rejected tick is noise; only a sustained run of them closes the
    /// episode and re-arms hysteresis.
    #[test]
    fn episode_close_is_debounced() {
        let mut c = cfg_no_transfer();
        c.episode_close_ticks = 3;
        let engine = ScreenerEngine::new(c);
        let good = snapshot(dec!(100), dec!(106));
        let flat = snapshot(dec!(100), dec!(100.05)); // below the spread floor
        let t = Instant::now();

        assert!(engine.on_instrument(&good, &NoTransferInfo, t, 1).is_some());
        // A single boundary-grazing tick must not re-arm the engine.
        assert!(engine.on_instrument(&flat, &NoTransferInfo, t, 2).is_none());
        assert!(
            engine.on_instrument(&good, &NoTransferInfo, t, 3).is_none(),
            "episode is still open; this is the same opportunity"
        );
        // Three in a row does close it.
        for i in 0..3 {
            assert!(engine.on_instrument(&flat, &NoTransferInfo, t, 10 + i).is_none());
        }
        assert!(
            engine.on_instrument(&good, &NoTransferInfo, t, 20).is_some(),
            "a genuinely new episode alerts again"
        );
    }

    #[test]
    fn requires_transferable_when_configured() {
        // With transfer required and NoTransferInfo (no data), signal is blocked.
        let mut cfg = cfg_no_transfer();
        cfg.require_transferable = true;
        let engine = ScreenerEngine::new(cfg);
        let snap = snapshot(dec!(100), dec!(106));
        let e = engine.on_instrument(&snap, &NoTransferInfo, Instant::now(), 1);
        assert!(e.is_none(), "should be blocked by missing transfer status");
    }
}
