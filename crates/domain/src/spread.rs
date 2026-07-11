//! Result types for the spread computation.

use crate::instrument::Instrument;
use crate::types::ExchangeId;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// The best cross-exchange pairing for one instrument, with executable economics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spread {
    pub instrument: Instrument,
    /// Venue with the lowest ask — where you would buy.
    pub buy_exchange: ExchangeId,
    /// Venue with the highest bid — where you would sell.
    pub sell_exchange: ExchangeId,
    /// Executable VWAP buy price for `executable_notional`.
    pub vwap_buy: Decimal,
    /// Executable VWAP sell price for `executable_notional`.
    pub vwap_sell: Decimal,
    /// (vwap_sell - vwap_buy) / vwap_buy, before fees.
    pub gross_pct: Decimal,
    /// gross - taker_fee_buy - taker_fee_sell.
    pub net_pct: Decimal,
    /// Quote notional (USDT) the spread was actually computed over. May be less
    /// than the requested target size when the book is thin.
    pub executable_notional: Decimal,
    /// True when the book could not supply the full target size on one/both legs.
    pub capped_by_depth: bool,
}

/// One raw sample of an instrument's best executable spread at an instant, used
/// to drive the real-time chart. Computed at a fixed cadence with default/shared
/// fees and **decoupled from the alert engine** (no hysteresis/cooldown).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpreadPoint {
    /// Epoch milliseconds of the sample.
    pub ts_ms: i64,
    /// Best net-of-fees spread (primary chart line).
    pub net_pct: Decimal,
    /// Gross spread before fees (optional secondary line).
    pub gross_pct: Decimal,
    /// Rolling median baseline (reference band), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_pct: Option<Decimal>,
    /// Venues forming the best spread at this instant (can change over time).
    pub buy_exchange: ExchangeId,
    pub sell_exchange: ExchangeId,
    /// Executable notional (USDT) available on both legs right now.
    pub executable_notional: Decimal,
    /// True when the book can't supply the full target size (thinner entry).
    pub capped_by_depth: bool,
}

/// Why a candidate spread was surfaced or rejected — attached to events for
/// client-side explanation and for lifetime/analysis logging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpreadReason {
    /// Passed all client filters and hysteresis — an actionable signal.
    Signal,
    /// Net spread above threshold but only BBO depth known (size unknown).
    BboOnly,
    BelowMinSpread,
    AboveMaxSpread,
    InsufficientDepth,
    NotTransferable,
    NoCommonNetwork,
    StaleBook,
    BelowMinVolume,
    BelowMinOpenInterest,
    /// Baseline spread is persistently wide — structural break, not opportunity.
    PersistentWide,
    /// Current spread isn't a genuine outlier vs its own baseline.
    NotASpike,
    /// Spread has stayed wide too long — likely a trap that never converges.
    TooPersistent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_serializes_snake_case() {
        let j = serde_json::to_string(&SpreadReason::NoCommonNetwork).unwrap();
        assert_eq!(j, "\"no_common_network\"");
    }
}
