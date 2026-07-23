//! WS protocol message types exchanged with clients.

use domain::{ChartPoint, Decimal, ExchangeId, Instrument};
use screener::{ClientConfig, ScreenerEvent};
use serde::{Deserialize, Serialize};
use universe::UniverseStore;

/// One row of the traded-instrument catalog: a base asset and the venues that
/// list its USDT perp.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogRow {
    pub base: String,
    pub quote: String,
    pub exchanges: Vec<ExchangeId>,
    pub coverage: usize,
}

/// Build the catalog from the universe store (most-covered first). Rows with
/// fewer than `min_coverage` venues are dropped (a single-venue coin can never
/// produce a cross-exchange signal).
pub fn build_catalog(universe: &UniverseStore, quote: &str, min_coverage: usize) -> Vec<CatalogRow> {
    universe
        .catalog()
        .into_iter()
        .filter(|(_, exchanges)| exchanges.len() >= min_coverage)
        .map(|(base, exchanges)| CatalogRow {
            base,
            quote: quote.to_string(),
            coverage: exchanges.len(),
            exchanges,
        })
        .collect()
}

/// Inbound client → server messages.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Subscribe (or re-subscribe) with a screening config. Missing fields fall
    /// back to server defaults via `ClientConfig`'s `#[serde(default)]`.
    Subscribe {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        config: Option<ClientConfig>,
    },
    /// Start streaming an instrument's raw spread for the live chart. Independent
    /// of `subscribe`; the server backfills then pushes ticks. Optionally pin the
    /// long/short pair (from the tapped signal card); if omitted the server fixes
    /// the best pair at open time and holds it, so the line doesn't jump.
    Watch {
        instrument: Instrument,
        #[serde(default)]
        window_ms: Option<u64>,
        #[serde(default)]
        resolution_ms: Option<u64>,
        /// Long leg (where you buy to open) = signal's `buy_exchange`.
        #[serde(default)]
        long_exchange: Option<ExchangeId>,
        /// Short leg (where you sell to open) = signal's `sell_exchange`.
        #[serde(default)]
        short_exchange: Option<ExchangeId>,
    },
    /// Stop streaming an instrument's spread.
    Unwatch { instrument: Instrument },
    /// Client keepalive.
    Ping,
}

/// Outbound server → client messages.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The server's current (persisted) screening config, pushed as the very
    /// first message on every connection — before any `subscribe` — so the
    /// client always knows what the server is screening with. Sent again is
    /// never needed: `subscribed` echoes the config after every change.
    Config { config: Box<ClientConfig> },
    /// Acknowledges a successful subscribe, echoing the effective config.
    Subscribed { config: Box<ClientConfig> },
    /// The traded-instrument catalog (which coins trade on which venues), sent
    /// once right after `subscribed`.
    Universe { instruments: Vec<CatalogRow> },
    /// A screening signal.
    Event(ScreenerEvent),
    /// One-shot backfill of the rolling window right after `watch`, for the fixed
    /// long/short pair. Header carries the pair and funding meta for labels/timer.
    WatchSnapshot {
        instrument: Instrument,
        resolution_ms: u64,
        window_ms: u64,
        /// Long leg (buy to open).
        long_exchange: ExchangeId,
        /// Short leg (sell to open).
        short_exchange: ExchangeId,
        #[serde(skip_serializing_if = "Option::is_none")]
        funding_interval_hours: Option<Decimal>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_funding_ms: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        funding_long_apr: Option<Decimal>,
        #[serde(skip_serializing_if = "Option::is_none")]
        funding_short_apr: Option<Decimal>,
        points: Vec<ChartPoint>,
    },
    /// A live chart point for a watched instrument's fixed pair.
    SpreadTick {
        instrument: Instrument,
        point: ChartPoint,
    },
    /// Server keepalive response.
    Pong,
    /// A protocol/auth error; the connection may be closed after.
    Error { message: String },
}
