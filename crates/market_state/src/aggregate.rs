//! Read-side views over market state, consumed by the screener.

use crate::dynamics::SpreadStats;
use domain::{Decimal, ExchangeId, FundingInfo, Instrument, TopBook};

/// One exchange's current view of an instrument, with freshness/validity flags
/// already evaluated by [`super::MarketState::snapshot`].
#[derive(Debug, Clone)]
pub struct ExchangeQuote {
    pub exchange: ExchangeId,
    pub book: TopBook,
    pub funding: Option<FundingInfo>,
    /// 24h quote volume (USDT) reported by this venue, if any.
    pub quote_volume_24h: Option<Decimal>,
    /// Open interest (base units) reported by this venue, if any.
    pub open_interest: Option<Decimal>,
    pub stale: bool,
    pub valid: bool,
}

impl ExchangeQuote {
    /// Usable for spread math: fresh and structurally valid.
    pub fn is_usable(&self) -> bool {
        !self.stale && self.valid
    }
}

/// All exchanges' views of a single instrument at one instant.
#[derive(Debug, Clone)]
pub struct InstrumentSnapshot {
    pub instrument: Instrument,
    pub quotes: Vec<ExchangeQuote>,
    /// Rolling spread statistics for this instrument, if any samples exist.
    pub stats: Option<SpreadStats>,
}

impl InstrumentSnapshot {
    /// Only the quotes usable for spread computation.
    pub fn usable(&self) -> impl Iterator<Item = &ExchangeQuote> {
        self.quotes.iter().filter(|q| q.is_usable())
    }

    /// True if at least two exchanges have usable books (needed for a spread).
    pub fn has_pairing(&self) -> bool {
        self.usable().count() >= 2
    }
}
