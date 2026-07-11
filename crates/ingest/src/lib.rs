//! Ingest manager: supervises exchange connectors and funnels every normalized
//! [`MarketUpdate`] into a single channel consumed by `market_state`.

pub mod manager;

pub use manager::{IngestManager, IngestParams};
