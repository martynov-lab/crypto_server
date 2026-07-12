//! Universe discovery: which USDT-perp base assets are listed on which venues.
//!
//! Polls each exchange's PUBLIC instruments/contracts endpoint, builds a
//! `base -> {exchanges}` map, and answers "screen everything listed on >= N
//! venues" — the coins where cross-exchange perp spreads actually appear (the
//! illiquid long tail, not the majors).

pub mod fetchers;
pub mod poller;

use dashmap::DashMap;
use domain::ExchangeId;
use std::collections::HashSet;

/// Concurrent catalog of `base -> set of venues listing {base}/USDT perp`.
#[derive(Default)]
pub struct UniverseStore {
    listings: DashMap<String, HashSet<ExchangeId>>,
}

impl UniverseStore {
    pub fn new() -> Self {
        UniverseStore {
            listings: DashMap::new(),
        }
    }

    /// Replace one exchange's listed bases (idempotent per refresh).
    pub fn set_exchange(&self, exchange: ExchangeId, bases: &[String]) {
        // Remove this exchange from every base first, then re-add.
        for mut e in self.listings.iter_mut() {
            e.value_mut().remove(&exchange);
        }
        for base in bases {
            self.listings
                .entry(base.to_uppercase())
                .or_default()
                .insert(exchange);
        }
        // Drop bases now listed nowhere.
        self.listings.retain(|_, v| !v.is_empty());
    }

    /// Bases listed on at least `min_venues` exchanges, most-covered first.
    pub fn screenable(&self, min_venues: usize) -> Vec<String> {
        let mut v: Vec<(String, usize)> = self
            .listings
            .iter()
            .filter(|e| e.value().len() >= min_venues)
            .map(|e| (e.key().clone(), e.value().len()))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.into_iter().map(|(base, _)| base).collect()
    }

    /// Like [`screenable`](Self::screenable), but restricted to bases listed on
    /// `anchor` — "take the anchor's coin list, then look for its spread on the
    /// other venues". `min_venues` counts the anchor itself, so 2 means "anchor
    /// plus at least one other venue" (the minimum for a cross-exchange spread).
    pub fn screenable_anchored(&self, anchor: ExchangeId, min_venues: usize) -> Vec<String> {
        let mut v: Vec<(String, usize)> = self
            .listings
            .iter()
            .filter(|e| e.value().contains(&anchor) && e.value().len() >= min_venues.max(2))
            .map(|e| (e.key().clone(), e.value().len()))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.into_iter().map(|(base, _)| base).collect()
    }

    /// Full catalog rows: `(base, sorted venues)`, most-covered first.
    pub fn catalog(&self) -> Vec<(String, Vec<ExchangeId>)> {
        let mut rows: Vec<(String, Vec<ExchangeId>)> = self
            .listings
            .iter()
            .map(|e| {
                let mut xs: Vec<ExchangeId> = e.value().iter().copied().collect();
                xs.sort_by_key(|x| x.as_str());
                (e.key().clone(), xs)
            })
            .collect();
        rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
        rows
    }

    pub fn len(&self) -> usize {
        self.listings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.listings.is_empty()
    }
}

pub use poller::{run_poller, DiscoveryConfig};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchored_requires_anchor_listing_and_a_counterparty() {
        let store = UniverseStore::new();
        store.set_exchange(ExchangeId::Bybit, &["BTC".into(), "AAA".into(), "SOLO".into()]);
        store.set_exchange(ExchangeId::Okx, &["BTC".into(), "AAA".into(), "XYZ".into()]);
        store.set_exchange(ExchangeId::Gate, &["AAA".into(), "XYZ".into()]);

        // XYZ is on 2 venues but not on the anchor; SOLO is anchor-only.
        let anchored = store.screenable_anchored(ExchangeId::Bybit, 2);
        assert_eq!(anchored, vec!["AAA".to_string(), "BTC".to_string()]); // most-covered first

        // min_venues below 2 is clamped: a spread needs a counterparty.
        let anchored = store.screenable_anchored(ExchangeId::Bybit, 1);
        assert!(!anchored.contains(&"SOLO".to_string()));
    }
}
