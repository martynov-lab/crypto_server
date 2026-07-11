//! Core market-data types: exchange ids, book levels, top-of-book, updates.

use crate::instrument::Instrument;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::time::Instant;

/// The eight venues monitored in Phase 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExchangeId {
    Bybit,
    Okx,
    Mexc,
    Bitget,
    Gate,
    Coinex,
    Kucoin,
    Phemex,
}

/// Every supported exchange, for iteration in config/wiring.
pub const ALL_EXCHANGES: [ExchangeId; 8] = [
    ExchangeId::Bybit,
    ExchangeId::Okx,
    ExchangeId::Mexc,
    ExchangeId::Bitget,
    ExchangeId::Gate,
    ExchangeId::Coinex,
    ExchangeId::Kucoin,
    ExchangeId::Phemex,
];

impl ExchangeId {
    pub const fn as_str(&self) -> &'static str {
        match self {
            ExchangeId::Bybit => "bybit",
            ExchangeId::Okx => "okx",
            ExchangeId::Mexc => "mexc",
            ExchangeId::Bitget => "bitget",
            ExchangeId::Gate => "gate",
            ExchangeId::Coinex => "coinex",
            ExchangeId::Kucoin => "kucoin",
            ExchangeId::Phemex => "phemex",
        }
    }
}

impl fmt::Display for ExchangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown exchange id: {0}")]
pub struct UnknownExchange(pub String);

impl FromStr for ExchangeId {
    type Err = UnknownExchange;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "bybit" => Ok(ExchangeId::Bybit),
            "okx" => Ok(ExchangeId::Okx),
            "mexc" => Ok(ExchangeId::Mexc),
            "bitget" => Ok(ExchangeId::Bitget),
            "gate" | "gateio" | "gate.io" => Ok(ExchangeId::Gate),
            "coinex" => Ok(ExchangeId::Coinex),
            "kucoin" => Ok(ExchangeId::Kucoin),
            "phemex" => Ok(ExchangeId::Phemex),
            other => Err(UnknownExchange(other.to_string())),
        }
    }
}

/// A single price level of a book.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: Decimal,
    pub qty: Decimal, // base-asset quantity
}

impl BookLevel {
    pub fn new(price: Decimal, qty: Decimal) -> Self {
        BookLevel { price, qty }
    }

    /// Notional value of this level in quote currency.
    pub fn notional(&self) -> Decimal {
        self.price * self.qty
    }
}

/// Top-N of a single instrument's book on a single exchange.
///
/// `recv_ts` is local monotonic time of receipt — the *only* clock we trust for
/// staleness (exchange clocks drift). `exch_ts` is kept for reference/logging.
#[derive(Debug, Clone)]
pub struct TopBook {
    /// Bids sorted by descending price (best first).
    pub bids: Vec<BookLevel>,
    /// Asks sorted by ascending price (best first).
    pub asks: Vec<BookLevel>,
    pub recv_ts: Instant,
    pub exch_ts: Option<i64>,
}

impl TopBook {
    pub fn best_bid(&self) -> Option<&BookLevel> {
        self.bids.first()
    }

    pub fn best_ask(&self) -> Option<&BookLevel> {
        self.asks.first()
    }

    /// A book is structurally valid if both sides exist, prices are positive,
    /// and the book is not crossed (best_bid <= best_ask).
    pub fn is_valid(&self) -> bool {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => {
                b.price > Decimal::ZERO && a.price > Decimal::ZERO && b.price <= a.price
            }
            _ => false,
        }
    }
}

/// Normalized update flowing from connectors into the ingest manager.
#[derive(Debug, Clone)]
pub enum MarketUpdate {
    Book {
        exchange: ExchangeId,
        instrument: Instrument,
        book: TopBook,
    },
    Funding {
        exchange: ExchangeId,
        instrument: Instrument,
        rate: Decimal,
        /// Funding interval in hours (e.g. 8). Used to annualize the rate.
        interval_hours: Decimal,
        next_ts: i64,
    },
}

impl MarketUpdate {
    pub fn exchange(&self) -> ExchangeId {
        match self {
            MarketUpdate::Book { exchange, .. } => *exchange,
            MarketUpdate::Funding { exchange, .. } => *exchange,
        }
    }

    pub fn instrument(&self) -> &Instrument {
        match self {
            MarketUpdate::Book { instrument, .. } => instrument,
            MarketUpdate::Funding { instrument, .. } => instrument,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::time::Instant;

    #[test]
    fn exchange_roundtrip() {
        for ex in ALL_EXCHANGES {
            assert_eq!(ExchangeId::from_str(ex.as_str()).unwrap(), ex);
        }
        assert_eq!(ExchangeId::from_str("gate.io").unwrap(), ExchangeId::Gate);
        assert!(ExchangeId::from_str("nope").is_err());
    }

    #[test]
    fn book_validity() {
        let now = Instant::now();
        let ok = TopBook {
            bids: vec![BookLevel::new(dec!(100), dec!(1))],
            asks: vec![BookLevel::new(dec!(101), dec!(1))],
            recv_ts: now,
            exch_ts: None,
        };
        assert!(ok.is_valid());

        let crossed = TopBook {
            bids: vec![BookLevel::new(dec!(102), dec!(1))],
            asks: vec![BookLevel::new(dec!(101), dec!(1))],
            recv_ts: now,
            exch_ts: None,
        };
        assert!(!crossed.is_valid());

        let empty = TopBook {
            bids: vec![],
            asks: vec![],
            recv_ts: now,
            exch_ts: None,
        };
        assert!(!empty.is_valid());
    }
}
