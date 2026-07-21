//! Client-filter evaluation over an instrument snapshot: find the best
//! executable cross-exchange pairing and decide whether it passes the client's
//! band, depth, and transferability filters.

use crate::config::ClientConfig;
use crate::executable::{executable_spread, pair_economics, ExecSpread, PairEconomics};
use crate::funding::{best_funding_diff, FundingSignal};
use domain::transfer::has_common_transfer_route;
use domain::{Decimal, ExchangeId, Spread, SpreadReason, TransferStatus};
use market_state::{ExchangeQuote, InstrumentSnapshot, SpreadStats};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use std::time::Instant;

/// Read-only source of transfer status, implemented by the `transfer_status`
/// crate. Kept as a trait so the screener has no dependency cycle and tests can
/// inject fixtures.
pub trait TransferOracle: Send + Sync {
    fn status(&self, exchange: ExchangeId, asset: &str) -> Option<TransferStatus>;
}

/// Oracle that knows nothing — every transfer check fails closed.
pub struct NoTransferInfo;
impl TransferOracle for NoTransferInfo {
    fn status(&self, _exchange: ExchangeId, _asset: &str) -> Option<TransferStatus> {
        None
    }
}

/// Outcome of evaluating one instrument for one client.
#[derive(Debug, Clone)]
pub struct Evaluation {
    pub spread: Spread,
    pub reason: SpreadReason,
    pub funding: Option<FundingSignal>,
    /// Rolling spread statistics for the instrument (if available).
    pub stats: Option<SpreadStats>,
    /// 0–100 arb-quality score (tight baseline + strong spike + brief + covered).
    pub quality_score: Option<Decimal>,
}

/// Evaluate the best pairing for `snapshot` under `cfg`. Returns `None` only
/// when there is no candidate at all (symbol filtered out, or <2 usable venues).
/// Otherwise returns the best pairing with `reason` = `Signal` when it passes
/// every filter, or the specific reject reason for logging/analysis.
pub fn evaluate(
    snapshot: &InstrumentSnapshot,
    cfg: &ClientConfig,
    oracle: &dyn TransferOracle,
) -> Option<Evaluation> {
    if !cfg.allows_symbol(&snapshot.instrument.base) {
        return None;
    }
    // Market-kind filter. A snapshot covers one instrument, so both legs share
    // its kind (perp/perp today, spot/spot once spot ingestion lands). Mixed
    // pairs (spot/perp) will need cross-instrument snapshots and are checked
    // here the same way when that lands.
    let kind = snapshot.instrument.kind;
    if !cfg.allows_market_pair(kind, kind) {
        return None;
    }

    // Usable, client-enabled quotes only.
    let all_quotes: Vec<_> = snapshot
        .usable()
        .filter(|q| cfg.includes(q.exchange))
        .collect();
    if all_quotes.len() < 2 {
        return None;
    }

    // Drop venues quoting a price far from the cross-venue consensus. A wrong
    // token, a frozen market, or a post-redenomination listing shows up as a
    // huge "spread" that passes every economic filter, so it has to be removed
    // before pairing rather than capped afterwards.
    let quotes = drop_price_outliers(&all_quotes, cfg);
    if quotes.len() < 2 {
        // A pairing existed until consensus filtering removed it — report that
        // explicitly instead of going silent, so the rejection is visible.
        return outlier_rejection(snapshot, &all_quotes, cfg);
    }

    // The level the position is expected to unwind at: the instrument's rolling
    // baseline. Clamped at zero — assuming the spread overshoots into negative
    // territory in our favor is not something to bank on.
    let convergence = snapshot
        .stats
        .as_ref()
        .map(|s| s.baseline_pct.max(Decimal::ZERO))
        .unwrap_or(Decimal::ZERO);

    // Brute-force best pairing (venue count is small). Ranked by expected
    // profit **in quote currency**, not percent: a 5% edge on $50 of depth is
    // worth less than a 2% edge on $2000, and ranking by percent used to hand
    // the tradable pair's slot to an untradable one.
    let mut best_qualified: Option<Candidate> = None;
    let mut best_any: Option<Candidate> = None;
    for (i, buy) in quotes.iter().enumerate() {
        for (j, sell) in quotes.iter().enumerate() {
            if i == j || buy.exchange == sell.exchange {
                continue;
            }
            let funding_cost = funding_cost_pct(cfg, buy, sell);
            let Some(econ) = pair_economics(
                &buy.book.asks,
                &buy.book.bids,
                &sell.book.bids,
                &sell.book.asks,
                cfg.target_notional_q,
                cfg.taker(buy.exchange),
                cfg.taker(sell.exchange),
                convergence,
                funding_cost,
            ) else {
                continue;
            };
            let cand = Candidate { buy: i, sell: j, econ };
            let better = |slot: &Option<Candidate>| {
                slot.as_ref().map_or(true, |b| {
                    cand.econ.expected_profit_quote > b.econ.expected_profit_quote
                })
            };
            if cand.econ.entry.executable_notional >= cfg.min_executable_notional
                && better(&best_qualified)
            {
                best_qualified = Some(cand.clone());
            }
            if better(&best_any) {
                best_any = Some(cand);
            }
        }
    }

    // Prefer a pair that actually clears the depth floor; fall back to the best
    // of the rest so the rejection reason is still reported.
    let Candidate { buy: bi, sell: sj, econ } = best_qualified.or(best_any)?;
    let es = econ.entry.clone();
    let buy_ex = quotes[bi].exchange;
    let sell_ex = quotes[sj].exchange;
    let leg_skew_ms = leg_skew_ms(quotes[bi], quotes[sj]);

    // Funding differential signal (independent of the price spread).
    let funding = if cfg.include_funding_diff {
        let fq: Vec<_> = quotes
            .iter()
            .filter_map(|q| q.funding.clone().map(|f| (q.exchange, f)))
            .collect();
        best_funding_diff(&fq).filter(|s| s.diff_apr >= cfg.min_funding_diff_apr)
    } else {
        None
    };

    let spread = Spread {
        instrument: snapshot.instrument.clone(),
        buy_exchange: buy_ex,
        sell_exchange: sell_ex,
        vwap_buy: es.vwap_buy,
        vwap_sell: es.vwap_sell,
        gross_pct: es.gross_pct,
        net_pct: es.net_pct,
        out_pct: econ.out_pct,
        round_trip_pct: econ.round_trip_pct,
        funding_cost_pct: econ.funding_cost_pct,
        expected_profit_quote: econ.expected_profit_quote,
        leg_skew_ms,
        executable_notional: es.executable_notional,
        capped_by_depth: es.capped_by_depth,
    };

    let stats = snapshot.stats.clone();
    let reason = classify(
        &econ,
        leg_skew_ms,
        cfg,
        oracle,
        quotes[bi],
        quotes[sj],
        stats.as_ref(),
    );
    let quality_score = Some(quality_score(
        &econ,
        leg_skew_ms,
        stats.as_ref(),
        quotes.len(),
        cfg,
    ));
    Some(Evaluation {
        spread,
        reason,
        funding,
        stats,
        quality_score,
    })
}

