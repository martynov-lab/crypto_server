//! `ClientConfig` — the per-client screening parameters sent over WS `Subscribe`.
//! Field docs double as the parameter reference for the client app.

use domain::{Decimal, ExchangeId, MarketKind, ALL_EXCHANGES};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

/// Default taker fee used when a client doesn't specify one for a venue.
pub const DEFAULT_TAKER_FEE_STR: &str = "0.0006";

/// One market-kind combination the client wants screened: the kind of the
/// buy (long) leg × the kind of the sell (short) leg. `perp`/`perp` is the
/// classic futures/futures arb; mixed pairs (`spot`/`perp`, `perp`/`spot`)
/// cover cash-and-carry style setups once spot ingestion is wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketPair {
    pub buy: MarketKind,
    pub sell: MarketKind,
}

impl MarketPair {
    pub const fn new(buy: MarketKind, sell: MarketKind) -> Self {
        MarketPair { buy, sell }
    }

    /// Futures/futures — the only pair the pipeline ingests today.
    pub const PERP_PERP: MarketPair = MarketPair::new(MarketKind::Perp, MarketKind::Perp);
    pub const SPOT_SPOT: MarketPair = MarketPair::new(MarketKind::Spot, MarketKind::Spot);
    pub const SPOT_PERP: MarketPair = MarketPair::new(MarketKind::Spot, MarketKind::Perp);
    pub const PERP_SPOT: MarketPair = MarketPair::new(MarketKind::Perp, MarketKind::Spot);
}

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
    /// Market-kind combinations to screen (buy leg × sell leg). Default:
    /// perp/perp only. Spot legs are accepted now for forward compatibility
    /// and take effect once spot ingestion is wired.
    pub market_pairs: Vec<MarketPair>,
    /// Drop markets below this 24h quote volume (USDT). Together with
    /// `max_24h_quote_volume` this forms the volume band the client screens.
    pub min_24h_quote_volume: Decimal,
    /// Drop markets above this 24h quote volume (USDT); `None` = no ceiling
    /// (the default). Set a low-cap band (e.g. 100k..200k) to target thin coins
    /// where spreads persist, instead of hyper-liquid majors.
    pub max_24h_quote_volume: Option<Decimal>,
    /// Optional perp open-interest floor (base units).
    pub min_open_interest: Option<Decimal>,

    // --- Spread band (the 0.6%..25% control) ---
    /// Floor on the **entry** spread (net of the two entry taker fees).
    /// Crossing it produces an `info`-level signal: shown in the client's list
    /// but not meant to notify.
    pub min_net_spread_pct: Decimal,
    /// Second, higher threshold: a signal whose entry spread reaches this level
    /// is marked `alert` — the one the client actually notifies on. The upgrade
    /// of an open episode from `info` to `alert` is pushed immediately,
    /// bypassing the hysteresis step and cooldown (but not the rate cap).
    pub alert_net_spread_pct: Decimal,
    /// Caps ghost spreads (delisted/frozen/wrong-token) that masquerade as huge edges.
    pub max_net_spread_pct: Decimal,
    /// Quote size (USDT) the executable VWAP spread is measured against.
    pub target_notional_q: Decimal,
    /// Require at least this much executable depth on both legs.
    pub min_executable_notional: Decimal,
    /// Book levels to walk for VWAP.
    pub depth_levels_n: u32,
    /// Floor on the **round-trip** edge: the entry spread minus the expected
    /// unwind level, four taker fees, and the funding carry. This is the real
    /// profitability gate — `min_net_spread_pct` only bounds the entry.
    pub min_round_trip_pct: Decimal,

    // --- Fees / funding ---
    /// Per-exchange taker fee fractions.
    pub taker_fee: HashMap<ExchangeId, Decimal>,
    pub include_funding_diff: bool,
    /// Minimum annualized funding differential to surface as a funding signal.
    pub min_funding_diff_apr: Decimal,
    /// Assumed holding period, used both to annualize the funding signal and to
    /// charge the position's funding carry against the round-trip edge.
    pub funding_hold_hours: Decimal,
    /// Subtract the expected funding carry from the round-trip edge. A perp
    /// pair pays funding on both legs while it waits for convergence.
    pub include_funding_cost: bool,

    // --- Reality filters (mirage killers) ---
    /// Settlement asset must be deposit+withdraw enabled on both legs.
    pub require_transferable: bool,
    /// Both legs must share >=1 enabled network for the settlement asset.
    pub require_common_network: bool,
    /// Books older than this (ms) are excluded.
    pub max_book_age_ms: u64,
    /// Maximum age difference between the two legs' books. Both legs can be
    /// individually "fresh" yet describe moments far enough apart that the
    /// spread between them never existed; this bounds that skew.
    pub max_leg_skew_ms: u64,
    /// Reject a venue whose mid price deviates from the cross-venue median by
    /// more than this fraction — the signature of a wrong token, a frozen
    /// market, or a redenomination. `None` disables the check.
    pub max_price_deviation_pct: Option<Decimal>,

    // --- Spread dynamics (tight-baseline-with-spikes detection) ---
    /// Master switch for the dynamics filters below.
    pub enable_dynamics: bool,
    /// Reject coins whose *baseline* (median) spread is above this — a
    /// persistently wide spread is a structural break, not an opportunity.
    pub max_baseline_spread_pct: Decimal,
    /// Require the current spread to be at least this many robust deviations
    /// above its own baseline (a genuine spike, not "it's always wide").
    pub min_spike_z: Decimal,
    /// Soft override for the spike requirement: a pairing whose round-trip
    /// edge is at least `min_round_trip_pct × this` passes even without a
    /// spike. A steady, executable, profitable spread is a signal too — the
    /// spike shape then only affects `quality_score`. `None` = spike is a hard
    /// requirement.
    pub spike_bypass_round_trip_mult: Option<Decimal>,
    /// Reject a spread that has stayed above threshold longer than this — a
    /// healthy arb closes fast; a long-lived wide gap is a trap.
    pub max_spread_duration_ms: u64,
    /// Don't apply dynamics filters until this many samples exist (warmup).
    pub min_dynamics_samples: u32,

    // --- Chart (watch stream) ---
    /// Longest long-history window (ms) this client wants back from
    /// `/spread/range`. Requests are additionally capped by the server's
    /// retention (`chart.history_window_ms`, default 3 days) — the server
    /// cannot return more than it retains, so this only ever tightens.
    pub history_window_ms: u64,
    /// Per-client anomaly cutoff for the live spread chart: watch backfill and
    /// ticks with `|net_pct|` above this are dropped before delivery to this
    /// client (a wrong-token / stale-quote spike is a data error, not a signal).
    /// The server also enforces a global hard cap; this only tightens it.
    pub max_chart_spread_pct: Decimal,

    // --- Noise control ---
    pub hysteresis_step_pct: Decimal,
    /// Consecutive rejected evaluations required before an open episode is
    /// considered closed and hysteresis resets. Without this, a single tick
    /// that grazes a filter boundary re-arms the engine and the same
    /// opportunity alerts again on the next tick.
    pub episode_close_ticks: u32,
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
            market_pairs: vec![MarketPair::PERP_PERP],
            min_24h_quote_volume: dec_lit("100000"),
            max_24h_quote_volume: None,
            min_open_interest: None,
            min_net_spread_pct: dec_lit("0.006"),
            alert_net_spread_pct: dec_lit("0.01"),
            max_net_spread_pct: dec_lit("0.25"),
            target_notional_q: dec_lit("2000"),
            min_executable_notional: dec_lit("500"),
            depth_levels_n: 20,
            min_round_trip_pct: dec_lit("0.001"),
            taker_fee: HashMap::new(),
            include_funding_diff: true,
            min_funding_diff_apr: dec_lit("0.15"),
            funding_hold_hours: dec_lit("8"),
            include_funding_cost: true,
            // Off by default: Phase-1 transfer data is public-only and partial
            // (see config/default.toml). Clients opt in once the store is
            // populated for their venues.
            require_transferable: false,
            require_common_network: false,
            max_book_age_ms: 3000,
            max_leg_skew_ms: 750,
            max_price_deviation_pct: Some(dec_lit("0.10")),
            enable_dynamics: true,
            max_baseline_spread_pct: dec_lit("0.01"),
            min_spike_z: dec_lit("3"),
            spike_bypass_round_trip_mult: Some(dec_lit("2")),
            max_spread_duration_ms: 300_000,
            min_dynamics_samples: 20,
            max_chart_spread_pct: dec_lit("0.50"),
            history_window_ms: 259_200_000, // 3 days
            hysteresis_step_pct: dec_lit("0.005"),
            episode_close_ticks: 3,
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

    /// Whether the client screens this buy-leg × sell-leg market combination.
    pub fn allows_market_pair(&self, buy: MarketKind, sell: MarketKind) -> bool {
        self.market_pairs.contains(&MarketPair::new(buy, sell))
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
        if self.market_pairs.is_empty() {
            return Err("market_pairs must not be empty".into());
        }
        if let Some(max_vol) = self.max_24h_quote_volume {
            if max_vol < self.min_24h_quote_volume {
                return Err("max_24h_quote_volume must be >= min_24h_quote_volume".into());
            }
        }
        if self.min_net_spread_pct < Decimal::ZERO {
            return Err("min_net_spread_pct must be >= 0".into());
        }
        if self.max_net_spread_pct < self.min_net_spread_pct {
            return Err("max_net_spread_pct must be >= min_net_spread_pct".into());
        }
        if self.alert_net_spread_pct < self.min_net_spread_pct {
            return Err("alert_net_spread_pct must be >= min_net_spread_pct".into());
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

    #[test]
    fn validate_catches_bad_volume_band() {
        let mut cfg = ClientConfig::default();
        cfg.min_24h_quote_volume = dec!(200000);
        cfg.max_24h_quote_volume = Some(dec!(100000));
        assert!(cfg.validate().is_err());
        cfg.max_24h_quote_volume = None; // no ceiling is always fine
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn default_is_perp_perp_only() {
        let cfg = ClientConfig::default();
        assert!(cfg.allows_market_pair(MarketKind::Perp, MarketKind::Perp));
        assert!(!cfg.allows_market_pair(MarketKind::Spot, MarketKind::Spot));
        assert!(!cfg.allows_market_pair(MarketKind::Spot, MarketKind::Perp));
    }

    #[test]
    fn validate_catches_empty_market_pairs() {
        let mut cfg = ClientConfig::default();
        cfg.market_pairs.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn market_pair_serde_roundtrip() {
        let j = serde_json::to_string(&MarketPair::SPOT_PERP).unwrap();
        assert_eq!(j, r#"{"buy":"spot","sell":"perp"}"#);
        let p: MarketPair = serde_json::from_str(&j).unwrap();
        assert_eq!(p, MarketPair::SPOT_PERP);
    }
}
