//! Canonical instrument identity, shared across every exchange.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Market type. Phase 1 is perp-only, but the enum is kept open for spot later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarketKind {
    Spot,
    Perp,
}

impl fmt::Display for MarketKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarketKind::Spot => f.write_str("spot"),
            MarketKind::Perp => f.write_str("perp"),
        }
    }
}

/// A canonical instrument, e.g. `BTC/USDT perp`.
///
/// `base`/`quote` are stored upper-cased so `Instrument` compares and hashes
/// consistently regardless of the exchange's casing conventions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Instrument {
    pub base: String,  // "BTC"
    pub quote: String, // "USDT"
    pub kind: MarketKind,
}

impl Instrument {
    pub fn perp(base: impl Into<String>, quote: impl Into<String>) -> Self {
        Instrument {
            base: base.into().to_uppercase(),
            quote: quote.into().to_uppercase(),
            kind: MarketKind::Perp,
        }
    }

    pub fn spot(base: impl Into<String>, quote: impl Into<String>) -> Self {
        Instrument {
            base: base.into().to_uppercase(),
            quote: quote.into().to_uppercase(),
            kind: MarketKind::Spot,
        }
    }
}

impl fmt::Display for Instrument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{} {}", self.base, self.quote, self.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_quote_are_uppercased() {
        let i = Instrument::perp("btc", "usdt");
        assert_eq!(i.base, "BTC");
        assert_eq!(i.quote, "USDT");
        assert_eq!(i.kind, MarketKind::Perp);
    }

    #[test]
    fn equal_instruments_hash_equal() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(Instrument::perp("BTC", "USDT"));
        assert!(set.contains(&Instrument::perp("btc", "usdt")));
    }
}