/// One candidate pairing under evaluation.
#[derive(Debug, Clone)]
struct Candidate {
    /// Indices into the filtered quote list.
    buy: usize,
    sell: usize,
    econ: PairEconomics,
}

/// Funding carried by the pair over the assumed hold: the long leg pays its own
/// funding, the short leg collects the other venue's. Positive is a cost.
/// Missing funding data counts as zero for that leg.
fn funding_cost_pct(cfg: &ClientConfig, buy_q: &ExchangeQuote, sell_q: &ExchangeQuote) -> Decimal {
    if !cfg.include_funding_cost {
        return Decimal::ZERO;
    }
    let hours = cfg.funding_hold_hours;
    let long = buy_q
        .funding
        .as_ref()
        .map_or(Decimal::ZERO, |f| f.cost_over(hours));
    let short = sell_q
        .funding
        .as_ref()
        .map_or(Decimal::ZERO, |f| f.cost_over(hours));
    long - short
}

/// How far apart in time the two legs' books were received.
fn leg_skew_ms(buy_q: &ExchangeQuote, sell_q: &ExchangeQuote) -> u64 {
    let (a, b) = (buy_q.book.recv_ts, sell_q.book.recv_ts);
    let older: Instant = a.min(b);
    let newer: Instant = a.max(b);
    newer.saturating_duration_since(older).as_millis() as u64
}

/// Mid price of a quote, used only for the cross-venue consensus check.
fn mid(q: &ExchangeQuote) -> Option<Decimal> {
    let (bid, ask) = (q.book.best_bid()?, q.book.best_ask()?);
    let m = (bid.price + ask.price) / Decimal::TWO;
    (m > Decimal::ZERO).then_some(m)
}

