//! Funding-rate snapshot for a perp instrument on one exchange.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Current funding for a perp. Rate is the per-interval fraction (e.g. 0.0001 = 1bp
/// per 8h). `interval_hours` lets the screener annualize consistently across venues
/// that use different funding cadences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingInfo {
    pub rate: Decimal,
    pub interval_hours: Decimal,
    /// Exchange timestamp (ms) of the next funding settlement.
    pub next_ts: i64,
}

impl FundingInfo {
    /// Annualized funding rate = per-interval rate * intervals-per-year.
    /// Guards against a zero/invalid interval by returning the raw rate.
    pub fn annualized(&self) -> Decimal {
        if self.interval_hours <= Decimal::ZERO {
            return self.rate;
        }
        let intervals_per_year = Decimal::from(24 * 365) / self.interval_hours;
        self.rate * intervals_per_year
    }

    /// Funding paid (positive) or received (negative) by a **long** position held
    /// for `hours`, as a fraction of notional. Assumes the current rate persists
    /// across the hold — the best estimate available at signal time.
    pub fn cost_over(&self, hours: Decimal) -> Decimal {
        if self.interval_hours <= Decimal::ZERO {
            return self.rate;
        }
        self.rate * (hours / self.interval_hours)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn annualizes_8h_funding() {
        let f = FundingInfo {
            rate: dec!(0.0001),
            interval_hours: dec!(8),
            next_ts: 0,
        };
        // 0.0001 * (8760 / 8) = 0.0001 * 1095 = 0.1095
        assert_eq!(f.annualized(), dec!(0.1095));
    }

    #[test]
    fn cost_scales_with_hold() {
        let f = FundingInfo {
            rate: dec!(0.0001),
            interval_hours: dec!(8),
            next_ts: 0,
        };
        // 24h hold over an 8h interval = 3 payments.
        assert_eq!(f.cost_over(dec!(24)), dec!(0.0003));
    }

    #[test]
    fn zero_interval_is_safe() {
        let f = FundingInfo {
            rate: dec!(0.0001),
            interval_hours: dec!(0),
            next_ts: 0,
        };
        assert_eq!(f.annualized(), dec!(0.0001));
    }
}
