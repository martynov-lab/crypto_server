//! Bybit v5 linear (USDT perp) public connector.
//!
//! DOCS (verify against live docs before production):
//!   WS:        wss://stream.bybit.com/v5/public/linear
//!   Book:      topic "orderbook.{depth}.{SYMBOL}" — snapshot then deltas
//!   Ticker:    topic "tickers.{SYMBOL}" — carries fundingRate/nextFundingTime
//!   Subscribe: {"op":"subscribe","args":["orderbook.50.BTCUSDT","tickers.BTCUSDT"]}
//!   Keepalive: {"op":"ping"} every ~20s

use crate::book::DeltaBook;
use crate::common::{SymbolCtx, WsExchange};
use crate::util::{dec_from_str, level_from_pair};
use domain::{
    BookLevel, Decimal, ExchangeId, FundingInfo, Instrument, MarketUpdate, TopBook,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Bybit {
    depth: usize,
    books: Mutex<HashMap<Instrument, DeltaBook>>,
}

impl Bybit {
    pub fn new(depth: usize) -> Self {
        // Bybit linear depth channels: 1, 50, 200, 500. Snap to the nearest valid.
        let depth = match depth {
            0..=1 => 1,
            2..=50 => 50,
            51..=200 => 200,
            _ => 500,
        };
        Bybit {
            depth,
            books: Mutex::new(HashMap::new()),
        }
    }
}

fn levels(v: Option<&Value>) -> Vec<BookLevel> {
    v.and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(level_from_pair).collect())
        .unwrap_or_default()
}

impl WsExchange for Bybit {
    fn id(&self) -> ExchangeId {
        ExchangeId::Bybit
    }

    fn ws_url(&self) -> String {
        "wss://stream.bybit.com/v5/public/linear".to_string()
    }

    fn to_symbol(&self, inst: &Instrument) -> String {
        format!("{}{}", inst.base, inst.quote)
    }

    fn subscribe_frames(&self, symbols: &[Instrument]) -> Vec<String> {
        // Chunk args so no single frame is oversized.
        let mut frames = Vec::new();
        for chunk in symbols.chunks(10) {
            let mut args: Vec<String> = Vec::new();
            for inst in chunk {
                let s = self.to_symbol(inst);
                args.push(format!("orderbook.{}.{}", self.depth, s));
                args.push(format!("tickers.{}", s));
            }
            frames.push(
                serde_json::json!({ "op": "subscribe", "args": args }).to_string(),
            );
        }
        frames
    }

    fn on_reconnect(&self) {
        if let Ok(mut b) = self.books.lock() {
            b.clear();
        }
    }

    fn ping_frame(&self) -> Option<String> {
        Some(r#"{"op":"ping"}"#.to_string())
    }

    fn ping_interval(&self) -> Duration {
        Duration::from_secs(20)
    }

    fn parse(&self, text: &str, ctx: &SymbolCtx) -> Vec<MarketUpdate> {
        let Ok(v): Result<Value, _> = serde_json::from_str(text) else {
            return vec![];
        };
        let Some(topic) = v.get("topic").and_then(Value::as_str) else {
            return vec![]; // ack/pong/etc.
        };
        let data = v.get("data");
        let exch_ts = v.get("ts").and_then(Value::as_i64);

        if let Some(sym) = topic.strip_prefix("orderbook.") {
            // topic form: orderbook.{depth}.{SYMBOL}
            let symbol = sym.split_once('.').map(|(_, s)| s).unwrap_or(sym);
            let Some(inst) = ctx.lookup(symbol) else {
                return vec![];
            };
            let Some(data) = data else { return vec![] };
            let bids = levels(data.get("b"));
            let asks = levels(data.get("a"));
            let is_snapshot = v.get("type").and_then(Value::as_str) == Some("snapshot");

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
            return vec![MarketUpdate::Book {
                exchange: ExchangeId::Bybit,
                instrument: inst.clone(),
                book: TopBook {
                    bids,
                    asks,
                    recv_ts: Instant::now(),
                    exch_ts,
                },
            }];
        }

        if let Some(symbol) = topic.strip_prefix("tickers.") {
            let Some(inst) = ctx.lookup(symbol) else {
                return vec![];
            };
            let Some(data) = data else { return vec![] };
            let mut out = Vec::new();
            // fundingRate is absent in deltas that don't change it — only emit when present.
            if let Some(rate) = data.get("fundingRate").and_then(Value::as_str).and_then(dec_from_str) {
                let next_ts = data
                    .get("nextFundingTime")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                out.push(MarketUpdate::Funding {
                    exchange: ExchangeId::Bybit,
                    instrument: inst.clone(),
                    rate,
                    interval_hours: Decimal::from(8), // Bybit linear default; some symbols differ
                    next_ts,
                });
            }
            // turnover24h = 24h quote volume (USDT); openInterest = base units.
            let vol = data.get("turnover24h").and_then(Value::as_str).and_then(dec_from_str);
            let oi = data.get("openInterest").and_then(Value::as_str).and_then(dec_from_str);
            if vol.is_some() || oi.is_some() {
                out.push(MarketUpdate::Ticker {
                    exchange: ExchangeId::Bybit,
                    instrument: inst.clone(),
                    quote_volume_24h: vol,
                    open_interest: oi,
                });
            }
            return out;
        }
        vec![]
    }
}

/// Convenience: the funding interval Bybit reports is per-symbol; we default to
/// 8h. Exposed for tests/documentation of the annualization assumption.
pub fn default_funding(rate: Decimal, next_ts: i64) -> FundingInfo {
    FundingInfo {
        rate,
        interval_hours: Decimal::from(8),
        next_ts,
    }
}
