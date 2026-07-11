//! WS session: subscribe → stream signals. Per-session screening engine reads
//! the shared market state; a slow client only slows its own send loop (the
//! broadcast fan-out tolerates lag), so it never stalls the core.

use crate::session::{ClientMessage, ServerMessage};
use crate::AppState;
use auth::AuthOutcome;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use screener::ScreenerEngine;
use std::time::Instant;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn send(tx: &mut SplitSink<WebSocket, Message>, msg: &ServerMessage) -> bool {
    match serde_json::to_string(msg) {
        Ok(txt) => tx.send(Message::Text(txt)).await.is_ok(),
        Err(e) => {
            warn!(error = %e, "failed to serialize server message");
            true // don't kill the session over one bad serialize
        }
    }
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // --- Handshake: wait for the first Subscribe. ---
    let (cfg, _token) = loop {
        match ws_rx.next().await {
            Some(Ok(Message::Text(txt))) => match serde_json::from_str::<ClientMessage>(&txt) {
                Ok(ClientMessage::Subscribe { token, config }) => {
                    if state.auth.check(token.as_deref()) == AuthOutcome::Rejected {
                        let _ = send(
                            &mut ws_tx,
                            &ServerMessage::Error {
                                message: "unauthorized".into(),
                            },
                        )
                        .await;
                        return;
                    }
                    let cfg = config.unwrap_or_else(|| (*state.default_cfg).clone());
                    if let Err(e) = cfg.validate() {
                        let _ = send(&mut ws_tx, &ServerMessage::Error { message: e }).await;
                        return;
                    }
                    break (cfg, token);
                }
                Ok(ClientMessage::Ping) => {
                    if !send(&mut ws_tx, &ServerMessage::Pong).await {
                        return;
                    }
                }
                Err(e) => {
                    let _ = send(
                        &mut ws_tx,
                        &ServerMessage::Error {
                            message: format!("bad message: {e}"),
                        },
                    )
                    .await;
                }
            },
            Some(Ok(Message::Close(_))) | None => return,
            Some(Ok(_)) => {} // ignore pings/binary during handshake
            Some(Err(_)) => return,
        }
    };

    info!(exchanges = ?cfg.exchanges, "client subscribed");
    let mut engine = ScreenerEngine::new(cfg.clone());
    if !send(
        &mut ws_tx,
        &ServerMessage::Subscribed {
            config: Box::new(cfg),
        },
    )
    .await
    {
        return;
    }

    // --- Stream loop. ---
    let mut events = state.events.subscribe();
    loop {
        tokio::select! {
            // Inbound client messages.
            inbound = ws_rx.next() => {
                match inbound {
                    Some(Ok(Message::Text(txt))) => {
                        match serde_json::from_str::<ClientMessage>(&txt) {
                            Ok(ClientMessage::Ping) => {
                                if !send(&mut ws_tx, &ServerMessage::Pong).await { return; }
                            }
                            Ok(ClientMessage::Subscribe { token, config }) => {
                                if state.auth.check(token.as_deref()) == AuthOutcome::Rejected {
                                    let _ = send(&mut ws_tx, &ServerMessage::Error { message: "unauthorized".into() }).await;
                                    return;
                                }
                                let newcfg = config.unwrap_or_else(|| (*state.default_cfg).clone());
                                if let Err(e) = newcfg.validate() {
                                    let _ = send(&mut ws_tx, &ServerMessage::Error { message: e }).await;
                                    continue;
                                }
                                debug!("client reconfigured");
                                engine = ScreenerEngine::new(newcfg.clone());
                                if !send(&mut ws_tx, &ServerMessage::Subscribed { config: Box::new(newcfg) }).await {
                                    return;
                                }
                            }
                            Err(e) => {
                                let _ = send(&mut ws_tx, &ServerMessage::Error { message: format!("bad message: {e}") }).await;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if ws_tx.send(Message::Pong(p)).await.is_err() { return; }
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => return,
                }
            }

            // Market-state change notifications.
            recv = events.recv() => {
                match recv {
                    Ok(instrument) => {
                        if let Some(ev) = evaluate(&state, &engine, &instrument) {
                            if !send(&mut ws_tx, &ServerMessage::Event(ev)).await { return; }
                        }
                    }
                    // Lagged: we fell behind and missed some notifications. Catch
                    // up by re-scanning every instrument once (coalesced).
                    Err(RecvError::Lagged(n)) => {
                        debug!(missed = n, "session lagged; rescanning");
                        for instrument in state.market.instruments() {
                            if let Some(ev) = evaluate(&state, &engine, &instrument) {
                                if !send(&mut ws_tx, &ServerMessage::Event(ev)).await { return; }
                            }
                        }
                    }
                    Err(RecvError::Closed) => return,
                }
            }
        }
    }
}

/// Snapshot the instrument and run the session's engine over it.
fn evaluate(
    state: &AppState,
    engine: &ScreenerEngine,
    instrument: &domain::Instrument,
) -> Option<screener::ScreenerEvent> {
    let now = Instant::now();
    let snap = state.market.snapshot(instrument, now);
    engine.on_instrument(&snap, state.oracle.as_ref(), now, now_ms())
}
