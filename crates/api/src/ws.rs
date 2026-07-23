//! WS session: `subscribe` → signal stream, plus `watch` → real-time spread
//! chart stream (backfill + live ticks), independent of the alert filters.
//!
//! All outbound writes funnel through a single writer task fed by an mpsc
//! channel, so the alert path, per-instrument watch forwarders, and keepalives
//! can safely share one socket. Slow clients drop intermediate events/ticks
//! (latest-wins) rather than stalling the core.

use crate::session::{ClientMessage, ServerMessage};
use crate::AppState;
use auth::AuthOutcome;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use domain::{Decimal, ExchangeId, Instrument};
use futures_util::{SinkExt, StreamExt};
use screener::{ClientConfig, ScreenerEngine};
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info};

/// Everything the single writer task can emit.
enum Outbound {
    Server(ServerMessage),
    /// Reply to a native WS ping frame.
    Pong(Vec<u8>),
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Resolve the config for a `subscribe`.
///
/// A client-supplied config **overwrites the persisted server config** — Phase 1
/// has one client, and its settings are the server's settings from that moment
/// on (sampler, terminal logger, REST all follow). Subscribing without a config
/// adopts whatever is currently stored.
fn adopt_config(
    state: &AppState,
    config: Option<ClientConfig>,
) -> Result<ClientConfig, String> {
    match config {
        Some(cfg) => state.cfg_store.set(cfg).map(|arc| (*arc).clone()),
        None => Ok((*state.cfg_store.get()).clone()),
    }
}

/// Await-send (used for handshake-critical messages). Returns false if the
/// writer is gone.
async fn emit(tx: &mpsc::Sender<Outbound>, out: Outbound) -> bool {
    tx.send(out).await.is_ok()
}

/// Non-blocking send (used for high-rate events/ticks). Drops on a full channel
/// (coalescing); returns false only when the writer is gone.
fn try_emit(tx: &mpsc::Sender<Outbound>, out: Outbound) -> bool {
    !matches!(tx.try_send(out), Err(mpsc::error::TrySendError::Closed(_)))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(256);

    // Single writer task owns the sink.
    let writer: JoinHandle<()> = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            let msg = match out {
                Outbound::Server(m) => match serde_json::to_string(&m) {
                    Ok(txt) => Message::Text(txt),
                    Err(_) => continue,
                },
                Outbound::Pong(p) => Message::Pong(p),
            };
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // First message on every connection: the server's current (persisted)
    // config, so the client knows what the server screens with before deciding
    // whether to subscribe with its own settings or adopt these.
    if !emit(
        &out_tx,
        Outbound::Server(ServerMessage::Config {
            config: Box::new((*state.cfg_store.get()).clone()),
        }),
    )
    .await
    {
        writer.abort();
        return;
    }

    // --- Handshake: wait for the first Subscribe. ---
    let cfg = loop {
        match ws_rx.next().await {
            Some(Ok(Message::Text(txt))) => match serde_json::from_str::<ClientMessage>(&txt) {
                Ok(ClientMessage::Subscribe { token, config }) => {
                    if state.auth.check(token.as_deref()) == AuthOutcome::Rejected {
                        let _ = emit(&out_tx, Outbound::Server(ServerMessage::Error {
                            message: "unauthorized".into(),
                        }))
                        .await;
                        writer.abort();
                        return;
                    }
                    match adopt_config(&state, config) {
                        Ok(cfg) => break cfg,
                        Err(e) => {
                            let _ = emit(&out_tx, Outbound::Server(ServerMessage::Error { message: e })).await;
                            writer.abort();
                            return;
                        }
                    }
                }
                Ok(ClientMessage::Ping) => {
                    if !emit(&out_tx, Outbound::Server(ServerMessage::Pong)).await {
                        writer.abort();
                        return;
                    }
                }
                _ => {} // ignore watch/unwatch/bad before subscribe
            },
            Some(Ok(Message::Close(_))) | None => {
                writer.abort();
                return;
            }
            Some(Ok(_)) => {}
            Some(Err(_)) => {
                writer.abort();
                return;
            }
        }
    };

    info!(exchanges = ?cfg.exchanges, "client subscribed");
    let quote = cfg.quote.clone();
    let mut engine = ScreenerEngine::new(cfg.clone());
    let _ = emit(&out_tx, Outbound::Server(ServerMessage::Subscribed { config: Box::new(cfg) })).await;
    let catalog = crate::session::build_catalog(&state.universe, &quote, 2);
    let _ = emit(&out_tx, Outbound::Server(ServerMessage::Universe { instruments: catalog })).await;

    // --- Stream loop. ---
    let mut events = state.events.subscribe();
    let mut watches: HashMap<Instrument, JoinHandle<()>> = HashMap::new();

    'main: loop {
        tokio::select! {
            inbound = ws_rx.next() => {
                match inbound {
                    Some(Ok(Message::Text(txt))) => {
                        match serde_json::from_str::<ClientMessage>(&txt) {
                            Ok(ClientMessage::Ping) => {
                                if !emit(&out_tx, Outbound::Server(ServerMessage::Pong)).await { break 'main; }
                            }
                            Ok(ClientMessage::Subscribe { token, config }) => {
                                if state.auth.check(token.as_deref()) == AuthOutcome::Rejected {
                                    let _ = emit(&out_tx, Outbound::Server(ServerMessage::Error { message: "unauthorized".into() })).await;
                                    break 'main;
                                }
                                let newcfg = match adopt_config(&state, config) {
                                    Ok(cfg) => cfg,
                                    Err(e) => {
                                        let _ = emit(&out_tx, Outbound::Server(ServerMessage::Error { message: e })).await;
                                        continue;
                                    }
                                };
                                debug!("client reconfigured (watches preserved)");
                                engine = ScreenerEngine::new(newcfg.clone());
                                let _ = emit(&out_tx, Outbound::Server(ServerMessage::Subscribed { config: Box::new(newcfg) })).await;
                            }
                            Ok(ClientMessage::Watch { instrument, window_ms, resolution_ms: _, long_exchange, short_exchange }) => {
                                let cfg = engine.config().clone();
                                handle_watch(&state, &out_tx, &mut watches, instrument, window_ms, (long_exchange, short_exchange), cfg).await;
                            }
                            Ok(ClientMessage::Unwatch { instrument }) => {
                                if let Some(h) = watches.remove(&instrument) { h.abort(); }
                            }
                            Err(e) => {
                                let _ = emit(&out_tx, Outbound::Server(ServerMessage::Error { message: format!("bad message: {e}") })).await;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if !emit(&out_tx, Outbound::Pong(p)).await { break 'main; }
                    }
                    Some(Ok(Message::Close(_))) | None => break 'main,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break 'main,
                }
            }

            recv = events.recv() => {
                match recv {
                    Ok(instrument) => {
                        if let Some(ev) = evaluate(&state, &engine, &instrument) {
                            if !try_emit(&out_tx, Outbound::Server(ServerMessage::Event(ev))) { break 'main; }
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        debug!(missed = n, "session lagged; rescanning");
                        for instrument in state.market.instruments() {
                            if let Some(ev) = evaluate(&state, &engine, &instrument) {
                                if !try_emit(&out_tx, Outbound::Server(ServerMessage::Event(ev))) { break 'main; }
                            }
                        }
                    }
                    Err(RecvError::Closed) => break 'main,
                }
            }
        }
    }

    // Cleanup: stop all watch forwarders and the writer.
    for (_, h) in watches {
        h.abort();
    }
    writer.abort();
}

/// Start (or restart) a watch on `instrument`: enforce the per-session cap,
/// resolve the fixed long/short pair, send the In/Out backfill + funding header,
/// and spawn a live forwarder that streams In/Out ticks for that pair.
async fn handle_watch(
    state: &AppState,
    out_tx: &mpsc::Sender<Outbound>,
    watches: &mut HashMap<Instrument, JoinHandle<()>>,
    instrument: Instrument,
    window_ms: Option<u64>,
    pinned: (Option<ExchangeId>, Option<ExchangeId>),
    cfg: ClientConfig,
) {
    // Re-watch replaces an existing stream for the same instrument.
    if let Some(h) = watches.remove(&instrument) {
        h.abort();
    }
    if watches.len() >= state.chart.max_watches {
        let _ = emit(
            out_tx,
            Outbound::Server(ServerMessage::Error {
                message: format!("watch limit reached ({} max)", state.chart.max_watches),
            }),
        )
        .await;
        return;
    }

    let window = window_ms.unwrap_or(900_000).min(state.chart.max_window_ms);

    let no_spread = |ex: &Instrument| ServerMessage::Error {
        message: format!("no live spread for {}/{}", ex.base, ex.quote),
    };

    let Some(w) = state.tape.watch(&instrument, window) else {
        let _ = emit(out_tx, Outbound::Server(no_spread(&instrument))).await;
        return;
    };

    // Resolve the fixed pair: pinned by the client, else the best pair at open.
    let (long, short) = match pinned {
        (Some(l), Some(s)) if l != s => (l, s),
        _ => match w.backfill.last().and_then(|sample| screener::best_pair(sample, &cfg)) {
            Some(p) => p,
            None => {
                let _ = emit(out_tx, Outbound::Server(no_spread(&instrument))).await;
                return;
            }
        },
    };

    // Effective anomaly cap = global hard cap tightened by the client's cap.
    let eff_cap = state
        .chart
        .sanity_max_spread_pct
        .min(cfg.max_chart_spread_pct);

    // Transform buffered venue samples into In/Out points for the fixed pair.
    let points: Vec<_> = w
        .backfill
        .iter()
        .filter_map(|s| screener::chart_point(s, long, short, &cfg))
        .filter(|p| p.in_pct.abs() <= eff_cap)
        .collect();

    // Funding header from the latest sample's two legs.
    let (interval, next_ms, long_apr, short_apr) = funding_header(&w.backfill, long, short);

    let _ = emit(
        out_tx,
        Outbound::Server(ServerMessage::WatchSnapshot {
            instrument: instrument.clone(),
            resolution_ms: w.resolution_ms,
            window_ms: window,
            long_exchange: long,
            short_exchange: short,
            funding_interval_hours: interval,
            next_funding_ms: next_ms,
            funding_long_apr: long_apr,
            funding_short_apr: short_apr,
            points,
        }),
    )
    .await;

    // Stream live In/Out ticks for the fixed pair until unwatch/disconnect.
    let mut live = w.live;
    let tx = out_tx.clone();
    let inst = instrument.clone();
    let handle = tokio::spawn(async move {
        loop {
            match live.recv().await {
                Ok(sample) => {
                    let Some(point) = screener::chart_point(&sample, long, short, &cfg) else {
                        continue; // pair not present this tick (gap)
                    };
                    if point.in_pct.abs() > eff_cap {
                        continue;
                    }
                    let msg = ServerMessage::SpreadTick {
                        instrument: inst.clone(),
                        point,
                    };
                    if tx.send(Outbound::Server(msg)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
    watches.insert(instrument, handle);
}

/// Annualize a per-interval funding rate.
fn apr(rate: Option<Decimal>, interval: Option<Decimal>) -> Option<Decimal> {
    match (rate, interval) {
        (Some(r), Some(i)) if i > Decimal::ZERO => Some(r * (Decimal::from(24 * 365) / i)),
        (Some(r), _) => Some(r),
        _ => None,
    }
}

/// Build the funding header (interval, soonest next-funding ms, per-leg APR) from
/// the latest sample's long/short legs.
fn funding_header(
    backfill: &[domain::VenueSample],
    long: ExchangeId,
    short: ExchangeId,
) -> (Option<Decimal>, Option<i64>, Option<Decimal>, Option<Decimal>) {
    let Some(sample) = backfill.last() else {
        return (None, None, None, None);
    };
    let l = sample.venues.iter().find(|v| v.exchange == long);
    let s = sample.venues.iter().find(|v| v.exchange == short);

    let interval = l
        .and_then(|v| v.funding_interval_hours)
        .or_else(|| s.and_then(|v| v.funding_interval_hours));
    let next_ms = [
        l.and_then(|v| v.next_funding_ms),
        s.and_then(|v| v.next_funding_ms),
    ]
    .into_iter()
    .flatten()
    .filter(|&t| t > 0)
    .min();
    let long_apr = apr(
        l.and_then(|v| v.funding_rate),
        l.and_then(|v| v.funding_interval_hours),
    );
    let short_apr = apr(
        s.and_then(|v| v.funding_rate),
        s.and_then(|v| v.funding_interval_hours),
    );
    (interval, next_ms, long_apr, short_apr)
}

/// Snapshot the instrument and run the session's engine over it.
fn evaluate(
    state: &AppState,
    engine: &ScreenerEngine,
    instrument: &Instrument,
) -> Option<screener::ScreenerEvent> {
    let now = Instant::now();
    let snap = state.market.snapshot(instrument, now);
    engine.on_instrument(&snap, state.oracle.as_ref(), now, now_ms())
}
