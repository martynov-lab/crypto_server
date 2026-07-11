//! Background poller that refreshes the transfer store on an interval.

use crate::fetchers::fetch;
use crate::TransferStore;
use domain::ExchangeId;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct PollConfig {
    pub exchanges: Vec<ExchangeId>,
    /// Assets to track (upper-cased), e.g. ["USDT"].
    pub assets: Vec<String>,
    pub interval: Duration,
}

/// Poll every exchange's public currency endpoint on `cfg.interval`, upserting
/// results into `store`. Runs until `shutdown` flips to `true`.
pub async fn run_poller(
    store: Arc<TransferStore>,
    cfg: PollConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let client = match Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("arb-screener/0.1")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "failed to build http client; transfer poller disabled");
            return;
        }
    };

    let mut timer = tokio::time::interval(cfg.interval);
    loop {
        tokio::select! {
            _ = timer.tick() => {
                refresh_once(&client, &store, &cfg).await;
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("transfer poller shutting down");
                    return;
                }
            }
        }
    }
}

async fn refresh_once(client: &Client, store: &TransferStore, cfg: &PollConfig) {
    for &exchange in &cfg.exchanges {
        match fetch(client, exchange, &cfg.assets).await {
            Ok(statuses) => {
                let n = statuses.len();
                for st in statuses {
                    store.upsert(exchange, st);
                }
                debug!(exchange = %exchange, count = n, "transfer status refreshed");
            }
            Err(e) => {
                warn!(exchange = %exchange, error = %e, "transfer status fetch failed");
            }
        }
    }
    info!(entries = store.len(), "transfer store updated");
}
