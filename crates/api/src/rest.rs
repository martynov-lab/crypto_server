//! REST endpoints: health, metrics, current summary, config validation.

use crate::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use domain::{Decimal, ExchangeId};
use screener::{best_pair, best_raw_net, chart_point, ClientConfig};
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

#[derive(serde::Deserialize)]
pub struct HistoryQuery {
    pub base: String,
    #[serde(default = "default_quote")]
    pub quote: String,
    #[serde(default = "default_window")]
    pub window_ms: u64,
    #[serde(default)]
    pub long_exchange: Option<ExchangeId>,
    #[serde(default)]
    pub short_exchange: Option<ExchangeId>,
}

fn default_quote() -> String {
    "USDT".to_string()
}
fn default_window() -> u64 {
    900_000
}

/// REST fallback for the spread chart: the buffered In/Out points for one
/// instrument's fixed long/short pair (pinned, or best pair at request time).
pub async fn spread_history(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<HistoryQuery>,
) -> impl IntoResponse {
    let instrument = domain::Instrument::perp(&q.base, &q.quote);
    let window = q.window_ms.min(state.chart.max_window_ms);
    let cfg = &state.default_cfg;

    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("no live spread for {}/{}", q.base.to_uppercase(), q.quote.to_uppercase())
            })),
        )
    };

    let Some(samples) = state.tape.history(&instrument, window) else {
        return not_found();
    };
    let pair = match (q.long_exchange, q.short_exchange) {
        (Some(l), Some(s)) if l != s => Some((l, s)),
        _ => samples.last().and_then(|s| best_pair(s, cfg)),
    };
    let Some((long, short)) = pair else {
        return not_found();
    };
    let points: Vec<_> = samples
        .iter()
        .filter_map(|s| chart_point(s, long, short, cfg))
        .collect();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "instrument": instrument,
            "resolution_ms": state.tape.resolution_ms(),
            "window_ms": window,
            "long_exchange": long,
            "short_exchange": short,
            "points": points,
        })),
    )
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