/// Median of the venues' mid prices — the consensus price for the instrument.
fn consensus_mid(quotes: &[&ExchangeQuote]) -> Option<Decimal> {
    let mut mids: Vec<Decimal> = quotes.iter().filter_map(|q| mid(q)).collect();
    if mids.is_empty() {
        return None;
    }
    mids.sort();
    let n = mids.len();
    Some(if n % 2 == 1 {
        mids[n / 2]
    } else {
        (mids[n / 2 - 1] + mids[n / 2]) / Decimal::TWO
    })
}

/// Remove venues whose mid deviates from the consensus by more than
/// `max_price_deviation_pct`. With the check disabled, or without a consensus,
/// every quote is kept.
///
/// Requires at least three venues: with two, the median is just their midpoint
/// and *both* sides of any wide spread look equally deviant — a legitimate 25%
/// dislocation would be thrown away along with the ghosts. Two-venue pairings
/// are left to `max_net_spread_pct` instead.
fn drop_price_outliers<'a>(
    quotes: &[&'a ExchangeQuote],
    cfg: &ClientConfig,
) -> Vec<&'a ExchangeQuote> {
    if quotes.len() < 3 {
        return quotes.to_vec();
    }
    let Some(max_dev) = cfg.max_price_deviation_pct else {
        return quotes.to_vec();
    };
    let Some(consensus) = consensus_mid(quotes) else {
        return quotes.to_vec();
    };
    quotes
        .iter()
        .copied()
        .filter(|q| match mid(q) {
            Some(m) => ((m - consensus) / consensus).abs() <= max_dev,
            None => false,
        })
        .collect()
}

/// Report a `PriceOutlier` rejection for the raw (pre-filter) best pairing, so
/// a consensus-driven drop is explained rather than silently swallowed.
fn outlier_rejection(
    snapshot: &InstrumentSnapshot,
    quotes: &[&ExchangeQuote],
    cfg: &ClientConfig,
) -> Option<Evaluation> {
    let mut best: Option<(ExchangeId, ExchangeId, ExecSpread)> = None;
    for (i, buy) in quotes.iter().enumerate() {
        for (j, sell) in quotes.iter().enumerate() {
            if i == j || buy.exchange == sell.exchange {
                continue;
            }
            if let Some(es) = executable_spread(
                &buy.book.asks,
                &sell.book.bids,
                cfg.target_notional_q,
                cfg.taker(buy.exchange),
                cfg.taker(sell.exchange),
            ) {
                if best.as_ref().map_or(true, |(_, _, b)| es.net_pct > b.net_pct) {
                    best = Some((buy.exchange, sell.exchange, es));
                }
            }
        }
    }
    let (buy_ex, sell_ex, es) = best?;
    Some(Evaluation {
        spread: Spread {
            instrument: snapshot.instrument.clone(),
            buy_exchange: buy_ex,
            sell_exchange: sell_ex,
            vwap_buy: es.vwap_buy,
            vwap_sell: es.vwap_sell,
            gross_pct: es.gross_pct,
            net_pct: es.net_pct,
            out_pct: Decimal::ZERO,
            round_trip_pct: Decimal::ZERO,
            funding_cost_pct: Decimal::ZERO,
            expected_profit_quote: Decimal::ZERO,
            leg_skew_ms: 0,
            executable_notional: es.executable_notional,
            capped_by_depth: es.capped_by_depth,
        },
        reason: SpreadReason::PriceOutlier,
        funding: None,
        stats: snapshot.stats.clone(),
        quality_score: None,
    })
}

