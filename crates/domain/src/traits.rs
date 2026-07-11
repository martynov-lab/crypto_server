//! The connector trait every exchange implementation satisfies.

use crate::instrument::Instrument;
use crate::types::{ExchangeId, MarketUpdate};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;

/// An exchange connector: connects, subscribes to the given instruments, and
/// streams normalized [`MarketUpdate`]s into `tx`.
///
/// Implementations own their reconnect strategy: `connect → subscribe → read
/// loop → normalize → tx.send`, with exponential backoff + jitter + resubscribe,
/// and (for L2 books) a snapshot resync on reconnect. `run` only returns on an
/// unrecoverable error or when `tx` is closed (shutdown).
#[async_trait]
pub trait ExchangeConnector: Send + Sync {
    fn id(&self) -> ExchangeId;

    async fn run(
        self: Arc<Self>,
        symbols: Vec<Instrument>,
        tx: mpsc::Sender<MarketUpdate>,
    ) -> anyhow::Result<()>;
}
