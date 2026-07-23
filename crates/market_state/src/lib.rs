//! In-memory market state: latest top-of-book and funding per (exchange,
//! instrument), with staleness-aware read views for the screener.

pub mod aggregate;
pub mod dynamics;

use dashmap::DashMap;
use domain::{ExchangeId, FundingInfo, Instrument, MarketUpdate, TopBook};
use dynamics::{DynamicsConfig, SpreadHistory};
use rust_decimal::Decimal;
use std::collections::HashSet;
use std::time::{Duration, Instant};

pub use aggregate::{ExchangeQuote, InstrumentSnapshot};
pub use dynamics::SpreadStats;

/// Concurrent store of the freshest book and funding per (exchange, instrument).
/// 24h volume / open interest per (exchange, instrument).
type TickerEntry = (Option<Decimal>, Option<Decimal>);

pub struct MarketState {
    books: DashMap<(ExchangeId, Instrument), TopBook>,
    funding: DashMap<(ExchangeId, Instrument), FundingInfo>,
    tickers: DashMap<(ExchangeId, Instrument), TickerEntry>,
    /// Last time *any* update arrived from each exchange — connection liveness.
    last_msg: DashMap<ExchangeId, Instant>,
    history: SpreadHistory,
    staleness: Duration,
    /// Absolute freshness cap for a book on a **live** connection.
    ///
    /// Venues with event-driven feeds (Gate, Bybit, MEXC deltas) send nothing
    /// while a book doesn't change, which is normal for the thin coins this
    /// screener targets. `recv_ts` therefore measures the last *message*, not
    /// the last *change* — an old book on a live connection is still current.
    /// Judging such books by `staleness` alone systematically excluded exactly
    /// the quiet markets we care about. This cap bounds how long "unchanged"
    /// is still believed (frozen/delisted symbols on a live connection must
    /// eventually go stale).
    quiet_book_max: Duration,
}

impl MarketState {
    pub fn new(staleness: Duration) -> Self {
        Self::with_dynamics(staleness, DynamicsConfig::default())
    }

    pub fn with_dynamics(staleness: Duration, dynamics: DynamicsConfig) -> Self {
        Self::with_params(staleness, staleness * 10, dynamics)
    }

    pub fn with_params(
        staleness: Duration,
        quiet_book_max: Duration,
        dynamics: DynamicsConfig,
    ) -> Self {
        MarketState {
            books: DashMap::new(),
            funding: DashMap::new(),
            tickers: DashMap::new(),
            last_msg: DashMap::new(),
            history: SpreadHistory::new(dynamics),
            staleness,
            quiet_book_max: quiet_book_max.max(staleness),
        }
    }

    /// Record a representative net spread sample for an instrument (called by
    /// the drain loop) to feed the rolling dynamics statistics.
    ///
    /// `reference_threshold` should be the caller's **current** client config
    /// (`min_net_spread_pct`) — read fresh on every call so a runtime config
    /// change is reflected on the next sample rather than needing a restart.
    pub fn record_spread(
        &self,
        instrument: &Instrument,
        net: Decimal,
        now: Instant,
        reference_threshold: Decimal,
    ) {
        self.history.record(instrument, net, now, reference_threshold);
    }

    pub fn staleness(&self) -> Duration {
        self.staleness
    }

    /// Apply one update. Returns the instrument that changed (for a book update)
    /// so the caller can trigger a screener recompute for just that instrument.
    pub fn apply(&self, update: MarketUpdate) -> Option<Instrument> {
        self.apply_at(update, Instant::now())
    }

    /// [`apply`](Self::apply) with an explicit clock, for deterministic tests.
    pub fn apply_at(&self, update: MarketUpdate, now: Instant) -> Option<Instrument> {
        // Any inbound message proves the exchange connection is alive, which is
        // what lets old-but-unchanged books stay usable below.
        self.last_msg.insert(update.exchange(), now);
        match update {
            MarketUpdate::Book {
                exchange,
                instrument,
                book,
            } => {
                self.books.insert((exchange, instrument.clone()), book);
                Some(instrument)
            }
            MarketUpdate::Funding {
                exchange,
                instrument,
                rate,
                interval_hours,
                next_ts,
            } => {
                self.funding.insert(
                    (exchange, instrument),
                    FundingInfo {
                        rate,
                        interval_hours,
                        next_ts,
                    },
                );
                None
            }
            MarketUpdate::Ticker {
                exchange,
                instrument,
                quote_volume_24h,
                open_interest,
            } => {
                self.tickers
                    .insert((exchange, instrument), (quote_volume_24h, open_interest));
                None
            }
        }
    }

