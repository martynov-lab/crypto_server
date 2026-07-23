//! Layered configuration: `config/default.toml` overlaid by `ARB__*` env vars.

use config::{Config, Environment, File};
use domain::Decimal;
use screener::ClientConfig;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub server: ServerCfg,
    pub ingest: IngestCfg,
    pub transfer: TransferCfg,
    #[serde(default)]
    pub chart: ChartCfg,
    #[serde(default)]
    pub persistence: PersistenceCfg,
    pub default_client: ClientConfig,
}

/// Real-time spread-chart sampling and watch limits.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ChartCfg {
    /// Fixed sampling cadence for the raw spread tape.
    pub resolution_ms: u64,
    /// Ring-buffer retention per instrument.
    pub window_ms: u64,
    /// Max concurrent watches per WS session.
    pub max_watches: usize,
    /// Global hard anomaly cap: samples with `|net_pct|` above this are treated
    /// as data errors and dropped from BOTH the shared tape and the dynamics
    /// history (protects every client's chart + baseline/z-score). Set well
    /// above the legit alert band (e.g. 0.50 = 50%).
    pub sanity_max_spread_pct: Decimal,
    /// Long-history retention (server resource bound): how far back the coarse
    /// best-pair spread aggregates go. Memory ≈ 72 bytes × (window/resolution)
    /// per instrument (~300 KB per coin for 3 days of 1-minute buckets).
    pub history_window_ms: u64,
    /// Bucket size of the long history.
    pub history_resolution_ms: u64,
}

impl Default for ChartCfg {
    fn default() -> Self {
        ChartCfg {
            resolution_ms: 1000,
            window_ms: 1_800_000, // 30 min
            max_watches: 3,
            sanity_max_spread_pct: Decimal::new(50, 2), // 0.50
            history_window_ms: 259_200_000, // 3 days
            history_resolution_ms: 60_000,  // 1-minute buckets
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ServerCfg {
    pub bind: String,
    /// Print every screening signal (evaluated with the current client config)
    /// to the terminal, so the operator sees the same spread alerts a subscribed
    /// client would — handy while setting the system up before any client connects.
    #[serde(default = "default_log_signals")]
    pub log_signals: bool,
    /// Where the client's screening config is persisted. The client's
    /// `subscribe` overwrites this file; on restart it takes precedence over
    /// `default_client` (which then only bootstraps a fresh install).
    #[serde(default = "default_client_config_path")]
    pub client_config_path: String,
}

fn default_log_signals() -> bool {
    true
}

fn default_client_config_path() -> String {
    "data/client_config.json".to_string()
}

#[derive(Debug, Deserialize)]
pub struct IngestCfg {
    pub exchanges: Vec<String>,
    pub symbols: Vec<String>,
    pub quote: String,
    pub depth_levels: usize,
    pub staleness_ms: u64,
    /// Absolute freshness cap (ms) for an unchanged book on a live connection.
    /// Event-driven feeds send nothing while a quiet coin's book doesn't move,
    /// so such books stay usable up to this age; a frozen/delisted symbol still
    /// goes stale once it is exceeded.
    #[serde(default = "default_quiet_book_max_ms")]
    pub quiet_book_max_ms: u64,
    #[serde(default)]
    pub auto_discover: bool,
    /// When set, discovery takes this exchange's coin list as the source and
    /// screens those bases against the other venues (e.g. "bybit"). Unset =
    /// screen everything listed on >= min_venues exchanges.
    #[serde(default)]
    pub anchor_exchange: Option<String>,
    #[serde(default = "default_min_venues")]
    pub min_venues: usize,
    #[serde(default = "default_max_symbols")]
    pub max_symbols: usize,
    /// Bases to screen unconditionally, on top of (and unaffected by) the
    /// discovery ranking and the `max_symbols` cap. The answer to "why is there
    /// no signal for coin X" when X isn't among the most-covered coins.
    #[serde(default)]
    pub always_screen: Vec<String>,
    #[serde(default = "default_discovery_interval")]
    pub discovery_interval_secs: u64,
    pub reconnect: ReconnectCfg,
}

fn default_quiet_book_max_ms() -> u64 {
    30_000
}
fn default_min_venues() -> usize {
    2
}
fn default_max_symbols() -> usize {
    150
}
fn default_discovery_interval() -> u64 {
    900
}

#[derive(Debug, Deserialize)]
pub struct ReconnectCfg {
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub jitter_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct TransferCfg {
    pub enabled: bool,
    pub poll_interval_secs: u64,
    pub assets: Vec<String>,
}

/// Optional CSV log of every emitted signal (spec §8.5 — track whether signals
/// actually stay tradeable after the fact).
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct PersistenceCfg {
    /// Path to append signals to. Empty (the default) disables logging.
    pub csv_path: String,
}

impl Settings {
    /// Load config. Base file path is `config/default` unless `ARB_CONFIG` is set.
    /// Env overrides use the `ARB__` prefix with `__` nesting, e.g.
    /// `ARB__SERVER__BIND=0.0.0.0:9000`.
    pub fn load() -> anyhow::Result<Self> {
        let base = std::env::var("ARB_CONFIG").unwrap_or_else(|_| "config/default".to_string());
        let cfg = Config::builder()
            .add_source(File::with_name(&base))
            .add_source(
                Environment::with_prefix("ARB")
                    .prefix_separator("__")
                    .separator("__"),
            )
            .build()?;
        Ok(cfg.try_deserialize()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use screener::MarketPair;

    /// The shipped config/default.toml must deserialize and validate.
    #[test]
    fn default_toml_parses_and_validates() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/default");
        let cfg = Config::builder()
            .add_source(File::with_name(path))
            .build()
            .expect("read config/default.toml");
        let settings: Settings = cfg.try_deserialize().expect("deserialize Settings");
        settings
            .default_client
            .validate()
            .expect("default_client validates");
        assert_eq!(settings.default_client.market_pairs, vec![MarketPair::PERP_PERP]);
        // Volume ceiling is opt-in: shipped defaults screen everything above the floor.
        assert!(settings.default_client.max_24h_quote_volume.is_none());
    }
}
