//! Background discovery poller: refreshes the universe catalog periodically.

use crate::fetchers::fetch;
use crate::UniverseStore;
use domain::ExchangeId;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub exchanges: Vec<ExchangeId>,
    pub interval: Duration,
}

/// Refresh every exchange's listed bases once, into `store`.
pub async fn refresh_once(client: &Client, store: &UniverseStore, exchanges: &[ExchangeId]) {
    for &exchange in exchanges {
        match fetch(client, exchange).await {
            Ok(bases) => {
                let n = bases.len();
                store.set_exchange(exchange, &bases);
                info!(exchange = %exchange, listed = n, "universe refreshed");
            }
            Err(e) => warn!(exchange = %exchange, error = %e, "universe fetch failed"),
        }
    }
    info!(bases = store.len(), "universe catalog updated");
}

/// Build a shared HTTP client for discovery.
pub fn build_client() -> anyhow::Result<Client> {
    Ok(Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("arb-screener/0.1")
        .build()?)
}

/// Poll on an interval until `shutdown` flips true.
pub async fn run_poller(
    store: Arc<UniverseStore>,
    cfg: DiscoveryConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "discovery client build failed; poller disabled");
            return;
        }
    };
    let mut timer = tokio::time::interval(cfg.interval);
    loop {
        tokio::select! {
            _ = timer.tick() => refresh_once(&client, &store, &cfg.exchanges).await,
            _ = shutdown.changed() => {
                if *shutdown.borrow() { info!("discovery poller shutting down"); return; }
            }
        }
    }
}
