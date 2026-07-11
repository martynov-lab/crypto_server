//! Client-filter evaluation over an instrument snapshot: find the best
//! executable cross-exchange pairing and decide whether it passes the client's
//! band, depth, and transferability filters.

use crate::config::ClientConfig;
use crate::executable::{executable_spread, ExecSpread};
use crate::funding::{best_funding_diff, FundingSignal};
use domain::transfer::has_common_transfer_route;
use domain::{Decimal, ExchangeId, Spread, SpreadReason, TransferStatus};
use market_state::InstrumentSnapshot;

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

    let reason = classify(&es, cfg, oracle, buy_ex, sell_ex);
    Some(Evaluation {
        spread,
        reason,
        funding,
    })
}

/// Apply band, depth, and transfer filters to a best pairing.
fn classify(
    es: &ExecSpread,
    cfg: &ClientConfig,
    oracle: &dyn TransferOracle,
    buy_ex: ExchangeId,
    sell_ex: ExchangeId,
) -> SpreadReason {
    if es.net_pct < cfg.min_net_spread_pct {
        return SpreadReason::BelowMinSpread;
    }
    if es.net_pct > cfg.max_net_spread_pct {
        return SpreadReason::AboveMaxSpread; // ghost/delisted territory
    }
    if es.executable_notional < cfg.min_executable_notional {
        return SpreadReason::InsufficientDepth;
    }
    if let Err(r) = transfer_ok(cfg, oracle, buy_ex, sell_ex) {
        return r;
    }
    SpreadReason::Signal
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