/// Apply timing, band, round-trip, depth, liquidity, dynamics, and transfer
/// filters to a best pairing. Ordered cheapest-and-most-decisive first.
#[allow(clippy::too_many_arguments)]
fn classify(
    econ: &PairEconomics,
    leg_skew_ms: u64,
    cfg: &ClientConfig,
    oracle: &dyn TransferOracle,
    buy_q: &ExchangeQuote,
    sell_q: &ExchangeQuote,
    stats: Option<&SpreadStats>,
) -> SpreadReason {
    let es = &econ.entry;
    let buy_ex = buy_q.exchange;
    let sell_ex = sell_q.exchange;

    // The band goes first: a pairing with no edge is not an opportunity for any
    // reason, and reporting a data-quality reason for it would drown the
    // rejection metrics in pairs nobody would trade anyway.
    if es.net_pct < cfg.min_net_spread_pct {
        return SpreadReason::BelowMinSpread;
    }
    if es.net_pct > cfg.max_net_spread_pct {
        return SpreadReason::AboveMaxSpread; // ghost/delisted territory
    }
    // Both books can be individually fresh yet describe different moments.
    if cfg.max_leg_skew_ms > 0 && leg_skew_ms > cfg.max_leg_skew_ms {
        return SpreadReason::LegSkew;
    }
    // The entry edge has to survive the unwind: two more taker fees, the
    // funding carry, and the fact that the spread only converges to its
    // baseline rather than to zero.
    if econ.round_trip_pct < cfg.min_round_trip_pct {
        return SpreadReason::NegativeRoundTrip;
    }
    if es.executable_notional < cfg.min_executable_notional {
        return SpreadReason::InsufficientDepth;
    }
    // Liquidity band. The floor applies to the **thinner** leg — that leg is
    // the bottleneck for getting filled — while the ceiling applies to the
    // thicker one, so "both venues are small" is what keeps a coin in a low-cap
    // band. Skipped for whichever bound no venue reports.
    if cfg.min_24h_quote_volume > Decimal::ZERO {
        if let Some(vol) = min_opt(buy_q.quote_volume_24h, sell_q.quote_volume_24h) {
            if vol < cfg.min_24h_quote_volume {
                return SpreadReason::BelowMinVolume;
            }
        }
    }
    if let Some(max_vol) = cfg.max_24h_quote_volume {
        if let Some(vol) = max_opt(buy_q.quote_volume_24h, sell_q.quote_volume_24h) {
            if vol > max_vol {
                return SpreadReason::AboveMaxVolume;
            }
        }
    }
    if let Some(min_oi) = cfg.min_open_interest {
        if min_oi > Decimal::ZERO {
            if let Some(oi) = min_opt(buy_q.open_interest, sell_q.open_interest) {
                if oi < min_oi {
                    return SpreadReason::BelowMinOpenInterest;
                }
            }
        }
    }
    // Spread-dynamics filters: reject persistently-wide, non-spike, or too-long
    // episodes once enough *baseline* history exists (skip during warmup).
    if cfg.enable_dynamics {
        if let Some(st) = stats {
            if st.baseline_samples >= cfg.min_dynamics_samples as usize {
                if st.baseline_pct > cfg.max_baseline_spread_pct {
                    return SpreadReason::PersistentWide;
                }
                // Score *this* client's spread, not the shared history's last
                // sample — fees differ per client, so the two are not the same
                // number and filtering on the wrong one rejects good signals.
                match st.z_for(es.net_pct) {
                    Some(z) if z >= cfg.min_spike_z => {}
                    _ => return SpreadReason::NotASpike,
                }
                if st.episode_ms > cfg.max_spread_duration_ms {
                    return SpreadReason::TooPersistent;
                }
            }
        }
    }
    if let Err(r) = transfer_ok(cfg, oracle, buy_ex, sell_ex) {
        return r;
    }
    SpreadReason::Signal
}

/// Max of two optional decimals; `None` only when both are `None`.
fn max_opt(a: Option<Decimal>, b: Option<Decimal>) -> Option<Decimal> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Min of two optional decimals; `None` only when both are `None`.
fn min_opt(a: Option<Decimal>, b: Option<Decimal>) -> Option<Decimal> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// 0–100 arb-quality score for ranking signals against each other.
///
/// Unlike the earlier version this is dominated by the things that decide
/// whether the trade makes money — the round-trip edge and the depth behind it
/// — with the baseline/spike shape and data quality as modifiers.
fn quality_score(
    econ: &PairEconomics,
    leg_skew_ms: u64,
    stats: Option<&SpreadStats>,
    coverage: usize,
    cfg: &ClientConfig,
) -> Decimal {
    let clamp01 = |x: f64| x.clamp(0.0, 1.0);
    let f = |d: Decimal| d.to_f64().unwrap_or(0.0);

    // Edge: the round-trip profit, saturating at 1%.
    let edge = clamp01(f(econ.round_trip_pct) / 0.01);
    // Depth: how much of the requested size the books actually support.
    let target = f(cfg.target_notional_q).max(1.0);
    let depth = clamp01(f(econ.entry.executable_notional) / target);
    // Freshness: how simultaneous the two legs were.
    let skew_budget = cfg.max_leg_skew_ms.max(1) as f64;
    let freshness = clamp01(1.0 - leg_skew_ms as f64 / skew_budget);
    let cov = clamp01(coverage as f64 / 4.0);

    // Shape: a tight baseline punctuated by a strong spike. Neutral (0.5) until
    // enough history exists to say anything.
    let (tightness, spike) = match stats {
        Some(st) if st.baseline_samples >= cfg.min_dynamics_samples as usize => {
            let max_base = f(cfg.max_baseline_spread_pct).max(1e-9);
            let z = st.z_for(econ.entry.net_pct).map(f).unwrap_or(0.0);
            (
                clamp01(1.0 - f(st.baseline_pct) / max_base),
                clamp01(z / (f(cfg.min_spike_z).max(1e-9) * 2.0)),
            )
        }
        _ => (0.5, 0.5),
    };

    let score = 100.0
        * (0.30 * edge
            + 0.20 * depth
            + 0.20 * spike
            + 0.15 * tightness
            + 0.10 * freshness
            + 0.05 * cov);
    Decimal::from_f64(score).unwrap_or_default().round_dp(1)
}

