//! Mock connector: emits scripted/synthetic updates with no network. Used by
//! offline integration tests and for local runs without live exchanges.

use async_trait::async_trait;
use domain::{ExchangeConnector, ExchangeId, Instrument, MarketUpdate};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Produces a batch of updates on each tick. Because `TopBook::recv_ts` must be
/// a fresh `Instant`, the generator is a closure invoked at send time rather
/// than a pre-built list.
pub type Generator = Arc<dyn Fn() -> Vec<MarketUpdate> + Send + Sync>;

pub struct MockConnector {
    id: ExchangeId,
    generator: Generator,
    interval: Duration,
    /// Stop after this many ticks; `None` runs until `tx` closes.
    max_ticks: Option<usize>,
}

impl MockConnector {
    pub fn new(id: ExchangeId, interval: Duration, generator: Generator) -> Self {
        MockConnector {
            id,
            generator,
            interval,
            max_ticks: None,
        }
    }

    pub fn with_max_ticks(mut self, n: usize) -> Self {
        self.max_ticks = Some(n);
        self
    }
}

#[async_trait]
impl ExchangeConnector for MockConnector {
    fn id(&self) -> ExchangeId {
        self.id
    }

    async fn run(
        self: Arc<Self>,
        _symbols: Vec<Instrument>,
        tx: mpsc::Sender<MarketUpdate>,
    ) -> anyhow::Result<()> {
        let mut ticks = 0usize;
        let mut timer = tokio::time::interval(self.interval);
        loop {
            tokio::select! {
                _ = tx.closed() => return Ok(()),
                _ = timer.tick() => {
                    for upd in (self.generator)() {
                        if tx.send(upd).await.is_err() {
                            return Ok(());
                        }
                    }
                    ticks += 1;
                    if let Some(max) = self.max_ticks {
                        if ticks >= max {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}
