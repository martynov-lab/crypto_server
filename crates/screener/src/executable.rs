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

/// Full economics of a fixed long/short venue pair, including the unwind.
///
/// A cross-exchange futures arb is **two** round trips of taker orders: you
/// open (buy the cheap venue's ask, sell the rich venue's bid) and later close
/// (sell the cheap venue's bid, buy back the rich venue's ask). Judging the
/// trade on the entry spread alone understates the cost by two taker fees and
/// ignores the funding carried while the position is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairEconomics {
    /// Entry leg: VWAPs, gross/net entry spread, executable size.
    pub entry: ExecSpread,
    /// Cost of unwinding immediately at the current books, net of exit fees.
    /// Normally negative; the entry edge has to cover it.
    pub out_pct: Decimal,
    /// Expected profit of the whole trade, net of four taker fees and funding.
    pub round_trip_pct: Decimal,
    /// Funding paid (positive) / earned (negative) over the assumed hold.
    pub funding_cost_pct: Decimal,
    /// `round_trip_pct * executable_notional` — edge in quote currency.
    pub expected_profit_quote: Decimal,
}

/// Compute [`PairEconomics`] for going long on the `buy_*` venue and short on
/// the `sell_*` venue.
///
/// `convergence_pct` is the **net-entry-space** spread level the position is
/// expected to unwind at — the instrument's rolling baseline. Passing zero
/// assumes full convergence, which is the optimistic bound.
#[allow(clippy::too_many_arguments)]
pub fn pair_economics(
    buy_asks: &[BookLevel],
    buy_bids: &[BookLevel],
    sell_bids: &[BookLevel],
    sell_asks: &[BookLevel],
    target_q: Decimal,
    fee_buy: Decimal,
    fee_sell: Decimal,
    convergence_pct: Decimal,
    funding_cost_pct: Decimal,
) -> Option<PairEconomics> {
    let entry = executable_spread(buy_asks, sell_bids, target_q, fee_buy, fee_sell)?;
    let fees_one_way = fee_buy + fee_sell;

    // Unwind at the current books, sized to the entry's executable notional so
    // both directions describe the same position.
    let q = entry.executable_notional;
    let out_pct = match (vwap_sell(buy_bids, q), vwap_buy(sell_asks, q)) {
        (Some(long_exit), Some(short_exit)) if short_exit.vwap > Decimal::ZERO => {
            (long_exit.vwap - short_exit.vwap) / short_exit.vwap - fees_one_way
        }
        // Unwind side unknown (one-sided book): fall back to the symmetric
        // assumption that closing costs the same fees as opening.
        _ => -entry.gross_pct - fees_one_way,
    };

    // Entry net already carries one round of fees; the unwind costs another,
    // and the position only realizes what it gains above the convergence level.
    let round_trip_pct = entry.net_pct - convergence_pct - fees_one_way - funding_cost_pct;

    Some(PairEconomics {
        expected_profit_quote: round_trip_pct * entry.executable_notional,
        entry,
        out_pct,
        round_trip_pct,
        funding_cost_pct,
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
    fn round_trip_charges_four_fees() {
        // Long venue: bid 99.9 / ask 100. Short venue: bid 106 / ask 106.1.
        // Entry gross 6%, fees 0.06% x2 per side.
        let e = pair_economics(
            &[lvl(dec!(100), dec!(100))],   // long venue asks
            &[lvl(dec!(99.9), dec!(100))],  // long venue bids
            &[lvl(dec!(106), dec!(100))],   // short venue bids
            &[lvl(dec!(106.1), dec!(100))], // short venue asks
            dec!(1000),
            dec!(0.0006),
            dec!(0.0006),
            dec!(0), // assume full convergence
            dec!(0),
        )
        .unwrap();
        assert_eq!(e.entry.gross_pct, dec!(0.06));
        assert_eq!(e.entry.net_pct, dec!(0.06) - dec!(0.0012));
        // Round trip pays 0.0012 twice.
        assert_eq!(e.round_trip_pct, dec!(0.06) - dec!(0.0024));
        assert!(e.out_pct < dec!(0), "unwinding now must cost money");
        assert_eq!(e.expected_profit_quote, e.round_trip_pct * dec!(1000));
    }

    #[test]
    fn round_trip_turns_negative_on_a_thin_edge() {
        // 0.15% gross edge cannot cover 4 x 0.06% fees.
        let e = pair_economics(
            &[lvl(dec!(100), dec!(100))],
            &[lvl(dec!(99.99), dec!(100))],
            &[lvl(dec!(100.15), dec!(100))],
            &[lvl(dec!(100.16), dec!(100))],
            dec!(1000),
            dec!(0.0006),
            dec!(0.0006),
            dec!(0),
            dec!(0),
        )
        .unwrap();
        assert!(e.entry.net_pct > dec!(0), "entry alone looks profitable");
        assert!(e.round_trip_pct < dec!(0), "round trip is not");
    }

    #[test]
    fn convergence_and_funding_reduce_the_edge() {
        let base = |conv, funding| {
            pair_economics(
                &[lvl(dec!(100), dec!(100))],
                &[lvl(dec!(99.9), dec!(100))],
                &[lvl(dec!(106), dec!(100))],
                &[lvl(dec!(106.1), dec!(100))],
                dec!(1000),
                dec!(0),
                dec!(0),
                conv,
                funding,
            )
            .unwrap()
            .round_trip_pct
        };
        // Exiting at a 1% baseline instead of 0 costs exactly that 1%.
        assert_eq!(base(dec!(0.01), dec!(0)), base(dec!(0), dec!(0)) - dec!(0.01));
        // Funding carry is subtracted too.
        assert_eq!(base(dec!(0), dec!(0.002)), base(dec!(0), dec!(0)) - dec!(0.002));
    }

    #[test]
    fn empty_book_is_none() {
        assert!(vwap_buy(&[], dec!(100)).is_none());
        assert!(executable_spread(&[], &[lvl(dec!(1), dec!(1))], dec!(100), dec!(0), dec!(0)).is_none());
    }
}
