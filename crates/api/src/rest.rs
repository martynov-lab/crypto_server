//! REST endpoints: health, metrics, current summary, config validation.

use crate::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use domain::{Decimal, ExchangeId};
use screener::{best_pair, chart_point, summary_row, ClientConfig};
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

/// Current best net spread per instrument, using the server default config's
/// static filters (symbol allow/deny, market pair, spread band, volume band) —
/// so denied coins and ghost spreads don't leak into the cold-start snapshot.
/// Dynamics/transfer/hysteresis are not applied. Useful without opening a WS;
/// per-client filters only apply on the WS signal stream.
pub async fn summary(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = state.cfg_store.get();
    let cfg: &ClientConfig = &cfg;
    let now = Instant::now();
    let mut rows = Vec::new();
    for instrument in state.market.instruments() {
        let snap = state.market.snapshot(&instrument, now);
        if let Some((buy, sell, net)) = summary_row(&snap, cfg) {
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
    let quote = state.cfg_store.get().quote.clone();
    Json(crate::session::build_catalog(&state.universe, &quote, 1))
}

/// The server's current (persisted) screening config — what every connection is
/// screened with until the next `subscribe` overwrites it.
pub async fn current_config(State(state): State<AppState>) -> impl IntoResponse {
    Json((*state.cfg_store.get()).clone())
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
    let cfg = state.cfg_store.get();
    let cfg = &*cfg;

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

#[derive(serde::Deserialize)]
pub struct RangeQuery {
    pub base: String,
    #[serde(default = "default_quote")]
    pub quote: String,
    /// Defaults to the client config's `history_window_ms` (up to 3 days).
    #[serde(default)]
    pub window_ms: Option<u64>,
}

/// `GET /spread/range?base=…&window_ms=…` — the long spread history: coarse
/// best-pair aggregates (min/max/close net spread per bucket, default 1 minute)
/// retained for days, so the client can see what spread a coin is even capable
/// of. Distinct from `/spread/history`, which serves the fine per-venue tape
/// for the live chart but only holds minutes.
pub async fn spread_range(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<RangeQuery>,
) -> impl IntoResponse {
    let instrument = domain::Instrument::perp(&q.base, &q.quote);
    let cfg = state.cfg_store.get();
    // Effective window: requested → client cap → server retention.
    let window = q
        .window_ms
        .unwrap_or(cfg.history_window_ms)
        .min(cfg.history_window_ms)
        .min(state.tape.history_window_ms());

    match state.tape.long_history(&instrument, window) {
        Some(buckets) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instrument": instrument,
                "resolution_ms": state.tape.history_resolution_ms(),
                "window_ms": window,
                "buckets": buckets,
            })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "no spread history for {}/{} (accumulates from server start)",
                    q.base.to_uppercase(),
                    q.quote.to_uppercase()
                )
            })),
        ),
    }
}

#[derive(serde::Deserialize)]
pub struct WhyQuery {
    pub base: String,
    #[serde(default = "default_quote")]
    pub quote: String,
}

#[derive(Serialize)]
struct WhyVenue {
    exchange: ExchangeId,
    bid: Option<Decimal>,
    ask: Option<Decimal>,
    age_ms: u64,
    stale: bool,
    valid: bool,
    enabled_for_client: bool,
    quote_volume_24h: Option<Decimal>,
    funding_rate: Option<Decimal>,
}

/// `GET /why?base=LA` — the operator's "why is there no signal for this coin?"
/// button. Runs the exact evaluation the screener runs, right now, and reports
/// which stage stopped it: not screened at all, no usable pairing, or the
/// specific filter that rejected the best pairing (with all the numbers).
pub async fn why(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<WhyQuery>,
) -> impl IntoResponse {
    let instrument = domain::Instrument::perp(&q.base, &q.quote);
    let cfg = state.cfg_store.get();
    let listed_on: Vec<ExchangeId> = state
        .universe
        .catalog()
        .into_iter()
        .find(|(base, _)| *base == instrument.base)
        .map(|(_, venues)| venues)
        .unwrap_or_default();

    // Stage 1: is the coin ingested at all? This is the answer to most "where
    // is my signal?" questions — the coin never made it into the subscription
    // set (not listed on enough venues, or outside the max_symbols cap).
    if !state.market.instruments().contains(&instrument) {
        let hint = if listed_on.len() < 2 {
            "listed on fewer than 2 venues — a cross-exchange spread cannot exist"
        } else {
            "listed but not subscribed — outside ingest.max_symbols (raise it or add the coin to ingest.always_screen)"
        };
        return Json(serde_json::json!({
            "instrument": instrument,
            "screened": false,
            "listed_on": listed_on,
            "hint": hint,
        }));
    }

    // Stage 2: per-venue quotes as the screener sees them right now.
    let now = Instant::now();
    let snap = state.market.snapshot(&instrument, now);
    let venues: Vec<WhyVenue> = snap
        .quotes
        .iter()
        .map(|v| WhyVenue {
            exchange: v.exchange,
            bid: v.book.best_bid().map(|l| l.price),
            ask: v.book.best_ask().map(|l| l.price),
            age_ms: now.saturating_duration_since(v.book.recv_ts).as_millis() as u64,
            stale: v.stale,
            valid: v.valid,
            enabled_for_client: cfg.includes(v.exchange),
            quote_volume_24h: v.quote_volume_24h,
            funding_rate: v.funding.as_ref().map(|f| f.rate),
        })
        .collect();

    // Stage 3: the actual evaluation, same code path as the alert engine
    // (minus hysteresis/cooldown, which only dedup — they never create or
    // destroy an opportunity).
    match screener::evaluate(&snap, &cfg, state.oracle.as_ref()) {
        Some(eval) => Json(serde_json::json!({
            "instrument": instrument,
            "screened": true,
            "listed_on": listed_on,
            "reason": eval.reason,
            "spread": eval.spread,
            "quality_score": eval.quality_score,
            "dynamics": eval.stats,
            "funding": eval.funding,
            "venues": venues,
        })),
        None => {
            let usable = snap.usable().filter(|v| cfg.includes(v.exchange)).count();
            let hint = if !cfg.allows_symbol(&instrument.base) {
                "symbol is excluded by the client's allow/deny lists"
            } else if usable < 2 {
                "fewer than 2 usable venues right now (stale/invalid books or venues disabled in the client config)"
            } else {
                "no evaluable pairing (books empty at the target size)"
            };
            Json(serde_json::json!({
                "instrument": instrument,
                "screened": true,
                "listed_on": listed_on,
                "reason": null,
                "hint": hint,
                "venues": venues,
            }))
        }
    }
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
