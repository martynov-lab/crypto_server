//! Universe discovery: which USDT-perp base assets are listed on which venues.
//!
//! Polls each exchange's PUBLIC instruments/contracts endpoint, builds a
//! `base -> {exchanges}` map, and answers "screen everything listed on >= N
//! venues" — the coins where cross-exchange perp spreads actually appear (the
//! illiquid long tail, not the majors).

pub mod fetchers;
pub mod poller;

use dashmap::DashMap;
use domain::{Decimal, ExchangeId};
use fetchers::Listing;
use std::collections::{HashMap, HashSet};

/// Concurrent catalog of `base -> set of venues listing {base}/USDT perp`, plus
/// each venue's contract size for that base.
#[derive(Default)]
pub struct UniverseStore {
    listings: DashMap<String, HashSet<ExchangeId>>,
    /// `(exchange, base) -> base units per contract`. Only venues that quote
    /// book sizes in contracts have anything other than 1 here.
    sizes: DashMap<(ExchangeId, String), Decimal>,
}

impl UniverseStore {
    pub fn new() -> Self {
        UniverseStore {
            listings: DashMap::new(),
            sizes: DashMap::new(),
        }
    }

    /// Replace one exchange's listings (idempotent per refresh).
    pub fn set_exchange(&self, exchange: ExchangeId, listings: &[Listing]) {
        // Remove this exchange from every base first, then re-add.
        for mut e in self.listings.iter_mut() {
            e.value_mut().remove(&exchange);
        }
        self.sizes.retain(|(ex, _), _| *ex != exchange);
        for l in listings {
            let base = l.base.to_uppercase();
            self.listings
                .entry(base.clone())
                .or_default()
                .insert(exchange);
            self.sizes.insert((exchange, base), l.contract_size);
        }
        // Drop bases now listed nowhere.
        self.listings.retain(|_, v| !v.is_empty());
    }

    /// Base units per contract for `base` on `exchange`. Defaults to `1` — an
    /// unknown multiplier must never silently rescale a book.
    pub fn contract_size(&self, exchange: ExchangeId, base: &str) -> Decimal {
        self.sizes
            .get(&(exchange, base.to_uppercase()))
            .map(|e| *e.value())
            .unwrap_or(Decimal::ONE)
    }

    /// All of one exchange's contract sizes, as a plain map for handing to a
    /// connector at startup.
    pub fn contract_sizes(&self, exchange: ExchangeId) -> HashMap<String, Decimal> {
        self.sizes
            .iter()
            .filter(|e| e.key().0 == exchange)
            .map(|e| (e.key().1.clone(), *e.value()))
            .collect()
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

    /// Listings with the default (base-unit) contract size.
    fn plain(bases: &[&str]) -> Vec<Listing> {
        bases
            .iter()
            .map(|b| Listing {
                base: b.to_string(),
                contract_size: Decimal::ONE,
            })
            .collect()
    }

    #[test]
    fn contract_sizes_are_per_exchange_and_default_to_one() {
        let store = UniverseStore::new();
        store.set_exchange(
            ExchangeId::Gate,
            &[Listing {
                base: "PEPE".into(),
                contract_size: Decimal::from(10_000_000),
            }],
        );
        store.set_exchange(ExchangeId::Bybit, &plain(&["PEPE"]));

        assert_eq!(
            store.contract_size(ExchangeId::Gate, "pepe"),
            Decimal::from(10_000_000)
        );
        assert_eq!(store.contract_size(ExchangeId::Bybit, "PEPE"), Decimal::ONE);
        // Never-seen pairs must not scale.
        assert_eq!(store.contract_size(ExchangeId::Okx, "PEPE"), Decimal::ONE);
    }

    #[test]
    fn refresh_replaces_an_exchanges_sizes() {
        let store = UniverseStore::new();
        store.set_exchange(
            ExchangeId::Gate,
            &[Listing {
                base: "AAA".into(),
                contract_size: Decimal::from(100),
            }],
        );
        // AAA delisted, BBB listed: the stale multiplier must not linger.
        store.set_exchange(ExchangeId::Gate, &plain(&["BBB"]));
        assert_eq!(store.contract_size(ExchangeId::Gate, "AAA"), Decimal::ONE);
    }

    #[test]
    fn anchored_requires_anchor_listing_and_a_counterparty() {
        let store = UniverseStore::new();
        store.set_exchange(ExchangeId::Bybit, &plain(&["BTC", "AAA", "SOLO"]));
        store.set_exchange(ExchangeId::Okx, &plain(&["BTC", "AAA", "XYZ"]));
        store.set_exchange(ExchangeId::Gate, &plain(&["AAA", "XYZ"]));

        // XYZ is on 2 venues but not on the anchor; SOLO is anchor-only.
        let anchored = store.screenable_anchored(ExchangeId::Bybit, 2);
        assert_eq!(anchored, vec!["AAA".to_string(), "BTC".to_string()]); // most-covered first

        // min_venues below 2 is clamped: a spread needs a counterparty.
        let anchored = store.screenable_anchored(ExchangeId::Bybit, 1);
        assert!(!anchored.contains(&"SOLO".to_string()));
    }
}
