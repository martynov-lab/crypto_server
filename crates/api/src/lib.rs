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
use screener::{ClientConfig, TransferOracle};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Shared, cheaply-cloneable application state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub market: Arc<MarketState>,
    pub oracle: Arc<dyn TransferOracle>,
    /// Fan-out of "this instrument's market state changed" notifications. WS
    /// sessions subscribe; lag is tolerated (natural coalescing).
    pub events: broadcast::Sender<Instrument>,
    pub default_cfg: Arc<ClientConfig>,
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
        .route("/config/validate", post(rest::validate_config))
        .route("/ws", get(ws::ws_handler))
        .with_state(state)
}