    /// Collect the fresh, structurally-valid book on each exchange for one
    /// instrument, along with its latest funding. Stale/invalid books are
    /// dropped. `now` is passed in so tests are deterministic.
    pub fn snapshot(&self, instrument: &Instrument, now: Instant) -> InstrumentSnapshot {
        let mut quotes = Vec::new();
        for entry in self.books.iter() {
            let (ex, inst) = entry.key();
            if inst != instrument {
                continue;
            }
            let book = entry.value();
            let age = now.saturating_duration_since(book.recv_ts);
            // A book on a live connection is current until the venue says
            // otherwise — event-driven feeds send nothing while nothing
            // changes. Only a silent connection makes age alone disqualifying.
            let alive = self
                .last_msg
                .get(ex)
                .map(|t| now.saturating_duration_since(*t) <= self.staleness)
                .unwrap_or(false);
            let stale = if alive {
                age > self.quiet_book_max
            } else {
                age > self.staleness
            };
            // Observation age for leg-skew. A live connection makes an
            // unchanged book usable, but its age is capped at `staleness`,
            // **not zeroed** — zeroing made a book quiet for 25s on a live
            // connection look exactly as fresh as one that just ticked,
            // hiding real skew whenever it was paired with an actively
            // trading leg on another venue (the "spread that never existed"
            // case leg-skew exists to catch). Two quiet legs still cap to the
            // same value and read as simultaneous; a fresh leg against a
            // quiet one now shows a real, checkable gap.
            let as_of_age_ms = if alive {
                age.min(self.staleness).as_millis() as u64
            } else {
                age.as_millis() as u64
            };
            let valid = book.is_valid();
            let funding = self
                .funding
                .get(&(*ex, inst.clone()))
                .map(|f| f.clone());
            let (quote_volume_24h, open_interest) = self
                .tickers
                .get(&(*ex, inst.clone()))
                .map(|t| *t)
                .unwrap_or((None, None));
            quotes.push(ExchangeQuote {
                exchange: *ex,
                book: book.clone(),
                funding,
                quote_volume_24h,
                open_interest,
                as_of_age_ms,
                stale,
                valid,
            });
        }
        InstrumentSnapshot {
            instrument: instrument.clone(),
            quotes,
            stats: self.history.stats(instrument, now),
        }
    }

    /// All instruments currently present in the book store.
    pub fn instruments(&self) -> HashSet<Instrument> {
        self.books.iter().map(|e| e.key().1.clone()).collect()
    }

