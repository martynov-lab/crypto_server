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
    /// Entry spread only: gross - taker_fee_buy - taker_fee_sell. Kept for
    /// backward compatibility — **not** the profit of the trade, see
    /// `round_trip_pct`.
    pub net_pct: Decimal,
    /// Cost of unwinding *right now* at the current books, net of the exit
    /// taker fees (sell the long leg's bid, buy back the short leg's ask).
    /// Normally negative — the entry edge is what has to cover it.
    pub out_pct: Decimal,
    /// Expected profit of the full round trip: enter now, unwind when the
    /// spread converges to its rolling baseline. Nets **four** taker fees and
    /// the expected funding carry. This is the number worth trading on.
    pub round_trip_pct: Decimal,
    /// Expected funding paid (positive) or earned (negative) over the assumed
    /// hold, already included in `round_trip_pct`.
    pub funding_cost_pct: Decimal,
    /// `round_trip_pct * executable_notional` — the edge in quote currency,
    /// which is what pairs are actually ranked by.
    pub expected_profit_quote: Decimal,
    /// Age difference between the two legs' books (ms). A large skew means the
    /// two sides were observed at different times and the spread is an artifact.
    pub leg_skew_ms: u64,
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

/// Per-venue VWAP snapshot for one exchange at one sample instant. Stored in the
/// spread tape so the chart can derive In/Out for **any** fixed pair on backfill,
/// not just the best pair at each tick.
#[derive(Debug, Clone)]
pub struct VenueQuote {
    pub exchange: ExchangeId,
    /// VWAP to buy the target size (walk asks).
    pub vwap_ask: Decimal,
    /// VWAP to sell the target size (walk bids).
    pub vwap_bid: Decimal,
    /// Quote notional actually fillable on each side (for entry/exit depth).
    pub ask_notional: Decimal,
    pub bid_notional: Decimal,
    pub ask_capped: bool,
    pub bid_capped: bool,
    /// Latest funding for this leg, if known (per-interval rate).
    pub funding_rate: Option<Decimal>,
    pub funding_interval_hours: Option<Decimal>,
    pub next_funding_ms: Option<i64>,
}

/// All venues' VWAP quotes for one instrument at one sample instant.
#[derive(Debug, Clone)]
pub struct VenueSample {
    pub ts_ms: i64,
    /// Rolling median baseline (best-pair dynamics) — reference band.
    pub baseline_pct: Option<Decimal>,
    pub venues: Vec<VenueQuote>,
}

/// One chart point for a **fixed** long/short pair: entry (In) and exit (Out)
/// executable spreads plus per-leg funding. `net_pct` mirrors `in_pct` for
/// backward compatibility with the single-line chart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChartPoint {
    pub ts_ms: i64,
    /// Legacy single-line value; equals `in_pct`.
    pub net_pct: Decimal,
    /// Entry spread (buy long-leg ask, sell short-leg bid), net of fees. Green line.
    pub in_pct: Decimal,
    /// Exit spread (sell long-leg bid, buy short-leg ask), net of fees. Red line.
    pub out_pct: Decimal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_pct: Option<Decimal>,
    /// Long leg (where you buy to open).
    pub buy_exchange: ExchangeId,
    /// Short leg (where you sell to open).
    pub sell_exchange: ExchangeId,
    /// Entry-side executable notional (min of both legs).
    pub executable_notional: Decimal,
    /// Entry-side depth cap flag.
    pub capped_by_depth: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funding_long_pct: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funding_short_pct: Option<Decimal>,
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
    /// 24h volume above the client's ceiling — outside the low-cap band.
    AboveMaxVolume,
    BelowMinOpenInterest,
    /// Baseline spread is persistently wide — structural break, not opportunity.
    PersistentWide,
    /// Current spread isn't a genuine outlier vs its own baseline.
    NotASpike,
    /// Spread has stayed wide too long — likely a trap that never converges.
    TooPersistent,
    /// The two legs' books were observed too far apart in time — the spread is
    /// a timing artifact, not a simultaneous quote.
    LegSkew,
    /// Entry edge does not cover the round trip (four taker fees + funding
    /// carry + expected convergence level).
    NegativeRoundTrip,
    /// One leg's mid price is far from the cross-venue median — wrong token,
    /// stale quote, or a redenomination, not an arb.
    PriceOutlier,
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
