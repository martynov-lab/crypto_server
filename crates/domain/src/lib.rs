//! Domain types and traits for the arbitrage screener.
//!
//! This crate has no dependency on any specific exchange. Everything inside the
//! system speaks in these canonical types; connectors are responsible for
//! translating exchange-specific wire formats to and from them.

pub mod funding;
pub mod instrument;
pub mod spread;
pub mod traits;
pub mod transfer;
pub mod types;

pub use funding::FundingInfo;
pub use instrument::{Instrument, MarketKind};
pub use spread::{Spread, SpreadReason};
pub use traits::ExchangeConnector;
pub use transfer::{Network, TransferStatus};
pub use types::{BookLevel, ExchangeId, MarketUpdate, TopBook, ALL_EXCHANGES};

/// Convenience re-export so downstream crates use one Decimal type.
pub use rust_decimal::Decimal;
