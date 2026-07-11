//! WS protocol message types exchanged with clients.

use domain::ExchangeId;
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
    /// Client keepalive.
    Ping,
}

/// Outbound server → client messages.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Acknowledges a successful subscribe, echoing the effective config.
    Subscribed { config: Box<ClientConfig> },
    /// The traded-instrument catalog (which coins trade on which venues), sent
    /// once right after `subscribed`.
    Universe { instruments: Vec<CatalogRow> },
    /// A screening signal.
    Event(ScreenerEvent),
    /// Server keepalive response.
    Pong,
    /// A protocol/auth error; the connection may be closed after.
    Error { message: String },
}
