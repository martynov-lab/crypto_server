//! Funding-rate differential signal (spec §8.3) — a latency-tolerant perp
//! strategy: long the venue with the lowest funding, short the highest, and
//! collect the annualized differential while holding a delta-neutral pair.

use domain::{Decimal, ExchangeId, FundingInfo};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingSignal {
    /// Go long here (lowest funding — you pay least / receive most).
    pub long_exchange: ExchangeId,
    /// Go short here (highest funding — shorts receive it).
    pub short_exchange: ExchangeId,
    /// Annualized funding differential captured by the pair.
    pub diff_apr: Decimal,
}

/// Best funding differential across the venues quoting this perp. Needs at least
/// two funding quotes on different exchanges.
pub fn best_funding_diff(
    quotes: &[(ExchangeId, FundingInfo)],
) -> Option<FundingSignal> {
    if quotes.len() < 2 {
        return None;
    }
    let mut lowest: Option<(ExchangeId, Decimal)> = None;
    let mut highest: Option<(ExchangeId, Decimal)> = None;

    for (ex, f) in quotes {
        let ann = f.annualized();
        if lowest.map_or(true, |(_, v)| ann < v) {
            lowest = Some((*ex, ann));
        }
        if highest.map_or(true, |(_, v)| ann > v) {
            highest = Some((*ex, ann));
        }
    }

    let (long_ex, low) = lowest?;
    let (short_ex, high) = highest?;
    if long_ex == short_ex {
        return None; // all equal / single distinct venue
    }
    Some(FundingSignal {
        long_exchange: long_ex,
        short_exchange: short_ex,
        diff_apr: high - low,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn f(rate: Decimal) -> FundingInfo {
        FundingInfo {
            rate,
            interval_hours: dec!(8),
            next_ts: 0,
        }
    }

    #[test]
    fn picks_extremes_and_annualizes() {
        let quotes = vec![
            (ExchangeId::Bybit, f(dec!(0.0001))),   // ann 0.1095
            (ExchangeId::Okx, f(dec!(-0.0002))),    // ann -0.219
            (ExchangeId::Mexc, f(dec!(0.00005))),   // ann 0.05475
        ];
        let s = best_funding_diff(&quotes).unwrap();
        assert_eq!(s.long_exchange, ExchangeId::Okx); // lowest funding
        assert_eq!(s.short_exchange, ExchangeId::Bybit); // highest funding
        // diff = 0.1095 - (-0.219) = 0.3285
        assert_eq!(s.diff_apr, dec!(0.3285));
    }

    #[test]
    fn single_quote_is_none() {
        let quotes = vec![(ExchangeId::Bybit, f(dec!(0.0001)))];
        assert!(best_funding_diff(&quotes).is_none());
    }
}
