//! Transfer-status store + poller.
//!
//! Phase 1 uses **public** currency endpoints only (no exchange keys, per spec
//! §0). Public deposit/withdraw status is available for gate, kucoin, bitget,
//! coinex and phemex; bybit, okx and mexc gate this behind authenticated
//! endpoints, so their status stays unknown until a later (keyed) phase. When a
//! venue's status is unknown, transfer-based filters fail closed for that venue.

pub mod fetchers;
pub mod poller;

use dashmap::DashMap;
use domain::{ExchangeId, TransferStatus};
use screener::TransferOracle;

/// Concurrent map of `(exchange, asset) -> TransferStatus`, refreshed by the poller.
#[derive(Default)]
pub struct TransferStore {
    statuses: DashMap<(ExchangeId, String), TransferStatus>,
}

impl TransferStore {
    pub fn new() -> Self {
        TransferStore {
            statuses: DashMap::new(),
        }
    }

    pub fn upsert(&self, exchange: ExchangeId, status: TransferStatus) {
        let key = (exchange, status.asset.to_uppercase());
        self.statuses.insert(key, status);
    }

    pub fn get(&self, exchange: ExchangeId, asset: &str) -> Option<TransferStatus> {
        self.statuses
            .get(&(exchange, asset.to_uppercase()))
            .map(|v| v.clone())
    }

    pub fn len(&self) -> usize {
        self.statuses.len()
    }

    pub fn is_empty(&self) -> bool {
        self.statuses.is_empty()
    }
}

impl TransferOracle for TransferStore {
    fn status(&self, exchange: ExchangeId, asset: &str) -> Option<TransferStatus> {
        self.get(exchange, asset)
    }
}

pub use poller::{run_poller, PollConfig};
