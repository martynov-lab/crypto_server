//! Per-exchange PUBLIC instrument-list fetchers. Each returns the canonical
//! base assets that have a **USDT-settled perpetual** listed and actively
//! trading on that venue. Verified against live responses.

use domain::ExchangeId;
use reqwest::Client;
use serde_json::Value;

pub async fn fetch(client: &Client, exchange: ExchangeId) -> anyhow::Result<Vec<String>> {
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

async fn bybit(client: &Client) -> anyhow::Result<Vec<String>> {
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
                    out.push(up(base));
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

async fn okx(client: &Client) -> anyhow::Result<Vec<String>> {
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
                    out.push(up(base));
                }
            }
        }
    }
    Ok(out)
}

async fn mexc(client: &Client) -> anyhow::Result<Vec<String>> {
    let v = get_json(client, "https://contract.mexc.com/api/v1/contract/detail").await?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("data").and_then(Value::as_array) {
        for it in arr {
            let quote = it.get("quoteCoin").and_then(Value::as_str).unwrap_or("");
            // state 0 == enabled; futureType/isHidden also gate visibility.
            let state = it.get("state").and_then(Value::as_i64).unwrap_or(-1);
            if quote == "USDT" && state == 0 {
                if let Some(base) = it.get("baseCoin").and_then(Value::as_str) {
                    out.push(up(base));
                }
            }
        }
    }
    Ok(out)
}

async fn bitget(client: &Client) -> anyhow::Result<Vec<String>> {
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
                    out.push(up(base));
                }
            }
        }
    }
    Ok(out)
}

async fn gate(client: &Client) -> anyhow::Result<Vec<String>> {
    let v = get_json(client, "https://api.gateio.ws/api/v4/futures/usdt/contracts").await?;
    let mut out = Vec::new();
    if let Some(arr) = v.as_array() {
        for it in arr {
            let name = it.get("name").and_then(Value::as_str).unwrap_or("");
            let delisting = it.get("in_delisting").and_then(Value::as_bool).unwrap_or(false);
            if !delisting {
                if let Some(base) = name.split('_').next() {
                    if !base.is_empty() {
                        out.push(up(base));
                    }
                }
            }
        }
    }
    Ok(out)
}

async fn coinex(client: &Client) -> anyhow::Result<Vec<String>> {
    let v = get_json(client, "https://api.coinex.com/v2/futures/market").await?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("data").and_then(Value::as_array) {
        for it in arr {
            let quote = it.get("quote_ccy").and_then(Value::as_str).unwrap_or("");
            if quote == "USDT" {
                if let Some(base) = it.get("base_ccy").and_then(Value::as_str) {
                    out.push(up(base));
                }
            }
        }
    }
    Ok(out)
}

async fn kucoin(client: &Client) -> anyhow::Result<Vec<String>> {
    let v = get_json(client, "https://api-futures.kucoin.com/api/v1/contracts/active").await?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("data").and_then(Value::as_array) {
        for it in arr {
            let quote = it.get("quoteCurrency").and_then(Value::as_str).unwrap_or("");
            let status = it.get("status").and_then(Value::as_str).unwrap_or("");
            if quote == "USDT" && status == "Open" {
                if let Some(base) = it.get("baseCurrency").and_then(Value::as_str) {
                    out.push(normalize_base(base));
                }
            }
        }
    }
    Ok(out)
}

async fn phemex(client: &Client) -> anyhow::Result<Vec<String>> {
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
                    out.push(up(base));
                }
            }
        }
    }
    Ok(out)
}
