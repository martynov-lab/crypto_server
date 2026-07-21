//! Exchange connectors. Each venue implements the thin [`common::WsExchange`]
//! trait; [`WsConnector`] adapts any `WsExchange` into a [`domain::ExchangeConnector`]
//! via the shared reconnecting driver. [`build_connector`] is the factory used
//! by ingest/wiring.

pub mod book;
pub mod common;
pub mod util;

pub mod bybit;
pub mod okx;
pub mod mexc;
pub mod bitget;
pub mod gate;
pub mod coinex;
pub mod kucoin;
pub mod phemex;

pub mod mock;

use async_trait::async_trait;
use common::{run_ws_exchange, Backoff, WsExchange};
use domain::{Decimal, ExchangeConnector, ExchangeId, Instrument, MarketUpdate};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Base units per contract, keyed by canonical base asset, for one venue.
/// Sourced from universe discovery; empty means "this venue quotes base units".
pub type ContractSizes = HashMap<String, Decimal>;

/// Adapts a [`WsExchange`] into an [`ExchangeConnector`] by running it through
/// the shared reconnecting WS driver.
pub struct WsConnector<E: WsExchange> {
    inner: E,
    backoff: Backoff,
    contract_sizes: ContractSizes,
}

impl<E: WsExchange> WsConnector<E> {
    pub fn new(inner: E, backoff: Backoff) -> Self {
        WsConnector::with_contract_sizes(inner, backoff, ContractSizes::new())
    }

    pub fn with_contract_sizes(
        inner: E,
        backoff: Backoff,
        contract_sizes: ContractSizes,
    ) -> Self {
        WsConnector {
            inner,
            backoff,
            contract_sizes,
        }
    }
}

#[async_trait]
impl<E: WsExchange> ExchangeConnector for WsConnector<E> {
    fn id(&self) -> ExchangeId {
        self.inner.id()
    }

    async fn run(
        self: Arc<Self>,
        symbols: Vec<Instrument>,
        tx: mpsc::Sender<MarketUpdate>,
    ) -> anyhow::Result<()> {
        run_ws_exchange(
            &self.inner,
            symbols,
            tx,
            self.backoff.clone(),
            self.contract_sizes.clone(),
        )
        .await
    }
}

/// Build a live connector for the given exchange. `depth` is the desired book
/// depth; each connector snaps it to a channel the venue actually supports.
/// `sizes` converts venues that quote books in contracts into base units.
pub fn build_connector(
    id: ExchangeId,
    depth: usize,
    backoff: Backoff,
    sizes: ContractSizes,
) -> Arc<dyn ExchangeConnector> {
    macro_rules! wire {
        ($venue:expr) => {
            Arc::new(WsConnector::with_contract_sizes($venue, backoff, sizes))
        };
    }
    match id {
        ExchangeId::Bybit => wire!(bybit::Bybit::new(depth)),
        ExchangeId::Okx => wire!(okx::Okx::new(depth)),
        ExchangeId::Mexc => wire!(mexc::Mexc::new(depth)),
        ExchangeId::Bitget => wire!(bitget::Bitget::new(depth)),
        ExchangeId::Gate => wire!(gate::Gate::new(depth)),
        ExchangeId::Coinex => wire!(coinex::Coinex::new(depth)),
        ExchangeId::Kucoin => wire!(kucoin::Kucoin::new(depth)),
        ExchangeId::Phemex => wire!(phemex::Phemex::new(depth)),
    }
}
