//! WS protocol message types exchanged with clients.

use screener::{ClientConfig, ScreenerEvent};
use serde::{Deserialize, Serialize};

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
    /// A screening signal.
    Event(ScreenerEvent),
    /// Server keepalive response.
    Pong,
    /// A protocol/auth error; the connection may be closed after.
    Error { message: String },
}
