//! Transfer (deposit/withdraw) status for an asset on an exchange.
//!
//! For perp arbitrage this is used against the **settlement asset** (USDT): to
//! rebalance margin between the buy and sell venue you must be able to withdraw
//! from one and deposit to the other over a shared network. The type is generic
//! over asset so base-coin (spot) transfer checks reuse it later.

use serde::{Deserialize, Serialize};

/// One on-chain network an asset can move over on a given exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    /// Canonical, upper-cased chain id, e.g. "ETH", "TRX", "BSC", "ARBITRUM".
    pub chain: String,
    pub deposit_enabled: bool,
    pub withdraw_enabled: bool,
}

/// Aggregate transfer status of an asset on one exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferStatus {
    pub asset: String, // upper-cased, e.g. "USDT"
    pub deposit_enabled: bool,
    pub withdraw_enabled: bool,
    pub networks: Vec<Network>,
}

impl TransferStatus {
    /// Fully transferable = at least one network with deposit and one with
    /// withdraw enabled (per-flag aggregate already precomputed by the poller).
    pub fn is_transferable(&self) -> bool {
        self.deposit_enabled && self.withdraw_enabled
    }

    /// Set of chains where withdraw is enabled.
    pub fn withdrawable_chains(&self) -> impl Iterator<Item = &str> {
        self.networks
            .iter()
            .filter(|n| n.withdraw_enabled)
            .map(|n| n.chain.as_str())
    }

    /// Set of chains where deposit is enabled.
    pub fn depositable_chains(&self) -> impl Iterator<Item = &str> {
        self.networks
            .iter()
            .filter(|n| n.deposit_enabled)
            .map(|n| n.chain.as_str())
    }
}

/// True if funds can move from `from` (withdraw) to `to` (deposit) over at least
/// one shared chain. This is the check that decides whether a perp margin
/// rebalance between two venues is actually possible.
pub fn has_common_transfer_route(from: &TransferStatus, to: &TransferStatus) -> bool {
    let deposit_chains: std::collections::HashSet<&str> = to.depositable_chains().collect();
    from.withdrawable_chains()
        .any(|c| deposit_chains.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(chain: &str, dep: bool, wd: bool) -> Network {
        Network {
            chain: chain.to_string(),
            deposit_enabled: dep,
            withdraw_enabled: wd,
        }
    }

    #[test]
    fn common_route_requires_shared_enabled_chain() {
        let a = TransferStatus {
            asset: "USDT".into(),
            deposit_enabled: true,
            withdraw_enabled: true,
            networks: vec![net("ETH", true, true), net("TRX", true, true)],
        };
        let b = TransferStatus {
            asset: "USDT".into(),
            deposit_enabled: true,
            withdraw_enabled: true,
            networks: vec![net("TRX", true, false), net("BSC", true, true)],
        };
        // a can withdraw over ETH/TRX; b can deposit over TRX/BSC => TRX shared.
        assert!(has_common_transfer_route(&a, &b));

        // Reverse: b withdraws only over BSC; a deposits over ETH/TRX => none shared.
        assert!(!has_common_transfer_route(&b, &a));
    }
}
