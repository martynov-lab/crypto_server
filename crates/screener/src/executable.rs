//! Executable (depth-aware) spread math — the core of "real vs mirage".
//!
//! Instead of trusting best-bid/best-ask, we walk the book to a target quote
//! size `Q` and compute the volume-weighted average price actually achievable,
//! then subtract taker fees on both legs. This is what separates a capturable
//! 2–20% edge from a paper one that vanishes past the first tiny level.

use domain::{BookLevel, Decimal};

/// One executed leg: the VWAP achieved and the quote notional actually filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leg {
    pub vwap: Decimal,
    /// Quote notional (USDT) filled — equals target unless the book ran out.
    pub filled_notional: Decimal,
    /// True if the book couldn't supply the full requested notional.
    pub capped: bool,
}

/// Walk `asks` (ascending price) spending up to `target_quote`, buying base.
/// Returns `None` if the book is empty or prices are non-positive.
pub fn vwap_buy(asks: &[BookLevel], target_quote: Decimal) -> Option<Leg> {
    walk(asks, target_quote)
}

/// Walk `bids` (descending price) receiving up to `target_quote`, selling base.
pub fn vwap_sell(bids: &[BookLevel], target_quote: Decimal) -> Option<Leg> {
    walk(bids, target_quote)
}

/// Shared accumulation: consume levels until `target_quote` of notional is
/// reached (last level partial). Works for both sides because in each case we
/// accumulate quote spent/received and base traded, then divide.
fn walk(levels: &[BookLevel], target_quote: Decimal) -> Option<Leg> {
    if target_quote <= Decimal::ZERO {
        return None;
    }
    let mut remaining = target_quote;
    let mut quote_traded = Decimal::ZERO;
    let mut base_traded = Decimal::ZERO;

    for lvl in levels {
        if lvl.price <= Decimal::ZERO || lvl.qty <= Decimal::ZERO {
            continue;
        }
        let level_notional = lvl.price * lvl.qty;
        if level_notional >= remaining {
            // Partial fill of this level completes the target.
            let base_taken = remaining / lvl.price;
            quote_traded += remaining;
            base_traded += base_taken;
            remaining = Decimal::ZERO;
            break;
        } else {
            quote_traded += level_notional;
            base_traded += lvl.qty;
            remaining -= level_notional;
        }
    }

    if base_traded <= Decimal::ZERO {
        return None;
    }
    let capped = remaining > Decimal::ZERO;
    Some(Leg {
        vwap: quote_traded / base_traded,
        filled_notional: quote_traded,
        capped,
    })
}

/// Full executable spread result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSpread {
    pub vwap_buy: Decimal,
    pub vwap_sell: Decimal,
    pub gross_pct: Decimal,
    pub net_pct: Decimal,
    /// Quote notional both legs were evaluated at (the smaller achievable size).
    pub executable_notional: Decimal,
    pub capped_by_depth: bool,
}

/// Compute the executable spread of buying on `buy_asks` and selling on
/// `sell_bids` for `target_q` quote, net of the two taker fees.
///
/// If one leg is thinner, both legs are re-evaluated at the smaller achievable
/// notional so the two VWAPs correspond to the *same* executable size.
pub fn executable_spread(
    buy_asks: &[BookLevel],
    sell_bids: &[BookLevel],
    target_q: Decimal,
    fee_buy: Decimal,
    fee_sell: Decimal,
) -> Option<ExecSpread> {
    let buy = vwap_buy(buy_asks, target_q)?;
    let sell = vwap_sell(sell_bids, target_q)?;

    let executable = buy.filled_notional.min(sell.filled_notional);
    if executable <= Decimal::ZERO {
        return None;
    }

    // Re-evaluate both legs at the common achievable size when capped.
    let (vwap_buy, vwap_sell) = if executable < target_q {
        let b = vwap_buy(buy_asks, executable)?;
        let s = vwap_sell(sell_bids, executable)?;
        (b.vwap, s.vwap)
    } else {
        (buy.vwap, sell.vwap)
    };

    let gross_pct = (vwap_sell - vwap_buy) / vwap_buy;
    let net_pct = gross_pct - fee_buy - fee_sell;

    Some(ExecSpread {
        vwap_buy,
        vwap_sell,
        gross_pct,
        net_pct,
        executable_notional: executable,
        capped_by_depth: buy.capped || sell.capped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn lvl(p: Decimal, q: Decimal) -> BookLevel {
        BookLevel::new(p, q)
    }

    #[test]
    fn vwap_single_level_partial() {
        // One level 100 x 10 (notional 1000). Buy 500 => vwap 100, not capped.
        let asks = vec![lvl(dec!(100), dec!(10))];
        let leg = vwap_buy(&asks, dec!(500)).unwrap();
        assert_eq!(leg.vwap, dec!(100));
        assert_eq!(leg.filled_notional, dec!(500));
        assert!(!leg.capped);
    }

    #[test]
    fn vwap_multi_level_weighted() {
        // 100x1 (100) + 110x10 (1100). Buy 200 => 100 from L1, 100 from L2.
        // base = 1 + 100/110 = 1.9090909...; vwap = 200 / 1.909.. ≈ 104.7619
        let asks = vec![lvl(dec!(100), dec!(1)), lvl(dec!(110), dec!(10))];
        let leg = vwap_buy(&asks, dec!(200)).unwrap();
        assert_eq!(leg.filled_notional, dec!(200));
        assert!(!leg.capped);
        assert!(leg.vwap > dec!(104) && leg.vwap < dec!(105));
    }

    #[test]
    fn vwap_capped_when_thin() {
        // Only 300 of notional available; asking for 1000 => capped at 300.
        let asks = vec![lvl(dec!(100), dec!(3))];
        let leg = vwap_buy(&asks, dec!(1000)).unwrap();
        assert_eq!(leg.filled_notional, dec!(300));
        assert!(leg.capped);
    }

    #[test]
    fn executable_spread_net_of_fees() {
        // Buy at 100, sell at 110 => gross 10%. Fees 0.06% + 0.06% => net ~9.88%.
        let asks = vec![lvl(dec!(100), dec!(100))];
        let bids = vec![lvl(dec!(110), dec!(100))];
        let s = executable_spread(&asks, &bids, dec!(1000), dec!(0.0006), dec!(0.0006)).unwrap();
        assert_eq!(s.gross_pct, dec!(0.10));
        assert_eq!(s.net_pct, dec!(0.10) - dec!(0.0012));
        assert!(!s.capped_by_depth);
        assert_eq!(s.executable_notional, dec!(1000));
    }

    #[test]
    fn executable_uses_thinner_leg_size() {
        // Buy side deep (10_000), sell side only 500 => executable capped at 500.
        let asks = vec![lvl(dec!(100), dec!(100))];
        let bids = vec![lvl(dec!(110), dec!(5))]; // 110*5 = 550 notional at that price
        let s = executable_spread(&asks, &bids, dec!(1000), dec!(0), dec!(0)).unwrap();
        assert!(s.capped_by_depth);
        assert!(s.executable_notional <= dec!(550));
        assert_eq!(s.gross_pct, dec!(0.10));
    }

    #[test]
    fn empty_book_is_none() {
        assert!(vwap_buy(&[], dec!(100)).is_none());
        assert!(executable_spread(&[], &[lvl(dec!(1), dec!(1))], dec!(100), dec!(0), dec!(0)).is_none());
    }
}
