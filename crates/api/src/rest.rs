//! REST endpoints: health, metrics, current summary, config validation.

use crate::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use domain::{Decimal, ExchangeId};
use screener::{best_raw_net, ClientConfig};
use serde::Serialize;
use std::time::Instant;

pub async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    let instruments = state.market.instruments().len();
    Json(serde_json::json!({
        "status": "ok",
        "instruments": instruments,
    }))
}

pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let body = (state.metrics_render)();
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
}

#[derive(Serialize)]
struct SummaryRow {
    instrument: String,
    buy_exchange: ExchangeId,
    sell_exchange: ExchangeId,
    net_pct: Decimal,
    coverage: usize,
}

/// Current best net spread per instrument, using the server default config.
/// Useful for a dashboard snapshot without opening a WS.
pub async fn summary(State(state): State<AppState>) -> impl IntoResponse {
    let cfg: &ClientConfig = &state.default_cfg;
    let now = Instant::now();
    let mut rows = Vec::new();
    for instrument in state.market.instruments() {
        let snap = state.market.snapshot(&instrument, now);
        if let Some((buy, sell, net)) = best_raw_net(&snap, cfg) {
            rows.push(SummaryRow {
                instrument: format!("{}/{}", instrument.base, instrument.quote),
                buy_exchange: buy,
                sell_exchange: sell,
                net_pct: net,
                coverage: snap.usable().count(),
            });
        }
    }
    // Highest spread first.
    rows.sort_by(|a, b| b.net_pct.cmp(&a.net_pct));
    Json(rows)
}

/// Traded-instrument catalog: which base assets have a USDT perp on which venues.
pub async fn instruments(State(state): State<AppState>) -> impl IntoResponse {
    let quote = &state.default_cfg.quote;
    Json(crate::session::build_catalog(&state.universe, quote, 1))
}

/// Validate a client-supplied config without subscribing.
pub async fn validate_config(Json(cfg): Json<ClientConfig>) -> impl IntoResponse {
    match cfg.validate() {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "valid": true }))),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "valid": false, "error": e })),
        ),
    }
}
