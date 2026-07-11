//! `ClientConfig` — the per-client screening parameters sent over WS `Subscribe`.
//! Field docs double as the parameter reference for the client app.

use domain::{Decimal, ExchangeId, ALL_EXCHANGES};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

/// Default taker fee used when a client doesn't specify one for a venue.
pub const DEFAULT_TAKER_FEE_STR: &str = "0.0006";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    // --- Universe / selection ---
    /// Venues to include (default: all eight).
    pub exchanges: Vec<ExchangeId>,
    /// Settlement/quote asset (perp margin), e.g. "USDT".
    pub quote: String,
    /// Base-asset allow list; empty = allow all screened symbols.
    pub allow_symbols: Vec<String>,
    /// Base-asset deny list.
    pub deny_symbols: Vec<String>,
    /// Drop markets below this 24h quote volume (USDT). Enforced once ticker
    /// volume ingestion is wired; accepted now for forward compatibility.
    pub min_24h_quote_volume: Decimal,
    /// Optional perp open-interest floor (base units).
    pub min_open_interest: Option<Decimal>,

    // --- Spread band (the 2%..20% control) ---
    pub min_net_spread_pct: Decimal,
    /// Caps ghost spreads (delisted/frozen/wrong-token) that masquerade as huge edges.
    pub max_net_spread_pct: Decimal,
    /// Quote size (USDT) the executable VWAP spread is measured against.
    pub target_notional_q: Decimal,
    /// Require at least this much executable depth on both legs.
    pub min_executable_notional: Decimal,
    /// Book levels to walk for VWAP.
    pub depth_levels_n: u32,

    // --- Fees / funding ---
    /// Per-exchange taker fee fractions.
    pub taker_fee: HashMap<ExchangeId, Decimal>,
    pub include_funding_diff: bool,
    /// Minimum annualized funding differential to surface as a funding signal.
    pub min_funding_diff_apr: Decimal,
    pub funding_hold_hours: Decimal,

    // --- Reality filters (mirage killers) ---
    /// Settlement asset must be deposit+withdraw enabled on both legs.
    pub require_transferable: bool,
    /// Both legs must share >=1 enabled network for the settlement asset.
    pub require_common_network: bool,
    /// Books older than this (ms) are excluded.
    pub max_book_age_ms: u64,

    // --- Noise control ---
    pub hysteresis_step_pct: Decimal,
    pub min_signal_lifetime_ms: u64,
    pub cooldown_ms: u64,
    pub max_signals_per_min: Option<u32>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            exchanges: ALL_EXCHANGES.to_vec(),
            quote: "USDT".to_string(),
            allow_symbols: vec![],
            deny_symbols: vec![],
            min_24h_quote_volume: Decimal::ZERO,
            min_open_interest: None,
            min_net_spread_pct: dec_lit("0.02"),
            max_net_spread_pct: dec_lit("0.20"),
            target_notional_q: dec_lit("2000"),
            min_executable_notional: dec_lit("500"),
            depth_levels_n: 20,
            taker_fee: HashMap::new(),
            include_funding_diff: true,
            min_funding_diff_apr: dec_lit("0.15"),
            funding_hold_hours: dec_lit("8"),
            // Off by default: Phase-1 transfer data is public-only and partial
            // (see config/default.toml). Clients opt in once the store is
            // populated for their venues.
            require_transferable: false,
            require_common_network: false,
            max_book_age_ms: 3000,
            hysteresis_step_pct: dec_lit("0.005"),
            min_signal_lifetime_ms: 1500,
            cooldown_ms: 2000,
            max_signals_per_min: Some(120),
        }
    }
}

impl ClientConfig {
    /// Taker fee for a venue, falling back to the default.
    pub fn taker(&self, ex: ExchangeId) -> Decimal {
        self.taker_fee
            .get(&ex)
            .copied()
            .unwrap_or_else(|| dec_lit(DEFAULT_TAKER_FEE_STR))
    }

    /// Whether a venue is enabled for this client.
    pub fn includes(&self, ex: ExchangeId) -> bool {
        self.exchanges.contains(&ex)
    }

    /// Whether a base asset passes the allow/deny lists.
    pub fn allows_symbol(&self, base: &str) -> bool {
        let base = base.to_uppercase();
        if self.deny_symbols.iter().any(|s| s.to_uppercase() == base) {
            return false;
        }
        if self.allow_symbols.is_empty() {
            return true;
        }
        self.allow_symbols.iter().any(|s| s.to_uppercase() == base)
    }

    /// Basic sanity validation for a client-supplied config.
    pub fn validate(&self) -> Result<(), String> {
        if self.exchanges.is_empty() {
            return Err("exchanges must not be empty".into());
        }
        if self.min_net_spread_pct < Decimal::ZERO {
            return Err("min_net_spread_pct must be >= 0".into());
        }
        if self.max_net_spread_pct < self.min_net_spread_pct {
            return Err("max_net_spread_pct must be >= min_net_spread_pct".into());
        }
        if self.target_notional_q <= Decimal::ZERO {
            return Err("target_notional_q must be > 0".into());
        }
        if self.depth_levels_n == 0 {
            return Err("depth_levels_n must be > 0".into());
        }
        Ok(())
    }
}

/// Parse a decimal literal known to be valid; used only for compile-time defaults.
fn dec_lit(s: &str) -> Decimal {
    Decimal::from_str(s).expect("valid decimal literal")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn default_taker_fallback() {
        let cfg = ClientConfig::default();
        assert_eq!(cfg.taker(ExchangeId::Bybit), dec!(0.0006));
    }

    #[test]
    fn allow_deny_logic() {
        let mut cfg = ClientConfig::default();
        assert!(cfg.allows_symbol("btc")); // empty allow => all
        cfg.deny_symbols = vec!["BTC".into()];
        assert!(!cfg.allows_symbol("btc"));
        cfg.deny_symbols.clear();
        cfg.allow_symbols = vec!["ETH".into()];
        assert!(cfg.allows_symbol("eth"));
        assert!(!cfg.allows_symbol("sol"));
    }

    #[test]
    fn validate_catches_bad_band() {
        let mut cfg = ClientConfig::default();
        cfg.max_net_spread_pct = dec!(0.01);
        cfg.min_net_spread_pct = dec!(0.02);
        assert!(cfg.validate().is_err());
    }
}
