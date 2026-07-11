//! Bitget v2 mix (USDT perp) connector.
//!
//! DOCS (VERIFY against live docs — not compile-checked here):
//!   WS:        wss://ws.bitget.com/v2/ws/public
//!   Book:      channel "books5" (full top-5 snapshot, stateless)
//!   Ticker:    channel "ticker" (carries fundingRate)
//!   Subscribe: {"op":"subscribe","args":[{"instType":"USDT-FUTURES","channel":"books5","instId":"BTCUSDT"}]}
//!   Keepalive: text "ping" → "pong"
//!   Symbol:    "{BASE}{QUOTE}"

use crate::common::{SymbolCtx, WsExchange};
use crate::util::{dec_from_json, finalize_sides, level_from_pair};
use domain::{BookLevel, Decimal, ExchangeId, Instrument, MarketUpdate, TopBook};
use serde_json::Value;
use std::time::{Duration, Instant};

const INST_TYPE: &str = "USDT-FUTURES";

pub struct Bitget {
    depth: usize,
}

impl Bitget {
    pub fn new(depth: usize) -> Self {
        Bitget {
            depth: depth.clamp(1, 5),
        }
    }
}

fn levels(v: Option<&Value>) -> Vec<BookLevel> {
    v.and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(level_from_pair).collect())
        .unwrap_or_default()
}

impl WsExchange for Bitget {
    fn id(&self) -> ExchangeId {
        ExchangeId::Bitget
    }

    fn ws_url(&self) -> String {
        "wss://ws.bitget.com/v2/ws/public".to_string()
    }

    fn to_symbol(&self, inst: &Instrument) -> String {
        format!("{}{}", inst.base, inst.quote)
    }

    fn subscribe_frames(&self, symbols: &[Instrument]) -> Vec<String> {
        let mut args: Vec<Value> = Vec::new();
        for inst in symbols {
            let s = self.to_symbol(inst);
            args.push(serde_json::json!({ "instType": INST_TYPE, "channel": "books5", "instId": s }));
            args.push(serde_json::json!({ "instType": INST_TYPE, "channel": "ticker", "instId": s }));
        }
        vec![serde_json::json!({ "op": "subscribe", "args": args }).to_string()]
    }

    fn ping_frame(&self) -> Option<String> {
        Some("ping".to_string())
    }

    fn ping_interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn parse(&self, text: &str, ctx: &SymbolCtx) -> Vec<MarketUpdate> {
        if text == "pong" {
            return vec![];
        }
        let Ok(v): Result<Value, _> = serde_json::from_str(text) else {
            return vec![];
        };
        let Some(arg) = v.get("arg") else {
            return vec![]; // subscribe ack / error
        };
        let channel = arg.get("channel").and_then(Value::as_str).unwrap_or("");
        let inst_id = arg.get("instId").and_then(Value::as_str).unwrap_or("");
        let Some(inst) = ctx.lookup(inst_id) else {
            return vec![];
        };
        let Some(data) = v.get("data").and_then(Value::as_array) else {
            return vec![];
        };

        match channel {
            "books5" => {
                let Some(d) = data.first() else { return vec![] };
                let bids = levels(d.get("bids"));
                let asks = levels(d.get("asks"));
                let (bids, asks) = finalize_sides(bids, asks, self.depth);
                if bids.is_empty() || asks.is_empty() {
                    return vec![];
                }
                let exch_ts = d.get("ts").and_then(|x| x.as_str().and_then(|s| s.parse().ok()));
                vec![MarketUpdate::Book {
                    exchange: ExchangeId::Bitget,
                    instrument: inst.clone(),
                    book: TopBook {
                        bids,
                        asks,
                        recv_ts: Instant::now(),
                        exch_ts,
                    },
                }]
            }
            "ticker" => {
                let Some(d) = data.first() else { return vec![] };
                let Some(rate) = d.get("fundingRate").and_then(dec_from_json) else {
                    return vec![];
                };
                vec![MarketUpdate::Funding {
                    exchange: ExchangeId::Bitget,
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
