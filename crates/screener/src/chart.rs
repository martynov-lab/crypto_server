//! Chart computations for the fixed-pair In/Out spread view.
//!
//! `venue_sample` snapshots every venue's VWAP quotes (to the target size) so the
//! tape can later derive In/Out for *any* long/short pair — including on backfill.
//! `chart_point` turns a sample into an In/Out point for one fixed pair;
//! `best_pair` picks the pair with the widest entry spread (used when the client
//! doesn't pin a pair).

use crate::config::ClientConfig;
use crate::executable::{vwap_buy, vwap_sell};
use domain::{ChartPoint, Decimal, ExchangeId, VenueQuote, VenueSample};
use market_state::InstrumentSnapshot;

/// Snapshot per-venue VWAP quotes (buy/sell to `target_notional_q`) plus funding.
/// Returns `None` if fewer than two enabled venues have usable books.
pub fn venue_sample(
    snapshot: &InstrumentSnapshot,
    cfg: &ClientConfig,
    ts_ms: i64,
) -> Option<VenueSample> {
    let q = cfg.target_notional_q;
    let mut venues = Vec::new();
    for quote in snapshot.usable().filter(|v| cfg.includes(v.exchange)) {
        let (Some(ask), Some(bid)) = (
            vwap_buy(&quote.book.asks, q),
            vwap_sell(&quote.book.bids, q),
        ) else {
            continue;
        };
        venues.push(VenueQuote {
            exchange: quote.exchange,
            vwap_ask: ask.vwap,
            vwap_bid: bid.vwap,
            ask_notional: ask.filled_notional,
            bid_notional: bid.filled_notional,
            ask_capped: ask.capped,
            bid_capped: bid.capped,
            funding_rate: quote.funding.as_ref().map(|f| f.rate),
            funding_interval_hours: quote.funding.as_ref().map(|f| f.interval_hours),
            next_funding_ms: quote.funding.as_ref().map(|f| f.next_ts),
        });
    }
    if venues.len() < 2 {
        return None;
    }
    Some(VenueSample {
        ts_ms,
        baseline_pct: snapshot.stats.as_ref().map(|s| s.baseline_pct),
        venues,
    })
}

fn find<'a>(sample: &'a VenueSample, ex: ExchangeId) -> Option<&'a VenueQuote> {
    sample.venues.iter().find(|v| v.exchange == ex)
}

/// Build an In/Out chart point for the fixed `long`/`short` pair. `None` if
/// either venue is absent from this sample (a gap in the line).
pub fn chart_point(
    sample: &VenueSample,
    long: ExchangeId,
    short: ExchangeId,
    cfg: &ClientConfig,
) -> Option<ChartPoint> {
    if long == short {
        return None;
    }
    let l = find(sample, long)?;
    let s = find(sample, short)?;
    if l.vwap_ask <= Decimal::ZERO || s.vwap_ask <= Decimal::ZERO {
        return None;
    }
    let fee = cfg.taker(long) + cfg.taker(short);

    // Entry: buy long-leg at its ask, sell short-leg at its bid.
    let in_net = (s.vwap_bid - l.vwap_ask) / l.vwap_ask - fee;
    // Exit: sell long-leg at its bid, buy short-leg at its ask.
    let out_net = (l.vwap_bid - s.vwap_ask) / s.vwap_ask - fee;

    Some(ChartPoint {
        ts_ms: sample.ts_ms,
        net_pct: in_net,
        in_pct: in_net,
        out_pct: out_net,
        baseline_pct: sample.baseline_pct,
        buy_exchange: long,
        sell_exchange: short,
        executable_notional: l.ask_notional.min(s.bid_notional),
        capped_by_depth: l.ask_capped || s.bid_capped,
        funding_long_pct: l.funding_rate,
        funding_short_pct: s.funding_rate,
    })
}

/// Pick the pair with the widest entry (In) spread — used when the client opens a
/// chart without pinning a pair.
pub fn best_pair(sample: &VenueSample, cfg: &ClientConfig) -> Option<(ExchangeId, ExchangeId)> {
    let mut best: Option<(ExchangeId, ExchangeId, Decimal)> = None;
    for l in &sample.venues {
        for s in &sample.venues {
            if l.exchange == s.exchange || l.vwap_ask <= Decimal::ZERO {
                continue;
            }
            let in_net = (s.vwap_bid - l.vwap_ask) / l.vwap_ask
                - cfg.taker(l.exchange)
                - cfg.taker(s.exchange);
            if best.map_or(true, |(_, _, v)| in_net > v) {
                best = Some((l.exchange, s.exchange, in_net));
            }
        }
    }
    best.map(|(l, s, _)| (l, s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{BookLevel, Instrument, TopBook};
    use market_state::{ExchangeQuote, InstrumentSnapshot};
    use rust_decimal_macros::dec;
    use std::time::Instant;

    fn book(bid: Decimal, ask: Decimal) -> TopBook {
        TopBook {
            bids: vec![BookLevel::new(bid, dec!(1000))],
            asks: vec![BookLevel::new(ask, dec!(1000))],
            recv_ts: Instant::now(),
            exch_ts: None,
        }
    }

    fn quote(ex: ExchangeId, bid: Decimal, ask: Decimal) -> ExchangeQuote {
        ExchangeQuote {
            exchange: ex,
            book: book(bid, ask),
            funding: None,
            quote_volume_24h: None,
            open_interest: None,
            as_of_age_ms: 0,
            stale: false,
            valid: true,
        }
    }

    fn cfg() -> ClientConfig {
        let mut c = ClientConfig::default();
        // Zero fees to make the arithmetic exact in the test.
        for ex in domain::ALL_EXCHANGES {
            c.taker_fee.insert(ex, dec!(0));
        }
        c
    }

    #[test]
    fn in_out_signs_are_consistent() {
        // gate cheap (ask 100), kucoin rich (bid 106). Long gate, short kucoin.
        let snap = InstrumentSnapshot {
            instrument: Instrument::perp("XYZ", "USDT"),
            quotes: vec![
                quote(ExchangeId::Gate, dec!(99.9), dec!(100)),
                quote(ExchangeId::Kucoin, dec!(106), dec!(106.1)),
            ],
            stats: None,
        };
        let sample = venue_sample(&snap, &cfg(), 1).unwrap();
        let p = chart_point(&sample, ExchangeId::Gate, ExchangeId::Kucoin, &cfg()).unwrap();
        // In: (106 - 100)/100 = 0.06
        assert_eq!(p.in_pct, dec!(0.06));
        // Out: (99.9 - 106.1)/106.1 < 0
        assert!(p.out_pct < dec!(0));
        assert_eq!(p.net_pct, p.in_pct);

        let (long, short) = best_pair(&sample, &cfg()).unwrap();
        assert_eq!((long, short), (ExchangeId::Gate, ExchangeId::Kucoin));
    }

    #[test]
    fn missing_leg_is_gap() {
        let snap = InstrumentSnapshot {
            instrument: Instrument::perp("XYZ", "USDT"),
            quotes: vec![
                quote(ExchangeId::Gate, dec!(99.9), dec!(100)),
                quote(ExchangeId::Kucoin, dec!(106), dec!(106.1)),
            ],
            stats: None,
        };
        let sample = venue_sample(&snap, &cfg(), 1).unwrap();
        // Bybit isn't in the sample → no point.
        assert!(chart_point(&sample, ExchangeId::Bybit, ExchangeId::Gate, &cfg()).is_none());
    }
}
