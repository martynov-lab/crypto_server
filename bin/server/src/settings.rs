//! Layered configuration: `config/default.toml` overlaid by `ARB__*` env vars.

use config::{Config, Environment, File};
use screener::ClientConfig;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub server: ServerCfg,
    pub ingest: IngestCfg,
    pub transfer: TransferCfg,
    pub default_client: ClientConfig,
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
