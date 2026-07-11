//! CoinEx v2 futures (USDT perp) connector.
//!
//! DOCS (VERIFY against live docs — not compile-checked here):
//!   WS:        wss://socket.coinex.com/v2/futures
//!   Book:      depth.subscribe → depth.update (is_full=true snapshot, else delta)
//!   Subscribe: {"method":"depth.subscribe","params":{"market_list":[["BTCUSDT",20,"0",true]]},"id":1}
//!   Keepalive: {"method":"server.ping","params":{},"id":1}
//!   Symbol:    "{BASE}{QUOTE}"
//!
//! Funding is not subscribed here (optional for Phase 1).

use crate::book::DeltaBook;
use crate::common::{SymbolCtx, WsExchange};
use crate::util::level_from_pair;
use domain::{BookLevel, ExchangeId, Instrument, MarketUpdate, TopBook};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Coinex {
    depth: usize,
    books: Mutex<HashMap<Instrument, DeltaBook>>,
}

impl Coinex {
    pub fn new(depth: usize) -> Self {
        Coinex {
            depth: depth.clamp(1, 50),
            books: Mutex::new(HashMap::new()),
        }
    }
}

fn levels(v: Option<&Value>) -> Vec<BookLevel> {
    v.and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(level_from_pair).collect())
        .unwrap_or_default()
}

impl WsExchange for Coinex {
    fn id(&self) -> ExchangeId {
        ExchangeId::Coinex
    }

    fn ws_url(&self) -> String {
        "wss://socket.coinex.com/v2/futures".to_string()
    }

    fn to_symbol(&self, inst: &Instrument) -> String {
        format!("{}{}", inst.base, inst.quote)
    }

    fn subscribe_frames(&self, symbols: &[Instrument]) -> Vec<String> {
        let market_list: Vec<Value> = symbols
            .iter()
            .map(|i| serde_json::json!([self.to_symbol(i), self.depth, "0", true]))
            .collect();
        vec![serde_json::json!({
            "method": "depth.subscribe",
            "params": { "market_list": market_list },
            "id": 1
        })
        .to_string()]
    }

    fn on_reconnect(&self) {
        if let Ok(mut b) = self.books.lock() {
            b.clear();
        }
    }

    fn ping_frame(&self) -> Option<String> {
        Some(r#"{"method":"server.ping","params":{},"id":1}"#.to_string())
    }

    fn ping_interval(&self) -> Duration {
        Duration::from_secs(20)
    }

    fn parse(&self, text: &str, ctx: &SymbolCtx) -> Vec<MarketUpdate> {
        let Ok(v): Result<Value, _> = serde_json::from_str(text) else {
            return vec![];
        };
        if v.get("method").and_then(Value::as_str) != Some("depth.update") {
            return vec![];
        }
        let Some(data) = v.get("data") else {
            return vec![];
        };
        let market = data.get("market").and_then(Value::as_str).unwrap_or("");
        let Some(inst) = ctx.lookup(market) else {
            return vec![];
        };
        let is_full = data.get("is_full").and_then(Value::as_bool).unwrap_or(true);
        let depth = data.get("depth");
        let bids = levels(depth.and_then(|d| d.get("bids")));
        let asks = levels(depth.and_then(|d| d.get("asks")));
        let exch_ts = depth.and_then(|d| d.get("updated_at")).and_then(Value::as_i64);

        let mut guard = match self.books.lock() {
            Ok(g) => g,
            Err(_) => return vec![],
        };
        let book = guard.entry(inst.clone()).or_default();
        if is_full {
            book.apply_snapshot(&bids, &asks);
        } else {
            book.apply_delta(&bids, &asks);
        }
        if book.is_empty() {
            return vec![];
        }
        let (bids, asks) = book.top_n(self.depth);
        vec![MarketUpdate::Book {
            exchange: ExchangeId::Coinex,
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