/// Settlement-asset transferability between the two venues (perp margin
/// rebalance feasibility). Fails closed when required data is missing.
fn transfer_ok(
    cfg: &ClientConfig,
    oracle: &dyn TransferOracle,
    buy_ex: ExchangeId,
    sell_ex: ExchangeId,
) -> Result<(), SpreadReason> {
    if !cfg.require_transferable && !cfg.require_common_network {
        return Ok(());
    }
    let asset = cfg.quote.to_uppercase();
    let buy_st = oracle.status(buy_ex, &asset);
    let sell_st = oracle.status(sell_ex, &asset);

    if cfg.require_transferable {
        match (&buy_st, &sell_st) {
            (Some(b), Some(s)) if b.is_transferable() && s.is_transferable() => {}
            _ => return Err(SpreadReason::NotTransferable),
        }
    }
    if cfg.require_common_network {
        match (&buy_st, &sell_st) {
            (Some(b), Some(s))
                if has_common_transfer_route(b, s) || has_common_transfer_route(s, b) => {}
            _ => return Err(SpreadReason::NoCommonNetwork),
        }
    }
    Ok(())
}

/// The best raw executable spread across a snapshot, ignoring all alert filters
/// (band/dynamics/transfer). This is the *unfiltered* spread that drives the
/// chart and the rolling dynamics — computed with the client/default fees.
///
/// `ts_ms` is stamped onto the returned point. Returns `None` when fewer than two
/// enabled venues have a usable book.
pub fn best_spread_point(
    snapshot: &InstrumentSnapshot,
    cfg: &ClientConfig,
    ts_ms: i64,
) -> Option<domain::SpreadPoint> {
    let all: Vec<_> = snapshot
        .usable()
        .filter(|q| cfg.includes(q.exchange))
        .collect();
    // Consensus filtering matters most here: this series feeds the rolling
    // baseline, and one wrong-token venue would poison it for every client.
    let quotes = drop_price_outliers(&all, cfg);
    if quotes.len() < 2 {
        return None;
    }
    let mut best: Option<(ExchangeId, ExchangeId, ExecSpread)> = None;
    for (i, buy) in quotes.iter().enumerate() {
        for (j, sell) in quotes.iter().enumerate() {
            if i == j || buy.exchange == sell.exchange {
                continue;
            }
            if cfg.max_leg_skew_ms > 0 && leg_skew_ms(buy, sell) > cfg.max_leg_skew_ms {
                continue;
            }
            if let Some(es) = executable_spread(
                &buy.book.asks,
                &sell.book.bids,
                cfg.target_notional_q,
                cfg.taker(buy.exchange),
                cfg.taker(sell.exchange),
            ) {
                if best.as_ref().map_or(true, |(_, _, b)| es.net_pct > b.net_pct) {
                    best = Some((buy.exchange, sell.exchange, es));
                }
            }
        }
    }
    let (buy_ex, sell_ex, es) = best?;
    Some(domain::SpreadPoint {
        ts_ms,
        net_pct: es.net_pct,
        gross_pct: es.gross_pct,
        baseline_pct: snapshot.stats.as_ref().map(|s| s.baseline_pct),
        buy_exchange: buy_ex,
        sell_exchange: sell_ex,
        executable_notional: es.executable_notional,
        capped_by_depth: es.capped_by_depth,
    })
}

/// The best raw net spread across a snapshot, ignoring all filters. Returns
/// `(buy, sell, net_pct)`.
pub fn best_raw_net(
    snapshot: &InstrumentSnapshot,
    cfg: &ClientConfig,
) -> Option<(ExchangeId, ExchangeId, Decimal)> {
    best_spread_point(snapshot, cfg, 0)
        .map(|p| (p.buy_exchange, p.sell_exchange, p.net_pct))
}