    /// Number of exchanges with a (any) book for `instrument`.
    pub fn coverage(&self, instrument: &Instrument) -> usize {
        self.books
            .iter()
            .filter(|e| &e.key().1 == instrument)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::BookLevel;
    use rust_decimal_macros::dec;

    fn book_at(recv_ts: Instant) -> TopBook {
        TopBook {
            bids: vec![BookLevel::new(dec!(100), dec!(1))],
            asks: vec![BookLevel::new(dec!(101), dec!(1))],
            recv_ts,
            exch_ts: None,
        }
    }

    fn apply_book(state: &MarketState, ex: ExchangeId, inst: &Instrument, at: Instant) {
        state.apply_at(
            MarketUpdate::Book {
                exchange: ex,
                instrument: inst.clone(),
                book: book_at(at),
            },
            at,
        );
    }

    /// The core liveness semantics: an old book stays usable while its
    /// exchange keeps talking (quiet coin on an event-driven feed), goes stale
    /// once the connection falls silent, and is capped by `quiet_book_max`
    /// even on a live connection.
    #[test]
    fn quiet_book_freshness_follows_connection_liveness() {
        let inst = Instrument::perp("XYZ", "USDT");
        let staleness = Duration::from_secs(3);
        let state = MarketState::with_params(
            staleness,
            Duration::from_secs(30),
            DynamicsConfig::default(),
        );
        let t0 = Instant::now();
        apply_book(&state, ExchangeId::Gate, &inst, t0);

        // 10s later the book hasn't changed, but the connection is alive
        // (a heartbeat/other-symbol update arrived 1s ago).
        let t1 = t0 + Duration::from_secs(10);
        state.apply_at(
            MarketUpdate::Ticker {
                exchange: ExchangeId::Gate,
                instrument: Instrument::perp("OTHER", "USDT"),
                quote_volume_24h: None,
                open_interest: None,
            },
            t1 - Duration::from_secs(1),
        );
        let q = &state.snapshot(&inst, t1).quotes[0];
        assert!(!q.stale, "unchanged book on a live connection is current");
        assert_eq!(
            q.as_of_age_ms,
            staleness.as_millis() as u64,
            "usable, but its age is capped at staleness — not zeroed"
        );

        // Connection silent for longer than staleness → the book's own age rules.
        let t2 = t1 + Duration::from_secs(5); // last_msg now 6s ago, book 15s old
        let q = &state.snapshot(&inst, t2).quotes[0];
        assert!(q.stale, "silent connection: book age exceeds staleness");
        assert!(q.as_of_age_ms >= 15_000);

        // Live connection but book older than quiet_book_max → stale anyway
        // (frozen/delisted symbol guard).
        let t3 = t0 + Duration::from_secs(40);
        state.apply_at(
            MarketUpdate::Ticker {
                exchange: ExchangeId::Gate,
                instrument: Instrument::perp("OTHER", "USDT"),
                quote_volume_24h: None,
                open_interest: None,
            },
            t3,
        );
        let q = &state.snapshot(&inst, t3).quotes[0];
        assert!(q.stale, "quiet_book_max caps live-connection freshness");
    }

    /// Regression: a book quiet for ~20s on a live connection used to report
    /// `as_of_age_ms = 0` — the same as a book that ticked 0ms ago. Paired with
    /// an actively-trading leg on another venue, that hid a real ~20s gap
    /// between the two observations, defeating the leg-skew check exactly in
    /// the case it exists for: a fresh price compared against a stale one.
    #[test]
    fn quiet_leg_is_not_reported_as_simultaneous_with_a_fresh_one() {
        let inst = Instrument::perp("XYZ", "USDT");
        let staleness = Duration::from_secs(3);
        let state = MarketState::with_params(
            staleness,
            Duration::from_secs(30),
            DynamicsConfig::default(),
        );
        let t0 = Instant::now();

        // Bybit ticks once, then goes quiet for 20s (connection stays alive
        // via unrelated traffic).
        apply_book(&state, ExchangeId::Bybit, &inst, t0);
        let now = t0 + Duration::from_secs(20);
        state.apply_at(
            MarketUpdate::Ticker {
                exchange: ExchangeId::Bybit,
                instrument: Instrument::perp("OTHER", "USDT"),
                quote_volume_24h: None,
                open_interest: None,
            },
            now,
        );
        // Gate ticks right now — a genuinely fresh, actively-trading book.
        apply_book(&state, ExchangeId::Gate, &inst, now);

        let snap = state.snapshot(&inst, now);
        let bybit = snap.quotes.iter().find(|q| q.exchange == ExchangeId::Bybit).unwrap();
        let gate = snap.quotes.iter().find(|q| q.exchange == ExchangeId::Gate).unwrap();

        assert!(!bybit.stale, "quiet-but-alive book is still usable");
        assert_eq!(gate.as_of_age_ms, 0, "just-updated book is exactly current");
        assert_eq!(
            bybit.as_of_age_ms,
            staleness.as_millis() as u64,
            "quiet leg is capped at staleness, not zeroed"
        );
        // The real gap between them must stay visible to the leg-skew check
        // (default max_leg_skew_ms is 750ms).
        assert!(bybit.as_of_age_ms.abs_diff(gate.as_of_age_ms) > 750);
    }
}
