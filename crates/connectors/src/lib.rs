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
use domain::{ExchangeConnector, ExchangeId, Instrument, MarketUpdate};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Adapts a [`WsExchange`] into an [`ExchangeConnector`] by running it through
/// the shared reconnecting WS driver.
pub struct WsConnector<E: WsExchange> {
    inner: E,
    backoff: Backoff,
}

impl<E: WsExchange> WsConnector<E> {
    pub fn new(inner: E, backoff: Backoff) -> Self {
        WsConnector { inner, backoff }
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
        run_ws_exchange(&self.inner, symbols, tx, self.backoff.clone()).await
    }
}

/// Build a live connector for the given exchange. `depth` is the desired book
/// depth; each connector snaps it to a channel the venue actually supports.
pub fn build_connector(
    id: ExchangeId,
    depth: usize,
    backoff: Backoff,
) -> Arc<dyn ExchangeConnector> {
    match id {
        ExchangeId::Bybit => Arc::new(WsConnector::new(bybit::Bybit::new(depth), backoff)),
        ExchangeId::Okx => Arc::new(WsConnector::new(okx::Okx::new(depth), backoff)),
        ExchangeId::Mexc => Arc::new(WsConnector::new(mexc::Mexc::new(depth), backoff)),
        ExchangeId::Bitget => Arc::new(WsConnector::new(bitget::Bitget::new(depth), backoff)),
        ExchangeId::Gate => Arc::new(WsConnector::new(gate::Gate::new(depth), backoff)),
        ExchangeId::Coinex => Arc::new(WsConnector::new(coinex::Coinex::new(depth), backoff)),
        ExchangeId::Kucoin => Arc::new(WsConnector::new(kucoin::Kucoin::new(depth), backoff)),
        ExchangeId::Phemex => Arc::new(WsConnector::new(phemex::Phemex::new(depth), backoff)),
    }
}
