//! Per-exchange PUBLIC instrument-list fetchers. Each returns the canonical
//! base assets that have a **USDT-settled perpetual** listed and actively
//! trading on that venue, together with the venue's contract size. Verified
//! against live responses.

use domain::{Decimal, ExchangeId};
use reqwest::Client;
use serde_json::Value;
use std::str::FromStr;

/// One listed perp: its canonical base asset and how much of that base one
/// contract represents on this venue.
///
/// Several venues quote order-book sizes in **contracts**, not base units. A
/// Gate book showing `10` might be 10 coins, 1 coin, or 1000 — depending on the
/// contract's multiplier. Without this the depth-aware VWAP walk, the
/// executable notional, and every filter built on them are wrong by whatever
/// factor the venue chose, which is exactly the "real vs mirage" distinction
/// the screener exists to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub base: String,
    /// Base units per contract. `1` for venues that quote sizes in base units.
    pub contract_size: Decimal,
}

impl Listing {
    /// A listing on a venue whose book sizes are already in base units.
    fn base_units(base: String) -> Self {
        Listing {
            base,
            contract_size: Decimal::ONE,
        }
    }
}

/// Read a contract multiplier that may arrive as a JSON string or number.
/// Falls back to `1` when absent or non-positive — never scale by garbage.
fn contract_size(v: &Value, field: &str) -> Decimal {
    let parsed = match v.get(field) {
        Some(Value::String(s)) => Decimal::from_str(s.trim()).ok(),
        // Route numbers through their JSON text so a float never loses
        // precision on the way into a decimal.
        Some(n @ Value::Number(_)) => Decimal::from_str(&n.to_string()).ok(),
        _ => None,
    };
    match parsed {
        Some(d) if d > Decimal::ZERO => d,
        _ => Decimal::ONE,
    }
}

pub async fn fetch(client: &Client, exchange: ExchangeId) -> anyhow::Result<Vec<Listing>> {
    match exchange {
        ExchangeId::Bybit => bybit(client).await,
        ExchangeId::Okx => okx(client).await,
        ExchangeId::Mexc => mexc(client).await,
        ExchangeId::Bitget => bitget(client).await,
        ExchangeId::Gate => gate(client).await,
        ExchangeId::Coinex => coinex(client).await,
        ExchangeId::Kucoin => kucoin(client).await,
        ExchangeId::Phemex => phemex(client).await,
    }
}

async fn get_json(client: &Client, url: &str) -> anyhow::Result<Value> {
    Ok(client.get(url).send().await?.json().await?)
}

fn up(s: &str) -> String {
    s.to_uppercase()
}

/// KuCoin uses XBT for Bitcoin on futures; normalize to BTC.
fn normalize_base(b: &str) -> String {
    let b = b.to_uppercase();
    if b == "XBT" {
        "BTC".to_string()
    } else {
        b
    }
}

/// Bybit v5 linear perps quote order-book sizes in the base coin already.
async fn bybit(client: &Client) -> anyhow::Result<Vec<Listing>> {
    // Paginated; USDT perps fit comfortably but honor nextPageCursor.
    let mut out = Vec::new();
    let mut cursor = String::new();
    loop {
        let url = format!(
            "https://api.bybit.com/v5/market/instruments-info?category=linear&limit=1000{}",
            if cursor.is_empty() {
                String::new()
            } else {
                format!("&cursor={cursor}")
            }
        );
        let v = get_json(client, &url).await?;
        let list = v
            .pointer("/result/list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for it in &list {
            let quote = it.get("quoteCoin").and_then(Value::as_str).unwrap_or("");
            let status = it.get("status").and_then(Value::as_str).unwrap_or("");
            let ctype = it.get("contractType").and_then(Value::as_str).unwrap_or("");
            if quote == "USDT" && status == "Trading" && ctype == "LinearPerpetual" {
                if let Some(base) = it.get("baseCoin").and_then(Value::as_str) {
                    out.push(Listing::base_units(up(base)));
                }
            }
        }
        cursor = v
            .pointer("/result/nextPageCursor")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if cursor.is_empty() {
            break;
        }
    }
    Ok(out)
}

