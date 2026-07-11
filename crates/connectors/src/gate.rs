//! Gate.io USDT-settled futures (perp) connector.
//!
//! DOCS (VERIFY against live docs — not compile-checked here):
//!   WS:        wss://fx-ws.gateio.ws/v4/ws/usdt
//!   Book:      channel "futures.book_ticker" (best bid/ask + sizes) — 1 level
//!   Ticker:    channel "futures.tickers" (carries funding_rate)
//!   Subscribe: {"channel":"futures.book_ticker","event":"subscribe","payload":["BTC_USDT"]}
//!   Keepalive: {"channel":"futures.ping"}
//!   Symbol:    "{BASE}_{QUOTE}"
//!
//! NOTE: book_ticker sizes are in *contracts*, not base units. Executable
//! notional here is approximate until contract multipliers are wired. For deep
//! books use "futures.order_book"/"futures.order_book_update" (snapshot+delta).

use crate::common::{SymbolCtx, WsExchange};
use crate::util::dec_from_json;
use domain::{BookLevel, Decimal, ExchangeId, Instrument, MarketUpdate, TopBook};
use serde_json::Value;
use std::time::{Duration, Instant};

pub struct Gate;

impl Gate {
    pub fn new(_depth: usize) -> Self {
        Gate
    }
}

impl WsExchange for Gate {
    fn id(&self) -> ExchangeId {
        ExchangeId::Gate
    }

    fn ws_url(&self) -> String {
        "wss://fx-ws.gateio.ws/v4/ws/usdt".to_string()
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
                    "channel": "futures.book_ticker", "event": "subscribe", "payload": [s]
                })
                .to_string(),
            );
            frames.push(
                serde_json::json!({
                    "channel": "futures.tickers", "event": "subscribe", "payload": [s]
                })
                .to_string(),
            );
        }
        frames
    }

    fn ping_frame(&self) -> Option<String> {
        Some(r#"{"channel":"futures.ping"}"#.to_string())
    }

    fn ping_interval(&self) -> Duration {
        Duration::from_secs(20)
    }

    fn parse(&self, text: &str, ctx: &SymbolCtx) -> Vec<MarketUpdate> {
        let Ok(v): Result<Value, _> = serde_json::from_str(text) else {
            return vec![];
        };
        let channel = v.get("channel").and_then(Value::as_str).unwrap_or("");
        let event = v.get("event").and_then(Value::as_str).unwrap_or("");
        if event == "subscribe" {
            return vec![]; // ack
        }
        let result = v.get("result");

        match channel {
            "futures.book_ticker" => {
                let Some(r) = result else { return vec![] };
                let sym = r.get("s").and_then(Value::as_str).unwrap_or("");
                let Some(inst) = ctx.lookup(sym) else {
                    return vec![];
                };
                let bid = dec_from_json(r.get("b").unwrap_or(&Value::Null));
                let bid_sz = dec_from_json(r.get("B").unwrap_or(&Value::Null));
                let ask = dec_from_json(r.get("a").unwrap_or(&Value::Null));
                let ask_sz = dec_from_json(r.get("A").unwrap_or(&Value::Null));
                let (Some(bid), Some(bid_sz), Some(ask), Some(ask_sz)) =
                    (bid, bid_sz, ask, ask_sz)
                else {
                    return vec![];
                };
                let exch_ts = r.get("t").and_then(Value::as_i64);
                vec![MarketUpdate::Book {
                    exchange: ExchangeId::Gate,
                    instrument: inst.clone(),
                    book: TopBook {
                        bids: vec![BookLevel::new(bid, bid_sz)],
                        asks: vec![BookLevel::new(ask, ask_sz)],
                        recv_ts: Instant::now(),
                        exch_ts,
                    },
                }]
            }
            "futures.tickers" => {
                let Some(arr) = result.and_then(Value::as_array) else {
                    return vec![];
                };
                let mut out = Vec::new();
                for r in arr {
                    let sym = r.get("contract").and_then(Value::as_str).unwrap_or("");
                    let Some(inst) = ctx.lookup(sym) else {
                        continue;
                    };
                    if let Some(rate) = r.get("funding_rate").and_then(dec_from_json) {
                        out.push(MarketUpdate::Funding {
                            exchange: ExchangeId::Gate,
                            instrument: inst.clone(),
                            rate,
                            interval_hours: Decimal::from(8),
                            next_ts: 0,
                        });
                    }
                }
                out
            }
            _ => vec![],
        }
    }
}
