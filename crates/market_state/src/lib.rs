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
    history: SpreadHistory,
    staleness: Duration,
}

impl MarketState {
    pub fn new(staleness: Duration) -> Self {
        Self::with_dynamics(staleness, DynamicsConfig::default())
    }

    pub fn with_dynamics(staleness: Duration, dynamics: DynamicsConfig) -> Self {
        MarketState {
            books: DashMap::new(),
            funding: DashMap::new(),
            tickers: DashMap::new(),
            history: SpreadHistory::new(dynamics),
            staleness,
        }
    }

    /// Record a representative net spread sample for an instrument (called by
    /// the drain loop) to feed the rolling dynamics statistics.
    pub fn record_spread(&self, instrument: &Instrument, net: Decimal, now: Instant) {
        self.history.record(instrument, net, now);
    }

    pub fn staleness(&self) -> Duration {
        self.staleness
    }

    /// Apply one update. Returns the instrument that changed (for a book update)
    /// so the caller can trigger a screener recompute for just that instrument.
    pub fn apply(&self, update: MarketUpdate) -> Option<Instrument> {
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
            let stale = age > self.staleness;
            let valid = book.is_valid();
            let funding = self
                .funding
                .get(&(*ex, inst.clone()))
                .map(|f| f.clone());
            let (quote_volume_24h, open_interest) = self
                .tickers
                .get(&(*ex, inst.clone()))
                .map(|t| t.clone())
                .unwrap_or((None, None));
            quotes.push(ExchangeQuote {
                exchange: *ex,
                book: book.clone(),
                funding,
                quote_volume_24h,
                open_interest,
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
