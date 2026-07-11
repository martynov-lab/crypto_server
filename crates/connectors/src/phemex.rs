//! Phemex USDT perpetual connector.
//!
//! DOCS (VERIFY against live docs — not compile-checked here):
//!   WS:        wss://ws.phemex.com
//!   Book:      orderbook_p.subscribe → { orderbook_p:{asks,bids}, type } snapshot/incremental
//!   Subscribe: {"id":1,"method":"orderbook_p.subscribe","params":["BTCUSDT"]}
//!   Keepalive: {"id":0,"method":"server.ping","params":[]}
//!   Symbol:    "{BASE}{QUOTE}"
//!
//! NOTE: USDT-margined ("*_p") channels use real numbers; legacy inverse
//! contracts use scaled integers. Verify the price/size scale for your symbols.

use crate::book::DeltaBook;
use crate::common::{SymbolCtx, WsExchange};
use crate::util::level_from_pair;
use domain::{BookLevel, ExchangeId, Instrument, MarketUpdate, TopBook};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Phemex {
    depth: usize,
    books: Mutex<HashMap<Instrument, DeltaBook>>,
}

impl Phemex {
    pub fn new(depth: usize) -> Self {
        Phemex {
            depth: depth.clamp(1, 30),
            books: Mutex::new(HashMap::new()),
        }
    }
}

fn levels(v: Option<&Value>) -> Vec<BookLevel> {
    v.and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(level_from_pair).collect())
        .unwrap_or_default()
}

impl WsExchange for Phemex {
    fn id(&self) -> ExchangeId {
        ExchangeId::Phemex
    }

    fn ws_url(&self) -> String {
        "wss://ws.phemex.com".to_string()
    }

    fn to_symbol(&self, inst: &Instrument) -> String {
        format!("{}{}", inst.base, inst.quote)
    }

    fn subscribe_frames(&self, symbols: &[Instrument]) -> Vec<String> {
        symbols
            .iter()
            .enumerate()
            .map(|(i, inst)| {
                serde_json::json!({
                    "id": i as i64 + 1,
                    "method": "orderbook_p.subscribe",
                    "params": [self.to_symbol(inst)]
                })
                .to_string()
            })
            .collect()
    }

    fn on_reconnect(&self) {
        if let Ok(mut b) = self.books.lock() {
            b.clear();
        }
    }

    fn ping_frame(&self) -> Option<String> {
        Some(r#"{"id":0,"method":"server.ping","params":[]}"#.to_string())
    }

    fn ping_interval(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn parse(&self, text: &str, ctx: &SymbolCtx) -> Vec<MarketUpdate> {
        let Ok(v): Result<Value, _> = serde_json::from_str(text) else {
            return vec![];
        };
        let Some(ob) = v.get("orderbook_p") else {
            return vec![]; // ack / pong / other
        };
        let symbol = v.get("symbol").and_then(Value::as_str).unwrap_or("");
        let Some(inst) = ctx.lookup(symbol) else {
            return vec![];
        };
        let bids = levels(ob.get("bids"));
        let asks = levels(ob.get("asks"));
        let is_snapshot = v.get("type").and_then(Value::as_str) == Some("snapshot");
        let exch_ts = v.get("timestamp").and_then(Value::as_i64);

        let mut guard = match self.books.lock() {
            Ok(g) => g,
            Err(_) => return vec![],
        };
        let book = guard.entry(inst.clone()).or_default();
        if is_snapshot {
            book.apply_snapshot(&bids, &asks);
        } else {
            book.apply_delta(&bids, &asks);
        }
        if book.is_empty() {
            return vec![];
        }
        let (bids, asks) = book.top_n(self.depth);
        vec![MarketUpdate::Book {
            exchange: ExchangeId::Phemex,
            instrument: inst.clone(),
            book: TopBook {
                bids,
                asks,
                recv_ts: Instant::now(),
                exch_ts,
            },
        }]
    }
}
