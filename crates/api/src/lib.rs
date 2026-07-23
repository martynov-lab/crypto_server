//! axum-based delivery: WS hub for client signal push + REST for health,
//! metrics, summary, and config validation.

pub mod rest;
pub mod session;
pub mod ws;

use auth::AuthPolicy;
use axum::routing::{get, post};
use axum::Router;
use domain::Instrument;
use market_state::MarketState;
use persistence::ConfigStore;
use screener::TransferOracle;
use spread_tape::SpreadTape;
use std::sync::Arc;
use tokio::sync::broadcast;
use universe::UniverseStore;

/// Chart/watch limits for a session.
#[derive(Clone, Copy)]
pub struct ChartParams {
    /// Largest backfill window a client may request.
    pub max_window_ms: u64,
    /// Max concurrent watches per WS session.
    pub max_watches: usize,
    /// Global hard anomaly cap for chart points (client cap only tightens it).
    pub sanity_max_spread_pct: domain::Decimal,
}

/// Shared, cheaply-cloneable application state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub market: Arc<MarketState>,
    pub oracle: Arc<dyn TransferOracle>,
    pub universe: Arc<UniverseStore>,
    pub tape: Arc<SpreadTape>,
    pub chart: ChartParams,
    /// Fan-out of "this instrument's market state changed" notifications. WS
    /// sessions subscribe; lag is tolerated (natural coalescing).
    pub events: broadcast::Sender<Instrument>,
    /// The persisted client config — the single config the whole server screens
    /// with. A client's `subscribe` overwrites it; sessions and REST read it.
    pub cfg_store: Arc<ConfigStore>,
    pub auth: Arc<AuthPolicy>,
    /// Renders the current Prometheus metrics text (injected by the binary so
    /// this crate needn't depend on a specific exporter).
    pub metrics_render: Arc<dyn Fn() -> String + Send + Sync>,
}

/// Build the full router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(rest::healthz))
        .route("/metrics", get(rest::metrics))
        .route("/summary", get(rest::summary))
        .route("/instruments", get(rest::instruments))
        .route("/spread/history", get(rest::spread_history))
        .route("/spread/range", get(rest::spread_range))
        .route("/why", get(rest::why))
        .route("/config", get(rest::current_config))
        .route("/config/validate", post(rest::validate_config))
        .route("/ws", get(ws::ws_handler))
        .with_state(state)
}
