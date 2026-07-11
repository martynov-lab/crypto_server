//! OKX v5 public (USDT perp / SWAP) connector.
//!
//! DOCS (verify against live docs before production):
//!   WS:        wss://ws.okx.com:8443/ws/v5/public
//!   Book:      channel "books5" — full top-5 snapshot every push (stateless)
//!   Funding:   channel "funding-rate"
//!   Subscribe: {"op":"subscribe","args":[{"channel":"books5","instId":"BTC-USDT-SWAP"}, ...]}
//!   Keepalive: send literal text "ping" (server replies "pong") ~every 25s
//!   instId:    "{BASE}-{QUOTE}-SWAP"

use crate::common::{SymbolCtx, WsExchange};
use crate::util::{dec_from_str, finalize_sides, level_from_pair};
use domain::{BookLevel, Decimal, ExchangeId, Instrument, MarketUpdate, TopBook};
use serde_json::Value;
use std::time::{Duration, Instant};

pub struct Okx {
    depth: usize,
}

impl Okx {
    pub fn new(depth: usize) -> Self {
        // books5 delivers 5 levels; keep the configured depth as an upper bound.
        Okx {
            depth: depth.min(5).max(1),
        }
    }
}

fn levels(v: Option<&Value>) -> Vec<BookLevel> {
    v.and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(level_from_pair).collect())
        .unwrap_or_default()
}

impl WsExchange for Okx {
    fn id(&self) -> ExchangeId {
        ExchangeId::Okx
    }

    fn ws_url(&self) -> String {
        "wss://ws.okx.com:8443/ws/v5/public".to_string()
    }

    fn to_symbol(&self, inst: &Instrument) -> String {
        format!("{}-{}-SWAP", inst.base, inst.quote)
    }

    fn subscribe_frames(&self, symbols: &[Instrument]) -> Vec<String> {
        let mut frames = Vec::new();
        for chunk in symbols.chunks(20) {
            let mut args: Vec<Value> = Vec::new();
            for inst in chunk {
                let s = self.to_symbol(inst);
                args.push(serde_json::json!({ "channel": "books5", "instId": s }));
                args.push(serde_json::json!({ "channel": "funding-rate", "instId": s }));
            }
            frames.push(serde_json::json!({ "op": "subscribe", "args": args }).to_string());
        }
        frames
    }

    fn ping_frame(&self) -> Option<String> {
        Some("ping".to_string())
    }

    fn ping_interval(&self) -> Duration {
        Duration::from_secs(25)
    }

    fn parse(&self, text: &str, ctx: &SymbolCtx) -> Vec<MarketUpdate> {
        if text == "pong" {
            return vec![];
        }
        let Ok(v): Result<Value, _> = serde_json::from_str(text) else {
            return vec![];
        };
        let Some(arg) = v.get("arg") else {
            return vec![]; // event ack/error
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
                let exch_ts = d.get("ts").and_then(Value::as_str).and_then(|s| s.parse().ok());
                vec![MarketUpdate::Book {
                    exchange: ExchangeId::Okx,
                    instrument: inst.clone(),
                    book: TopBook {
                        bids,
                        asks,
                        recv_ts: Instant::now(),
                        exch_ts,
                    },
                }]
            }
            "funding-rate" => {
                let Some(d) = data.first() else { return vec![] };
                let Some(rate) = d.get("fundingRate").and_then(Value::as_str).and_then(dec_from_str)
                else {
                    return vec![];
                };
                let next_ts = d
                    .get("nextFundingTime")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                vec![MarketUpdate::Funding {
                    exchange: ExchangeId::Okx,
                    instrument: inst.clone(),
                    rate,
                    interval_hours: Decimal::from(8),
                    next_ts,
                }]
            }
            _ => vec![],
        }
    }
}
