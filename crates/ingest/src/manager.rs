//! Connector supervision and update routing.

use domain::{ExchangeConnector, Instrument, MarketUpdate};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Tunables for the ingest funnel.
#[derive(Debug, Clone)]
pub struct IngestParams {
    /// Bound on the shared update channel. Connectors block (backpressure) when
    /// `market_state` falls behind — bounded so a burst can't blow memory.
    pub channel_buffer: usize,
    /// Delay before restarting a connector whose `run` returned unexpectedly.
    pub restart_delay: Duration,
}

impl Default for IngestParams {
    fn default() -> Self {
        IngestParams {
            channel_buffer: 8192,
            restart_delay: Duration::from_secs(2),
        }
    }
}

pub struct IngestManager {
    connectors: Vec<Arc<dyn ExchangeConnector>>,
    symbols: Vec<Instrument>,
    params: IngestParams,
}

impl IngestManager {
    pub fn new(
        connectors: Vec<Arc<dyn ExchangeConnector>>,
        symbols: Vec<Instrument>,
        params: IngestParams,
    ) -> Self {
        IngestManager {
            connectors,
            symbols,
            params,
        }
    }

    /// Spawn every connector under a supervisor and return the receiving end of
    /// the shared update channel plus the supervisor join handles. Dropping the
    /// returned receiver signals every connector to stop (their `tx` closes).
    pub fn spawn(self) -> (mpsc::Receiver<MarketUpdate>, Vec<JoinHandle<()>>) {
        let (tx, rx) = mpsc::channel::<MarketUpdate>(self.params.channel_buffer);
        let mut handles = Vec::with_capacity(self.connectors.len());

        for connector in self.connectors {
            let symbols = self.symbols.clone();
            let tx = tx.clone();
            let restart_delay = self.params.restart_delay;
            let id = connector.id();

            handles.push(tokio::spawn(async move {
                info!(exchange = %id, "starting connector supervisor");
                loop {
                    if tx.is_closed() {
                        return;
                    }
                    let conn = connector.clone();
                    match conn.run(symbols.clone(), tx.clone()).await {
                        Ok(()) => {
                            // Clean stop only happens when tx is closed (shutdown).
                            info!(exchange = %id, "connector stopped");
                            return;
                        }
                        Err(e) => {
                            error!(exchange = %id, error = %e, "connector run failed; restarting");
                        }
                    }
                    if tx.is_closed() {
                        return;
                    }
                    warn!(exchange = %id, ?restart_delay, "restarting connector");
                    tokio::time::sleep(restart_delay).await;
                }
            }));
        }

        // Drop our own sender so the channel closes once all connectors exit.
        drop(tx);
        (rx, handles)
    }
}
