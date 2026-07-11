//! KuCoin Futures (USDT perp) connector.
//!
//! DOCS (VERIFY against live docs — not compile-checked here):
//!   Bootstrap: POST https://api-futures.kucoin.com/api/v1/bullet-public
//!              → { data: { token, instanceServers:[{ endpoint }] } }
//!   Connect:   {endpoint}?token={token}&connectId=...
//!   Book:      topic "/contractMarket/level2Depth5:{SYM}" (top-5 snapshot, stateless)
//!   Funding:   topic "/contract/instrument:{SYM}" (subject carries fundingRate)
//!   Keepalive: {"type":"ping","id":"..."}
//!   Symbol:    "{BASE*}USDTM"  (BTC → XBT)

use crate::common::{SymbolCtx, WsExchange};
use crate::util::{dec_from_json, finalize_sides, level_from_pair};
use domain::{BookLevel, Decimal, ExchangeId, Instrument, MarketUpdate, TopBook};
use serde_json::Value;
use std::time::{Duration, Instant};

pub struct Kucoin {
    depth: usize,
}

impl Kucoin {
    pub fn new(depth: usize) -> Self {
        Kucoin {
            depth: depth.clamp(1, 5),
        }
    }

    fn base_code(base: &str) -> String {
        // KuCoin uses XBT for Bitcoin on futures.
        if base.eq_ignore_ascii_case("BTC") {
            "XBT".to_string()
        } else {
            base.to_uppercase()
        }
    }
}

fn levels(v: Option<&Value>) -> Vec<BookLevel> {
    v.and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(level_from_pair).collect())
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl WsExchange for Kucoin {
    fn id(&self) -> ExchangeId {
        ExchangeId::Kucoin
    }

    fn ws_url(&self) -> String {
        // Unused: resolve_ws_url overrides with the bootstrapped endpoint.
        "wss://ws-api-futures.kucoin.com/".to_string()
    }

    async fn resolve_ws_url(&self) -> anyhow::Result<String> {
        let client = reqwest::Client::new();
        let resp: Value = client
            .post("https://api-futures.kucoin.com/api/v1/bullet-public")
            .send()
            .await?
            .json()
            .await?;
        let data = resp
            .get("data")
            .ok_or_else(|| anyhow::anyhow!("kucoin bullet-public: missing data"))?;
        let token = data
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("kucoin bullet-public: missing token"))?;
        let endpoint = data
            .get("instanceServers")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|s| s.get("endpoint"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("kucoin bullet-public: missing endpoint"))?;
        Ok(format!("{endpoint}?token={token}&connectId=arb-screener"))
    }

    fn to_symbol(&self, inst: &Instrument) -> String {
        format!("{}{}M", Self::base_code(&inst.base), inst.quote)
    }

    fn subscribe_frames(&self, symbols: &[Instrument]) -> Vec<String> {
        let syms: Vec<String> = symbols.iter().map(|i| self.to_symbol(i)).collect();
        let mut frames = Vec::new();
        // Batch symbols per topic (comma-separated), chunked to stay small.
        for chunk in syms.chunks(10) {
            let joined = chunk.join(",");
            frames.push(
                serde_json::json!({
                    "id": "sub-l2", "type": "subscribe",
                    "topic": format!("/contractMarket/level2Depth5:{joined}"),
                    "response": true
                })
                .to_string(),
            );
            frames.push(
                serde_json::json!({
                    "id": "sub-inst", "type": "subscribe",
                    "topic": format!("/contract/instrument:{joined}"),
                    "response": true
                })
                .to_string(),
            );
        }
        frames
    }

    fn ping_frame(&self) -> Option<String> {
        Some(r#"{"type":"ping","id":"keepalive"}"#.to_string())
    }

    fn ping_interval(&self) -> Duration {
        Duration::from_secs(15)
    }

    fn parse(&self, text: &str, ctx: &SymbolCtx) -> Vec<MarketUpdate> {
        let Ok(v): Result<Value, _> = serde_json::from_str(text) else {
            return vec![];
        };
        if v.get("type").and_then(Value::as_str) != Some("message") {
            return vec![]; // welcome / ack / pong
        }
        let topic = v.get("topic").and_then(Value::as_str).unwrap_or("");
        let Some((prefix, symbol)) = topic.split_once(':') else {
            return vec![];
        };
        let Some(inst) = ctx.lookup(symbol) else {
            return vec![];
        };
        let Some(data) = v.get("data") else {
            return vec![];
        };

        if prefix.ends_with("level2Depth5") {
            let bids = levels(data.get("bids"));
            let asks = levels(data.get("asks"));
            let (bids, asks) = finalize_sides(bids, asks, self.depth);
            if bids.is_empty() || asks.is_empty() {
                return vec![];
            }
            let exch_ts = data.get("timestamp").and_then(Value::as_i64);
            return vec![MarketUpdate::Book {
                exchange: ExchangeId::Kucoin,
                instrument: inst.clone(),
                book: TopBook {
                    bids,
                    asks,
                    recv_ts: Instant::now(),
                    exch_ts,
                },
            }];
        }

        if prefix.ends_with("instrument") {
            if let Some(rate) = data.get("fundingRate").and_then(dec_from_json) {
                return vec![MarketUpdate::Funding {
                    exchange: ExchangeId::Kucoin,
                    instrument: inst.clone(),
                    rate,
                    interval_hours: Decimal::from(8),
                    next_ts: 0,
                }];
            }
        }
        vec![]
    }
}
