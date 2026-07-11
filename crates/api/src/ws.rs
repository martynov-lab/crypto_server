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
use domain::{Decimal, Instrument};
use futures_util::{SinkExt, StreamExt};
use screener::ScreenerEngine;
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
                    let cfg = config.unwrap_or_else(|| (*state.default_cfg).clone());
                    if let Err(e) = cfg.validate() {
                        let _ = emit(&out_tx, Outbound::Server(ServerMessage::Error { message: e })).await;
                        writer.abort();
                        return;
                    }
                    break cfg;
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
                                let newcfg = config.unwrap_or_else(|| (*state.default_cfg).clone());
                                if let Err(e) = newcfg.validate() {
                                    let _ = emit(&out_tx, Outbound::Server(ServerMessage::Error { message: e })).await;
                                    continue;
                                }
                                debug!("client reconfigured (watches preserved)");
                                engine = ScreenerEngine::new(newcfg.clone());
                                let _ = emit(&out_tx, Outbound::Server(ServerMessage::Subscribed { config: Box::new(newcfg) })).await;
                            }
                            Ok(ClientMessage::Watch { instrument, window_ms, resolution_ms: _ }) => {
                                let chart_cap = engine.config().max_chart_spread_pct;
                                handle_watch(&state, &out_tx, &mut watches, instrument, window_ms, chart_cap).await;
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
/// validate coverage, send the backfill, and spawn a live forwarder.
async fn handle_watch(
    state: &AppState,
    out_tx: &mpsc::Sender<Outbound>,
    watches: &mut HashMap<Instrument, JoinHandle<()>>,
    instrument: Instrument,
    window_ms: Option<u64>,
    chart_cap: Decimal,
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

    let window = window_ms
        .unwrap_or(900_000)
        .min(state.chart.max_window_ms);

    let Some(mut w) = state.tape.watch(&instrument, window) else {
        let _ = emit(
            out_tx,
            Outbound::Server(ServerMessage::Error {
                message: format!("no live spread for {}/{}", instrument.base, instrument.quote),
            }),
        )
        .await;
        return;
    };

    // Client-side anomaly filter on the backfill.
    w.backfill.retain(|p| p.net_pct.abs() <= chart_cap);

    // Backfill first, so the chart fills instantly.
    let _ = emit(
        out_tx,
        Outbound::Server(ServerMessage::WatchSnapshot {
            instrument: instrument.clone(),
            resolution_ms: w.resolution_ms,
            window_ms: window,
            points: w.backfill,
        }),
    )
    .await;

    // Then stream live ticks until unwatch/disconnect.
    let mut live = w.live;
    let tx = out_tx.clone();
    let inst = instrument.clone();
    let handle = tokio::spawn(async move {
        loop {
            match live.recv().await {
                Ok(point) => {
                    // Client-side anomaly filter on live ticks.
                    if point.net_pct.abs() > chart_cap {
                        continue;
                    }
                    let msg = ServerMessage::SpreadTick {
                        instrument: inst.clone(),
                        point,
                    };
                    // Await-send: broadcast lag coalesces if the client is slow.
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
