//! Arbitrage screener server (Phase 1): wires connectors → ingest → market
//! state → screener → axum WS/REST, with a transfer-status poller and graceful
//! shutdown.

mod settings;

use anyhow::Context;
use api::AppState;
use auth::AuthPolicy;
use connectors::{build_connector, common::Backoff};
use domain::{ExchangeId, Instrument};
use ingest::{IngestManager, IngestParams};
use market_state::MarketState;
use metrics_exporter_prometheus::PrometheusBuilder;
use settings::Settings;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};
use transfer_status::{run_poller, PollConfig, TransferStore};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing();

    let settings = Settings::load().context("loading configuration")?;
    settings
        .default_client
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid default_client config: {e}"))?;
    info!(bind = %settings.server.bind, "starting arb-screener");

    // Metrics recorder → render closure for /metrics.
    let prom = PrometheusBuilder::new()
        .install_recorder()
        .context("installing prometheus recorder")?;
    let metrics_render = Arc::new(move || prom.render());

    // Shared state.
    let market = Arc::new(MarketState::new(Duration::from_millis(
        settings.ingest.staleness_ms,
    )));
    let store = Arc::new(TransferStore::new());
    let (events_tx, _events_rx) = broadcast::channel::<Instrument>(1024);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Canonical instruments to screen (perp only in Phase 1).
    let symbols: Vec<Instrument> = settings
        .ingest
        .symbols
        .iter()
        .map(|b| Instrument::perp(b, &settings.ingest.quote))
        .collect();
    info!(count = symbols.len(), "screening instruments");

    // Build connectors for the configured exchanges.
    let backoff = Backoff::new(
        Duration::from_millis(settings.ingest.reconnect.initial_backoff_ms),
        Duration::from_millis(settings.ingest.reconnect.max_backoff_ms),
        Duration::from_millis(settings.ingest.reconnect.jitter_ms),
    );
    let mut connectors = Vec::new();
    for raw in &settings.ingest.exchanges {
        match ExchangeId::from_str(raw) {
            Ok(id) => connectors.push(build_connector(
                id,
                settings.ingest.depth_levels,
                backoff.clone(),
            )),
            Err(e) => warn!(exchange = %raw, error = %e, "skipping unknown exchange"),
        }
    }
    anyhow::ensure!(!connectors.is_empty(), "no valid exchanges configured");

    // Spawn ingestion.
    let manager = IngestManager::new(connectors, symbols.clone(), IngestParams::default());
    let (mut rx, _conn_handles) = manager.spawn();

    // Drain updates → market state → notify sessions.
    {
        let market = market.clone();
        let events_tx = events_tx.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    maybe = rx.recv() => match maybe {
                        Some(update) => {
                            metrics::counter!("market_updates_total").increment(1);
                            if let Some(instrument) = market.apply(update) {
                                // Ok if there are currently no subscribers.
                                let _ = events_tx.send(instrument);
                            }
                        }
                        None => break, // all connectors stopped
                    },
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() { break; }
                    }
                }
            }
            info!("ingest drain loop stopped");
        });
    }

    // Transfer-status poller.
    if settings.transfer.enabled {
        let exchanges = settings
            .ingest
            .exchanges
            .iter()
            .filter_map(|s| ExchangeId::from_str(s).ok())
            .collect();
        let poll_cfg = PollConfig {
            exchanges,
            assets: settings
                .transfer
                .assets
                .iter()
                .map(|a| a.to_uppercase())
                .collect(),
            interval: Duration::from_secs(settings.transfer.poll_interval_secs),
        };
        let store = store.clone();
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move { run_poller(store, poll_cfg, shutdown_rx).await });
    }

    // HTTP/WS server.
    let oracle: Arc<dyn screener::TransferOracle> = store.clone();
    let state = AppState {
        market: market.clone(),
        oracle,
        events: events_tx.clone(),
        default_cfg: Arc::new(settings.default_client.clone()),
        auth: Arc::new(AuthPolicy::default()),
        metrics_render,
    };
    let app = api::router(state);

    let listener = tokio::net::TcpListener::bind(&settings.server.bind)
        .await
        .with_context(|| format!("binding {}", settings.server.bind))?;
    info!(addr = %settings.server.bind, "listening (REST + /ws)");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_tx))
        .await
        .context("server error")?;

    info!("shutdown complete");
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,arb_screener=debug"));
    fmt().with_env_filter(filter).init();
}

/// Await Ctrl-C, then broadcast shutdown to background tasks.
async fn shutdown_signal(shutdown_tx: watch::Sender<bool>) {
    let _ = tokio::signal::ctrl_c().await;
    info!("ctrl-c received; shutting down");
    let _ = shutdown_tx.send(true);
}
