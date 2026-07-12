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
}

impl Default for ChartCfg {
    fn default() -> Self {
        ChartCfg {
            resolution_ms: 1000,
            window_ms: 1_800_000, // 30 min
            max_watches: 3,
            sanity_max_spread_pct: Decimal::new(50, 2), // 0.50
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ServerCfg {
    pub bind: String,
}

#[derive(Debug, Deserialize)]
pub struct IngestCfg {
    pub exchanges: Vec<String>,
    pub symbols: Vec<String>,
    pub quote: String,
    pub depth_levels: usize,
    pub staleness_ms: u64,
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
    #[serde(default = "default_discovery_interval")]
    pub discovery_interval_secs: u64,
    pub reconnect: ReconnectCfg,
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
        assert!(settings.default_client.max_24h_quote_volume.is_some());
    }
}