/// Best spread for the REST `/summary` snapshot, with the config's *static*
/// filters applied: symbol allow/deny, market pair, the net-spread band, and
/// the 24h volume band. Dynamics, transferability, and hysteresis are
/// deliberately skipped — this is a cold-start dashboard row, not an alert —
/// but denied coins and ghost spreads must not leak into the view either.
pub fn summary_row(
    snapshot: &InstrumentSnapshot,
    cfg: &ClientConfig,
) -> Option<(ExchangeId, ExchangeId, Decimal)> {
    if !cfg.allows_symbol(&snapshot.instrument.base) {
        return None;
    }
    let kind = snapshot.instrument.kind;
    if !cfg.allows_market_pair(kind, kind) {
        return None;
    }
    let (buy, sell, net) = best_raw_net(snapshot, cfg)?;
    if net < cfg.min_net_spread_pct || net > cfg.max_net_spread_pct {
        return None;
    }
    // Volume band against the chosen legs (skipped when neither reports it).
    let leg_vol = |ex: ExchangeId| {
        snapshot
            .usable()
            .find(|q| q.exchange == ex)
            .and_then(|q| q.quote_volume_24h)
    };
    if cfg.min_24h_quote_volume > Decimal::ZERO {
        if let Some(vol) = min_opt(leg_vol(buy), leg_vol(sell)) {
            if vol < cfg.min_24h_quote_volume {
                return None;
            }
        }
    }
    if let Some(max_vol) = cfg.max_24h_quote_volume {
        if let Some(vol) = max_opt(leg_vol(buy), leg_vol(sell)) {
            if vol > max_vol {
                return None;
            }
        }
    }
    Some((buy, sell, net))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{BookLevel, Instrument, TopBook};
    use market_state::InstrumentSnapshot;
    use rust_decimal_macros::dec;
    use std::time::{Duration, Instant};

    fn book(bid: Decimal, ask: Decimal) -> TopBook {
        book_full(bid, ask, dec!(1000), Instant::now())
    }

    fn book_full(bid: Decimal, ask: Decimal, qty: Decimal, recv_ts: Instant) -> TopBook {
        TopBook {
            bids: vec![BookLevel::new(bid, qty)],
            asks: vec![BookLevel::new(ask, qty)],
            recv_ts,
            exch_ts: None,
        }
    }

    fn quote(ex: ExchangeId, bid: Decimal, ask: Decimal, vol: Option<Decimal>) -> ExchangeQuote {
        from_book(ex, book(bid, ask), vol)
    }

    fn from_book(ex: ExchangeId, book: TopBook, vol: Option<Decimal>) -> ExchangeQuote {
        ExchangeQuote {
            exchange: ex,
            book,
            funding: None,
            quote_volume_24h: vol,
            open_interest: None,
            stale: false,
            valid: true,
        }
    }

    /// Snapshot from an explicit quote list, with the in-band default volume.
    fn snapshot_of(quotes: Vec<ExchangeQuote>) -> InstrumentSnapshot {
        InstrumentSnapshot {
            instrument: Instrument::perp("XYZ", "USDT"),
            quotes,
            stats: None,
        }
    }

    fn eval_cfg() -> ClientConfig {
        let mut c = ClientConfig::default();
        c.require_transferable = false;
        c.require_common_network = false;
        c
    }

    fn reason_of(snap: &InstrumentSnapshot, cfg: &ClientConfig) -> SpreadReason {
        evaluate(snap, cfg, &NoTransferInfo).expect("a candidate").reason
    }

    /// buy on Bybit at `cheap_ask`, sell on OKX at `rich_bid`.
    fn snapshot(base: &str, cheap_ask: Decimal, rich_bid: Decimal) -> InstrumentSnapshot {
        InstrumentSnapshot {
            instrument: Instrument::perp(base, "USDT"),
            quotes: vec![
                quote(ExchangeId::Bybit, cheap_ask - dec!(1), cheap_ask, Some(dec!(150000))),
                quote(ExchangeId::Okx, rich_bid, rich_bid + dec!(1), None),
            ],
            stats: None,
        }
    }

    fn cfg() -> ClientConfig {
        ClientConfig::default() // band 0.6%..25%, volume band 100k..200k
    }

    #[test]
    fn summary_row_passes_in_band_spread() {
        // buy 100, sell 106 => ~6% gross, inside the band; volume 150k in band.
        let row = summary_row(&snapshot("XYZ", dec!(100), dec!(106)), &cfg());
        let (buy, sell, net) = row.expect("in-band row");
        assert_eq!(buy, ExchangeId::Bybit);
        assert_eq!(sell, ExchangeId::Okx);
        assert!(net > dec!(0.05) && net < dec!(0.07));
    }

    #[test]
    fn summary_row_drops_ghost_spread() {
        // buy 100, sell 56770 => the "56669%" ghost; must not leak into /summary.
        assert!(summary_row(&snapshot("BB", dec!(100), dec!(56770)), &cfg()).is_none());
    }

    #[test]
    fn summary_row_drops_below_min_spread() {
        // buy 100, sell 100.2 => ~0.08% net, below the 0.6% floor.
        assert!(summary_row(&snapshot("XYZ", dec!(100), dec!(100.2)), &cfg()).is_none());
    }

    #[test]
    fn summary_row_respects_deny_list() {
        let mut c = cfg();
        c.deny_symbols = vec!["bb".into()]; // case-insensitive
        assert!(summary_row(&snapshot("BB", dec!(100), dec!(106)), &c).is_none());
        assert!(summary_row(&snapshot("XYZ", dec!(100), dec!(106)), &c).is_some());
    }

    #[test]
    fn summary_row_respects_volume_band() {
        let mut snap = snapshot("XYZ", dec!(100), dec!(106));
        snap.quotes[0].quote_volume_24h = Some(dec!(900000)); // above 200k ceiling
        assert!(summary_row(&snap, &cfg()).is_none());
        snap.quotes[0].quote_volume_24h = Some(dec!(50000)); // below 100k floor
        assert!(summary_row(&snap, &cfg()).is_none());
        snap.quotes[0].quote_volume_24h = None; // unknown on both legs => skipped
        assert!(summary_row(&snap, &cfg()).is_some());
    }

    /// An entry spread that clears the band can still lose money once the
    /// unwind's two extra taker fees are charged.
    #[test]
    fn round_trip_rejects_a_fee_thin_edge() {
        let mut c = eval_cfg();
        c.min_net_spread_pct = dec!(0.001); // let the thin edge past the band
        // gross 0.25%, entry net 0.13%, round trip 0.01% — below the floor.
        let snap = snapshot_of(vec![
            quote(ExchangeId::Bybit, dec!(99.9), dec!(100), Some(dec!(150000))),
            quote(ExchangeId::Okx, dec!(100.25), dec!(100.35), Some(dec!(150000))),
        ]);
        assert_eq!(reason_of(&snap, &c), SpreadReason::NegativeRoundTrip);

        let ev = evaluate(&snap, &c, &NoTransferInfo).unwrap();
        assert!(ev.spread.net_pct > dec!(0), "the entry alone looks fine");
        assert!(ev.spread.round_trip_pct < ev.spread.net_pct);
        assert!(ev.spread.out_pct < dec!(0));
    }

    /// Funding carry is part of the trade's economics, not a separate signal.
    #[test]
    fn funding_carry_is_charged_against_the_edge() {
        use domain::FundingInfo;
        let mut long_leg = quote(ExchangeId::Bybit, dec!(99.9), dec!(100), Some(dec!(150000)));
        // Long leg pays 1% per 8h interval over an 8h hold.
        long_leg.funding = Some(FundingInfo {
            rate: dec!(0.01),
            interval_hours: dec!(8),
            next_ts: 0,
        });
        let snap = snapshot_of(vec![
            long_leg,
            quote(ExchangeId::Okx, dec!(106), dec!(106.1), Some(dec!(150000))),
        ]);

        let mut c = eval_cfg();
        let with_cost = evaluate(&snap, &c, &NoTransferInfo).unwrap();
        c.include_funding_cost = false;
        let without = evaluate(&snap, &c, &NoTransferInfo).unwrap();

        assert_eq!(with_cost.spread.funding_cost_pct, dec!(0.01));
        assert_eq!(
            with_cost.spread.round_trip_pct,
            without.spread.round_trip_pct - dec!(0.01)
        );
    }

    /// Ranking by percent handed the slot to a pair nobody could trade. The
    /// wide-but-empty pairing must lose to the narrower one with real depth.
    #[test]
    fn ranks_pairs_by_expected_profit_not_percent() {
        let vol = Some(dec!(150000));
        let snap = snapshot_of(vec![
            from_book(
                ExchangeId::Bybit,
                book_full(dec!(99.9), dec!(100), dec!(1000), Instant::now()),
                vol,
            ),
            // 6% away, but only ~$53 of depth behind the bid.
            from_book(
                ExchangeId::Okx,
                book_full(dec!(106), dec!(106.1), dec!(0.5), Instant::now()),
                vol,
            ),
            // 2% away with the full target size available.
            from_book(
                ExchangeId::Mexc,
                book_full(dec!(102), dec!(102.1), dec!(1000), Instant::now()),
                vol,
            ),
        ]);
        let ev = evaluate(&snap, &eval_cfg(), &NoTransferInfo).unwrap();
        assert_eq!(ev.reason, SpreadReason::Signal);
        assert_eq!(ev.spread.buy_exchange, ExchangeId::Bybit);
        assert_eq!(
            ev.spread.sell_exchange,
            ExchangeId::Mexc,
            "the tradable pair must win over the wider empty one"
        );
        assert!(ev.spread.expected_profit_quote > dec!(0));
    }

    /// Two individually-fresh books observed seconds apart never coexisted.
    #[test]
    fn legs_observed_far_apart_are_rejected() {
        let now = Instant::now();
        let snap = snapshot_of(vec![
            from_book(
                ExchangeId::Bybit,
                book_full(dec!(99.9), dec!(100), dec!(1000), now - Duration::from_millis(2000)),
                Some(dec!(150000)),
            ),
            from_book(
                ExchangeId::Okx,
                book_full(dec!(106), dec!(106.1), dec!(1000), now),
                Some(dec!(150000)),
            ),
        ]);
        let ev = evaluate(&snap, &eval_cfg(), &NoTransferInfo).unwrap();
        assert_eq!(ev.reason, SpreadReason::LegSkew);
        assert!(ev.spread.leg_skew_ms >= 2000);
    }

    /// A venue quoting a different token entirely must be dropped before
    /// pairing, not merely capped by the spread band afterwards.
    #[test]
    fn wrong_token_venue_is_dropped_from_the_consensus() {
        let vol = Some(dec!(150000));
        let snap = snapshot_of(vec![
            quote(ExchangeId::Bybit, dec!(99.9), dec!(100), vol),
            quote(ExchangeId::Okx, dec!(100.5), dec!(100.6), vol),
            // 3x the consensus price — a different asset under the same base.
            quote(ExchangeId::Mexc, dec!(300), dec!(300.1), vol),
        ]);
        let ev = evaluate(&snap, &eval_cfg(), &NoTransferInfo).unwrap();
        assert_ne!(ev.spread.buy_exchange, ExchangeId::Mexc);
        assert_ne!(ev.spread.sell_exchange, ExchangeId::Mexc);
        assert!(
            ev.spread.gross_pct < dec!(0.02),
            "only the two consensus venues should pair"
        );
    }

    /// Fewer than three venues carries no consensus, so a legitimate wide
    /// spread must survive.
    #[test]
    fn two_venue_wide_spread_is_not_treated_as_an_outlier() {
        let snap = snapshot_of(vec![
            quote(ExchangeId::Bybit, dec!(99.9), dec!(100), Some(dec!(150000))),
            quote(ExchangeId::Okx, dec!(122), dec!(122.1), Some(dec!(150000))),
        ]);
        assert_eq!(reason_of(&snap, &eval_cfg()), SpreadReason::Signal);
    }

    /// The volume floor guards fillability, so it belongs on the *thinner* leg
    /// — the bottleneck — not on whichever leg happens to be busiest.
    #[test]
    fn volume_floor_applies_to_the_thinner_leg() {
        let mut c = eval_cfg();
        c.max_24h_quote_volume = None;
        c.min_24h_quote_volume = dec!(100000);
        let snap = snapshot_of(vec![
            quote(ExchangeId::Bybit, dec!(99.9), dec!(100), Some(dec!(5000000))),
            quote(ExchangeId::Okx, dec!(106), dec!(106.1), Some(dec!(50000))),
        ]);
        assert_eq!(reason_of(&snap, &c), SpreadReason::BelowMinVolume);
    }

    #[test]
    fn summary_row_respects_market_pairs() {
        let mut c = cfg();
        c.market_pairs = vec![crate::config::MarketPair::SPOT_SPOT];
        assert!(summary_row(&snapshot("XYZ", dec!(100), dec!(106)), &c).is_none());
    }
}
