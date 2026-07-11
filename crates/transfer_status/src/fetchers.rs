//! Per-exchange public currency-info fetchers.
//!
//! Each returns transfer status for the requested assets. Only PUBLIC endpoints
//! are used (Phase 1: no keys). Venues without a public endpoint return an empty
//! vec (status stays unknown → transfer filters fail closed for them).
//!
//! DOCS (verify against live docs before production):
//!   gate:   GET https://api.gateio.ws/api/v4/spot/currencies
//!   kucoin: GET https://api.kucoin.com/api/v3/currencies
//!   bitget: GET https://api.bitget.com/api/v2/spot/public/coins        (TODO parse)
//!   coinex: GET https://api.coinex.com/v2/assets/all-deposit-withdraw-config (TODO parse)
//!   phemex: GET https://api.phemex.com/public/cfg/v2/products          (TODO parse)
//!   bybit/okx/mexc: authenticated only — not available in Phase 1.

use domain::{ExchangeId, Network, TransferStatus};
use reqwest::Client;
use serde_json::Value;
use std::collections::BTreeMap;
use tracing::warn;

/// Fetch transfer status for `assets` (upper-cased) on `exchange`. Assets not
/// present at the venue are simply omitted from the result.
pub async fn fetch(
    client: &Client,
    exchange: ExchangeId,
    assets: &[String],
) -> anyhow::Result<Vec<TransferStatus>> {
    match exchange {
        ExchangeId::Gate => fetch_gate(client, assets).await,
        ExchangeId::Kucoin => fetch_kucoin(client, assets).await,
        // Public endpoints exist but parsing is not implemented yet — documented
        // above. Returning empty keeps transfer filters fail-closed here.
        ExchangeId::Bitget | ExchangeId::Coinex | ExchangeId::Phemex => {
            warn!(exchange = %exchange, "public transfer fetch not yet implemented");
            Ok(vec![])
        }
        // Authenticated-only in Phase 1.
        ExchangeId::Bybit | ExchangeId::Okx | ExchangeId::Mexc => Ok(vec![]),
    }
}

fn wants(assets: &[String], asset: &str) -> bool {
    assets.iter().any(|a| a.eq_ignore_ascii_case(asset))
}

/// Gate.io v4 spot currencies. Each row carries a currency, a chain, and
/// deposit/withdraw *disabled* flags; we invert to enabled and group by chain.
async fn fetch_gate(client: &Client, assets: &[String]) -> anyhow::Result<Vec<TransferStatus>> {
    let url = "https://api.gateio.ws/api/v4/spot/currencies";
    let rows: Value = client.get(url).send().await?.json().await?;
    let Some(arr) = rows.as_array() else {
        return Ok(vec![]);
    };

    // asset -> chain -> Network
    let mut by_asset: BTreeMap<String, BTreeMap<String, Network>> = BTreeMap::new();
    for row in arr {
        let Some(cur) = row.get("currency").and_then(Value::as_str) else {
            continue;
        };
        if !wants(assets, cur) {
            continue;
        }
        let chain = row
            .get("chain")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(cur)
            .to_uppercase();
        let deposit_enabled = !row
            .get("deposit_disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let withdraw_enabled = !row
            .get("withdraw_disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        by_asset
            .entry(cur.to_uppercase())
            .or_default()
            .insert(
                chain.clone(),
                Network {
                    chain,
                    deposit_enabled,
                    withdraw_enabled,
                },
            );
    }
    Ok(aggregate(by_asset))
}

/// KuCoin v3 currencies. Each currency has a `chains` array with
/// isDepositEnabled/isWithdrawEnabled per chain.
async fn fetch_kucoin(client: &Client, assets: &[String]) -> anyhow::Result<Vec<TransferStatus>> {
    let url = "https://api.kucoin.com/api/v3/currencies";
    let body: Value = client.get(url).send().await?.json().await?;
    let Some(arr) = body.get("data").and_then(Value::as_array) else {
        return Ok(vec![]);
    };

    let mut out = Vec::new();
    for cur in arr {
        let Some(name) = cur.get("currency").and_then(Value::as_str) else {
            continue;
        };
        if !wants(assets, name) {
            continue;
        }
        let mut networks = Vec::new();
        if let Some(chains) = cur.get("chains").and_then(Value::as_array) {
            for ch in chains {
                let chain = ch
                    .get("chainName")
                    .or_else(|| ch.get("chainId"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_uppercase();
                let deposit_enabled = ch
                    .get("isDepositEnabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let withdraw_enabled = ch
                    .get("isWithdrawEnabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                networks.push(Network {
                    chain,
                    deposit_enabled,
                    withdraw_enabled,
                });
            }
        }
        out.push(finalize(name.to_uppercase(), networks));
    }
    Ok(out)
}

/// Collapse a per-chain map into `TransferStatus` values.
fn aggregate(by_asset: BTreeMap<String, BTreeMap<String, Network>>) -> Vec<TransferStatus> {
    by_asset
        .into_iter()
        .map(|(asset, chains)| finalize(asset, chains.into_values().collect()))
        .collect()
}

/// Build a `TransferStatus`, deriving the aggregate flags from the networks.
fn finalize(asset: String, networks: Vec<Network>) -> TransferStatus {
    let deposit_enabled = networks.iter().any(|n| n.deposit_enabled);
    let withdraw_enabled = networks.iter().any(|n| n.withdraw_enabled);
    TransferStatus {
        asset,
        deposit_enabled,
        withdraw_enabled,
        networks,
    }
}