/// OKX swaps report book sizes in **contracts**; `ctVal` is the base amount per
/// contract and `ctMult` the contract multiplier.
async fn okx(client: &Client) -> anyhow::Result<Vec<Listing>> {
    let v = get_json(client, "https://www.okx.com/api/v5/public/instruments?instType=SWAP").await?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("data").and_then(Value::as_array) {
        for it in arr {
            let settle = it.get("settleCcy").and_then(Value::as_str).unwrap_or("");
            let state = it.get("state").and_then(Value::as_str).unwrap_or("");
            let ct = it.get("ctType").and_then(Value::as_str).unwrap_or("");
            let inst = it.get("instId").and_then(Value::as_str).unwrap_or("");
            if settle == "USDT" && state == "live" && ct == "linear" {
                if let Some(base) = inst.split('-').next() {
                    out.push(Listing {
                        base: up(base),
                        contract_size: contract_size(it, "ctVal") * contract_size(it, "ctMult"),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// MEXC contract depth is expressed in contracts; `contractSize` converts.
async fn mexc(client: &Client) -> anyhow::Result<Vec<Listing>> {
    let v = get_json(client, "https://contract.mexc.com/api/v1/contract/detail").await?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("data").and_then(Value::as_array) {
        for it in arr {
            let quote = it.get("quoteCoin").and_then(Value::as_str).unwrap_or("");
            // state 0 == enabled; futureType/isHidden also gate visibility.
            let state = it.get("state").and_then(Value::as_i64).unwrap_or(-1);
            if quote == "USDT" && state == 0 {
                if let Some(base) = it.get("baseCoin").and_then(Value::as_str) {
                    out.push(Listing {
                        base: up(base),
                        contract_size: contract_size(it, "contractSize"),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Bitget v2 mix books are quoted in the base coin.
async fn bitget(client: &Client) -> anyhow::Result<Vec<Listing>> {
    let v = get_json(
        client,
        "https://api.bitget.com/api/v2/mix/market/contracts?productType=usdt-futures",
    )
    .await?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("data").and_then(Value::as_array) {
        for it in arr {
            let status = it.get("symbolStatus").and_then(Value::as_str).unwrap_or("");
            if status == "normal" {
                if let Some(base) = it.get("baseCoin").and_then(Value::as_str) {
                    out.push(Listing::base_units(up(base)));
                }
            }
        }
    }
    Ok(out)
}

/// Gate futures books are quoted in **contracts**; `quanto_multiplier` is the
/// base amount each contract represents.
async fn gate(client: &Client) -> anyhow::Result<Vec<Listing>> {
    let v = get_json(client, "https://api.gateio.ws/api/v4/futures/usdt/contracts").await?;
    let mut out = Vec::new();
    if let Some(arr) = v.as_array() {
        for it in arr {
            let name = it.get("name").and_then(Value::as_str).unwrap_or("");
            let delisting = it.get("in_delisting").and_then(Value::as_bool).unwrap_or(false);
            if !delisting {
                if let Some(base) = name.split('_').next() {
                    if !base.is_empty() {
                        out.push(Listing {
                            base: up(base),
                            contract_size: contract_size(it, "quanto_multiplier"),
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}

/// CoinEx v2 futures books are quoted in the base asset.
async fn coinex(client: &Client) -> anyhow::Result<Vec<Listing>> {
    let v = get_json(client, "https://api.coinex.com/v2/futures/market").await?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("data").and_then(Value::as_array) {
        for it in arr {
            let quote = it.get("quote_ccy").and_then(Value::as_str).unwrap_or("");
            if quote == "USDT" {
                if let Some(base) = it.get("base_ccy").and_then(Value::as_str) {
                    out.push(Listing::base_units(up(base)));
                }
            }
        }
    }
    Ok(out)
}

/// KuCoin futures books are quoted in lots; `multiplier` is base per lot.
async fn kucoin(client: &Client) -> anyhow::Result<Vec<Listing>> {
    let v = get_json(client, "https://api-futures.kucoin.com/api/v1/contracts/active").await?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("data").and_then(Value::as_array) {
        for it in arr {
            let quote = it.get("quoteCurrency").and_then(Value::as_str).unwrap_or("");
            let status = it.get("status").and_then(Value::as_str).unwrap_or("");
            if quote == "USDT" && status == "Open" {
                if let Some(base) = it.get("baseCurrency").and_then(Value::as_str) {
                    out.push(Listing {
                        base: normalize_base(base),
                        contract_size: contract_size(it, "multiplier"),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Phemex USDT perps (`PerpetualV2`) publish real numbers on the `*_p`
/// channels, in base units. The legacy inverse products use scaled integers and
/// are not screened here.
async fn phemex(client: &Client) -> anyhow::Result<Vec<Listing>> {
    let v = get_json(client, "https://api.phemex.com/public/products").await?;
    let mut out = Vec::new();
    // USDT perps live under perpProductsV2 (settle in USDT).
    let arrays = [
        v.pointer("/data/perpProductsV2").and_then(Value::as_array),
        v.pointer("/data/products").and_then(Value::as_array),
    ];
    for arr in arrays.into_iter().flatten() {
        for it in arr {
            let quote = it.get("quoteCurrency").and_then(Value::as_str).unwrap_or("");
            let ptype = it.get("type").and_then(Value::as_str).unwrap_or("");
            let status = it.get("status").and_then(Value::as_str).unwrap_or("");
            if quote == "USDT" && ptype == "PerpetualV2" && (status == "Listed" || status.is_empty())
            {
                if let Some(base) = it.get("baseCurrency").and_then(Value::as_str) {
                    out.push(Listing::base_units(up(base)));
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn contract_size_reads_strings_and_numbers() {
        assert_eq!(contract_size(&json!({"m": "0.001"}), "m"), Decimal::from_str("0.001").unwrap());
        assert_eq!(contract_size(&json!({"m": 10}), "m"), Decimal::from(10));
    }

    #[test]
    fn contract_size_defaults_to_one_when_unusable() {
        // Missing, zero, negative, and unparseable all mean "do not scale".
        assert_eq!(contract_size(&json!({}), "m"), Decimal::ONE);
        assert_eq!(contract_size(&json!({"m": "0"}), "m"), Decimal::ONE);
        assert_eq!(contract_size(&json!({"m": -5}), "m"), Decimal::ONE);
        assert_eq!(contract_size(&json!({"m": "1 SOL"}), "m"), Decimal::ONE);
    }
}
