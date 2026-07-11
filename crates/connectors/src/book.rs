//! Reusable L2 book maintainer for venues that stream snapshot + incremental
//! deltas. Connectors that instead receive a *full* top-N snapshot every tick
//! (e.g. OKX `books5`) don't need this — they build `TopBook` directly.

use domain::{BookLevel, Decimal};
use std::collections::BTreeMap;

/// Maintains full bid/ask maps and can emit the top-N view. `qty == 0` in a
/// delta removes the level (standard exchange convention).
#[derive(Debug, Default, Clone)]
pub struct DeltaBook {
    bids: BTreeMap<Decimal, Decimal>, // price -> qty
    asks: BTreeMap<Decimal, Decimal>,
}

impl DeltaBook {
    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
    }

    /// Replace the whole book from a snapshot.
    pub fn apply_snapshot(&mut self, bids: &[BookLevel], asks: &[BookLevel]) {
        self.clear();
        self.apply_delta(bids, asks);
    }

    /// Apply incremental changes; qty 0 deletes the level.
    pub fn apply_delta(&mut self, bids: &[BookLevel], asks: &[BookLevel]) {
        for lvl in bids {
            if lvl.qty.is_zero() {
                self.bids.remove(&lvl.price);
            } else {
                self.bids.insert(lvl.price, lvl.qty);
            }
        }
        for lvl in asks {
            if lvl.qty.is_zero() {
                self.asks.remove(&lvl.price);
            } else {
                self.asks.insert(lvl.price, lvl.qty);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() || self.asks.is_empty()
    }

    /// Top-N view: bids descending, asks ascending (both best-first).
    pub fn top_n(&self, n: usize) -> (Vec<BookLevel>, Vec<BookLevel>) {
        let bids = self
            .bids
            .iter()
            .rev()
            .take(n)
            .map(|(p, q)| BookLevel::new(*p, *q))
            .collect();
        let asks = self
            .asks
            .iter()
            .take(n)
            .map(|(p, q)| BookLevel::new(*p, *q))
            .collect();
        (bids, asks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn lvl(p: Decimal, q: Decimal) -> BookLevel {
        BookLevel::new(p, q)
    }

    #[test]
    fn snapshot_then_delta_updates_and_deletes() {
        let mut b = DeltaBook::default();
        b.apply_snapshot(
            &[lvl(dec!(100), dec!(1)), lvl(dec!(99), dec!(2))],
            &[lvl(dec!(101), dec!(1)), lvl(dec!(102), dec!(3))],
        );
        let (bids, asks) = b.top_n(5);
        assert_eq!(bids[0].price, dec!(100));
        assert_eq!(asks[0].price, dec!(101));

        // Delta: remove best bid (qty 0), update best ask qty, add new bid.
        b.apply_delta(
            &[lvl(dec!(100), dec!(0)), lvl(dec!(98), dec!(5))],
            &[lvl(dec!(101), dec!(9))],
        );
        let (bids, asks) = b.top_n(5);
        assert_eq!(bids[0].price, dec!(99));
        assert_eq!(bids[1].price, dec!(98));
        assert_eq!(asks[0].price, dec!(101));
        assert_eq!(asks[0].qty, dec!(9));
    }
}
