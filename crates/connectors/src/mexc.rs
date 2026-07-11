//! MEXC contract (USDT perp) connector.
//!
//! DOCS (VERIFY against live docs — not compile-checked here):
//!   WS:        wss://contract.mexc.com/edge
//!   Book:      sub.depth.full → push.depth.full (full top-N snapshot, stateless)
//!   Ticker:    sub.ticker → push.ticker (carries fundingRate)
//!   Subscribe: {"method":"sub.depth.full","param":{"symbol":"BTC_USDT","limit":20}}
//!   Keepalive: {"method":"ping"} → {"channel":"pong"}
//!   Symbol:    "{BASE}_{QUOTE}"

use crate::common::{SymbolCtx, WsExchange};
use crate::util::{dec_from_json, finalize_sides, level_from_pair};
use domain::{BookLevel, Decimal, ExchangeId, Instrument, MarketUpdate, TopBook};
use serde_json::Value;
use std::time::{Duration, Instant};

pub struct Mexc {
    depth: usize,
}

impl Mexc {
    pub fn new(depth: usize) -> Self {
        Mexc {
            depth: depth.clamp(1, 20),
        }
    }
}

fn levels(v: Option<&Value>) -> Vec<BookLevel> {
    v.and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(level_from_pair).collect())
        .unwrap_or_default()
}

impl WsExchange for Mexc {
    fn id(&self) -> ExchangeId {
        ExchangeId::Mexc
    }

    fn ws_url(&self) -> String {
        "wss://contract.mexc.com/edge".to_string()
    }

    fn to_symbol(&self, inst: &Instrument) -> String {
        format!("{}_{}", inst.base, inst.quote)
    }

    fn subscribe_frames(&self, symbols: &[Instrument]) -> Vec<String> {
        let mut frames = Vec::new();
        for inst in symbols {
            let s = self.to_symbol(inst);
            frames.push(
                serde_json::json!({
                    "method": "sub.depth.full",
                    "param": { "symbol": s, "limit": self.depth }
                })
                .to_string(),
            );
            frames.push(
                serde_json::json!({ "method": "sub.ticker", "param": { "symbol": s } })
                    .to_string(),
            );
        }
        frames
    }

    fn ping_frame(&self) -> Option<String> {
        Some(r#"{"method":"ping"}"#.to_string())
    }

    fn ping_interval(&self) -> Duration {
        Duration::from_secs(15)
    }

    fn parse(&self, text: &str, ctx: &SymbolCtx) -> Vec<MarketUpdate> {
        let Ok(v): Result<Value, _> = serde_json::from_str(text) else {
            return vec![];
        };
        let channel = v.get("channel").and_then(Value::as_str).unwrap_or("");
        let symbol = v.get("symbol").and_then(Value::as_str).unwrap_or("");
        let Some(inst) = ctx.lookup(symbol) else {
            return vec![];
        };
        let data = v.get("data");
        let exch_ts = v.get("ts").and_then(Value::as_i64);

        match channel {
            "push.depth.full" | "push.depth" => {
                let Some(d) = data else { return vec![] };
                let bids = levels(d.get("bids"));
                let asks = levels(d.get("asks"));
                let (bids, asks) = finalize_sides(bids, asks, self.depth);
                if bids.is_empty() || asks.is_empty() {
                    return vec![];
                }
                vec![MarketUpdate::Book {
                    exchange: ExchangeId::Mexc,
                    instrument: inst.clone(),
                    book: TopBook {
                        bids,
                        asks,
                        recv_ts: Instant::now(),
                        exch_ts,
                    },
                }]
            }
            "push.ticker" => {
                let Some(d) = data else { return vec![] };
                let Some(rate) = d.get("fundingRate").and_then(dec_from_json) else {
                    return vec![];
                };
                vec![MarketUpdate::Funding {
                    exchange: ExchangeId::Mexc,
                    instrument: inst.clone(),
                    rate,
                    interval_hours: Decimal::from(8),
                    next_ts: 0,
                }]
            }
            _ => vec![],
        }
    }
}
