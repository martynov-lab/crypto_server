//! Arbitrage screener server (Phase 1): wires connectors → ingest → market
//! state → screener → axum WS/REST, with a transfer-status poller and graceful
//! shutdown.

mod settings;

use anyhow::Context;
use api::{AppState, ChartParams};
use auth::AuthPolicy;
use connectors::{build_connector, common::Backoff};
use domain::{ExchangeId, Instrument};
use ingest::{IngestManager, IngestParams};
use market_state::MarketState;
use metrics_exporter_prometheus::PrometheusBuilder;
use settings::Settings;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};
use transfer_status::{run_poller, PollConfig, TransferStore};
use universe::poller::refresh_once;
use universe::{DiscoveryConfig, UniverseStore};

/// Build per-exchange subscription lists. With `auto_discover`, screen every base
/// listed on >= `min_venues` venues (capped at `max_symbols`, most-covered first),
/// each assigned only to the venues that actually list it. Otherwise use the
/// static `symbols` list, still filtered by discovered listings when available.
fn build_symbol_map(
    settings: &Settings,
    enabled: &[ExchangeId],
    universe: &UniverseStore,
    quote: &str,
) -> HashMap<ExchangeId, Vec<Instrument>> {
    // base -> venues that list it.
    let listed: HashMap<String, Vec<ExchangeId>> = universe.catalog().into_iter().collect();

    let bases: Vec<String> = if settings.ingest.auto_discover && !universe.is_empty() {
        let mut b = universe.screenable(settings.ingest.min_venues);
        b.truncate(settings.ingest.max_symbols);
        b
    } else {
        settings
            .ingest
            .symbols
            .iter()
            .map(|s| s.to_uppercase())
            .collect()
    };

    let mut map: HashMap<ExchangeId, Vec<Instrument>> =
        enabled.iter().map(|&e| (e, Vec::new())).collect();
    for base in &bases {
        let venues: Vec<ExchangeId> = match listed.get(base) {
            // Restrict to venues that actually list it (∩ enabled).
            Some(v) => v.iter().copied().filter(|e| enabled.contains(e)).collect(),
            // Unknown listing (discovery failed): try everywhere.
            None => enabled.to_vec(),
        };
        for v in venues {
            if let Some(list) = map.get_mut(&v) {
                list.push(Instrument::perp(base, quote));
            }
        }
    }
    map
}

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
    let dynamics_cfg = market_state::dynamics::DynamicsConfig {
        window: Duration::from_secs(300),
        min_sample_interval: Duration::from_millis(500),
        reference_threshold: settings.default_client.min_net_spread_pct,
    };
    let market = Arc::new(MarketState::with_dynamics(
        Duration::from_millis(settings.ingest.staleness_ms),
        dynamics_cfg,
    ));
    let tape = Arc::new(spread_tape::SpreadTape::new(
        Duration::from_millis(settings.chart.window_ms),
        Duration::from_millis(settings.chart.resolution_ms),
        128,
    ));
    let store = Arc::new(TransferStore::new());
    let (events_tx, _events_rx) = broadcast::channel::<Instrument>(1024);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Enabled exchanges.
    let mut enabled: Vec<ExchangeId> = Vec::new();
    for raw in &settings.ingest.exchanges {
        match ExchangeId::from_str(raw) {
            Ok(id) => enabled.push(id),
            Err(e) => warn!(exchange = %raw, error = %e, "skipping unknown exchange"),
        }
    }
    anyhow::ensure!(!enabled.is_empty(), "no valid exchanges configured");

    // Universe discovery: build the catalog now (powers /instruments and, when
    // auto_discover is on, the screened symbol set).
    let universe_store = Arc::new(UniverseStore::new());
    if let Ok(client) = universe::poller::build_client() {
        refresh_once(&client, &universe_store, &enabled).await;
    } else {
        warn!("universe discovery client unavailable");
    }

    // Choose the screened bases and per-exchange subscription lists.
    let quote = settings.ingest.quote.clone();
    let symbols_by_exchange =
        build_symbol_map(&settings, &enabled, &universe_store, &quote);
    let total: usize = symbols_by_exchange.values().map(|v| v.len()).sum();
    info!(
        bases = universe_store.len(),
        subscriptions = total,
        "screening instruments"
    );

    // Build connectors for the enabled exchanges.
    let backoff = Backoff::new(
        Duration::from_millis(settings.ingest.reconnect.initial_backoff_ms),
        Duration::from_millis(settings.ingest.reconnect.max_backoff_ms),
        Duration::from_millis(settings.ingest.reconnect.jitter_ms),
    );
    let connectors: Vec<_> = enabled
        .iter()
        .map(|&id| build_connector(id, settings.ingest.depth_levels, backoff.clone()))
        .collect();

    // Spawn ingestion.
    let manager = IngestManager::new(connectors, symbols_by_exchange, IngestParams::default());
    let (mut rx, _conn_handles) = manager.spawn();

    let default_cfg = Arc::new(settings.default_client.clone());

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

    // Fixed-cadence spread sampler: every resolution_ms, sample each screened
    // instrument's raw best spread into the dynamics history and the chart tape.
    // Decoupled from the alert engine (no hysteresis/cooldown/filters).
    {
        let market = market.clone();
        let tape = tape.clone();
        let default_cfg = default_cfg.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        let resolution = Duration::from_millis(settings.chart.resolution_ms);
        let sanity_cap = settings.chart.sanity_max_spread_pct;
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(resolution);
            loop {
                tokio::select! {
                    _ = timer.tick() => {
                        let now = std::time::Instant::now();
                        let ts_ms = chrono::Utc::now().timestamp_millis();
                        for instrument in market.instruments() {
                            let snap = market.snapshot(&instrument, now);
                            // Dynamics feed (best pair) with the sanity guard so a
                            // data-error spike can't pollute baseline/z-score.
                            if let Some(point) =
                                screener::best_spread_point(&snap, &default_cfg, ts_ms)
                            {
                                if point.net_pct.abs() <= sanity_cap {
                                    market.record_spread(&instrument, point.net_pct, now);
                                } else {
                                    metrics::counter!("spread_anomalies_dropped_total").increment(1);
                                }
                            }
                            // Chart tape: per-venue VWAP snapshot. Pair selection
                            // and In/Out/funding are derived per-watcher at delivery.
                            if let Some(sample) =
                                screener::venue_sample(&snap, &default_cfg, ts_ms)
                            {
                                tape.record(&instrument, sample);
                            }
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() { break; }
                    }
                }
            }
            info!("spread sampler stopped");
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

    // Universe discovery poller (keeps /instruments catalog fresh).
    {
        let disc_cfg = DiscoveryConfig {
            exchanges: enabled.clone(),
            interval: Duration::from_secs(settings.ingest.discovery_interval_secs),
        };
        let universe_store = universe_store.clone();
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move { universe::run_poller(universe_store, disc_cfg, shutdown_rx).await });
    }

    // HTTP/WS server.
    let oracle: Arc<dyn screener::TransferOracle> = store.clone();
    let state = AppState {
        market: market.clone(),
        oracle,
        universe: universe_store.clone(),
        tape: tape.clone(),
        chart: ChartParams {
            max_window_ms: settings.chart.window_ms,
            max_watches: settings.chart.max_watches,
            sanity_max_spread_pct: settings.chart.sanity_max_spread_pct,
        },
        events: events_tx.clone(),
        default_cfg: default_cfg.clone(),
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
