//! Client-filter evaluation over an instrument snapshot: find the best
//! executable cross-exchange pairing and decide whether it passes the client's
//! band, depth, and transferability filters.

use crate::config::ClientConfig;
use crate::executable::{executable_spread, ExecSpread};
use crate::funding::{best_funding_diff, FundingSignal};
use domain::transfer::has_common_transfer_route;
use domain::{Decimal, ExchangeId, Spread, SpreadReason, TransferStatus};
use market_state::{ExchangeQuote, InstrumentSnapshot, SpreadStats};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};

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

    // Usable, client-enabled quotes only.
    let quotes: Vec<_> = snapshot
        .usable()
        .filter(|q| cfg.includes(q.exchange))
        .collect();
    if quotes.len() < 2 {
        return None;
    }

    // Brute-force best pairing by net spread (venue count is small).
    let mut best: Option<(usize, usize, ExecSpread)> = None;
    for (i, buy) in quotes.iter().enumerate() {
        for (j, sell) in quotes.iter().enumerate() {
            if i == j || buy.exchange == sell.exchange {
                continue;
            }
            let fee_buy = cfg.taker(buy.exchange);
            let fee_sell = cfg.taker(sell.exchange);
            if let Some(es) = executable_spread(
                &buy.book.asks,
                &sell.book.bids,
                cfg.target_notional_q,
                fee_buy,
                fee_sell,
            ) {
                if best.as_ref().map_or(true, |(_, _, b)| es.net_pct > b.net_pct) {
                    best = Some((i, j, es));
                }
            }
        }
    }

    let (bi, sj, es) = best?;
    let buy_ex = quotes[bi].exchange;
    let sell_ex = quotes[sj].exchange;

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
        executable_notional: es.executable_notional,
        capped_by_depth: es.capped_by_depth,
    };

    let stats = snapshot.stats.clone();
    let reason = classify(&es, cfg, oracle, quotes[bi], quotes[sj], stats.as_ref());
    let quality_score = stats
        .as_ref()
        .map(|st| quality_score(st, quotes.len(), cfg.max_spread_duration_ms));
    Some(Evaluation {
        spread,
        reason,
        funding,
        stats,
        quality_score,
    })
}

/// Apply band, depth, liquidity, dynamics, and transfer filters to a best pairing.
fn classify(
    es: &ExecSpread,
    cfg: &ClientConfig,
    oracle: &dyn TransferOracle,
    buy_q: &ExchangeQuote,
    sell_q: &ExchangeQuote,
    stats: Option<&SpreadStats>,
) -> SpreadReason {
    let buy_ex = buy_q.exchange;
    let sell_ex = sell_q.exchange;
    if es.net_pct < cfg.min_net_spread_pct {
        return SpreadReason::BelowMinSpread;
    }
    if es.net_pct > cfg.max_net_spread_pct {
        return SpreadReason::AboveMaxSpread; // ghost/delisted territory
    }
    if es.executable_notional < cfg.min_executable_notional {
        return SpreadReason::InsufficientDepth;
    }
    // Liquidity floors. Applied against the higher of the two legs' reported
    // figures; skipped when neither venue reports the metric (unknown).
    if cfg.min_24h_quote_volume > Decimal::ZERO {
        if let Some(vol) = max_opt(buy_q.quote_volume_24h, sell_q.quote_volume_24h) {
            if vol < cfg.min_24h_quote_volume {
                return SpreadReason::BelowMinVolume;
            }
        }
    }
    if let Some(min_oi) = cfg.min_open_interest {
        if min_oi > Decimal::ZERO {
            if let Some(oi) = max_opt(buy_q.open_interest, sell_q.open_interest) {
                if oi < min_oi {
                    return SpreadReason::BelowMinOpenInterest;
                }
            }
        }
    }
    // Spread-dynamics filters: reject persistently-wide, non-spike, or too-long
    // episodes once enough history exists (skip during warmup).
    if cfg.enable_dynamics {
        if let Some(st) = stats {
            if st.sample_count >= cfg.min_dynamics_samples as usize {
                if st.baseline_pct > cfg.max_baseline_spread_pct {
                    return SpreadReason::PersistentWide;
                }
                match st.z_score {
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

/// 0–100 arb-quality score: rewards a tight baseline, a strong current spike,
/// short episode duration, and broad venue coverage.
fn quality_score(stats: &SpreadStats, coverage: usize, max_dur_ms: u64) -> Decimal {
    let clamp01 = |x: f64| x.clamp(0.0, 1.0);
    let baseline = stats.baseline_pct.to_f64().unwrap_or(0.0);
    let z = stats.z_score.and_then(|d| d.to_f64()).unwrap_or(0.0);
    let episode = stats.episode_ms as f64;

    let tightness = clamp01(1.0 - baseline / 0.02); // baseline 0 → 1, ≥2% → 0
    let spike = clamp01(z / 6.0);
    let transience = clamp01(1.0 - episode / (max_dur_ms.max(1) as f64));
    let cov = clamp01(coverage as f64 / 4.0);

    let score = 100.0 * (0.35 * tightness + 0.30 * spike + 0.20 * transience + 0.15 * cov);
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

/// The best raw net spread across a snapshot, ignoring all filters — used by the
/// REST `/summary` endpoint. Returns `(buy, sell, net_pct)`.
pub fn best_raw_net(
    snapshot: &InstrumentSnapshot,
    cfg: &ClientConfig,
) -> Option<(ExchangeId, ExchangeId, Decimal)> {
    let quotes: Vec<_> = snapshot
        .usable()
        .filter(|q| cfg.includes(q.exchange))
        .collect();
    let mut best: Option<(ExchangeId, ExchangeId, Decimal)> = None;
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
                if best.map_or(true, |(_, _, n)| es.net_pct > n) {
                    best = Some((buy.exchange, sell.exchange, es.net_pct));
                }
            }
        }
    }
    best
}
