//! Offline end-to-end: mock connector → market_state → screener engine.
//! No network; deterministic. Mirrors the runtime data path.

use connectors::mock::MockConnector;
use domain::{
    BookLevel, Decimal, ExchangeConnector, ExchangeId, Instrument, MarketUpdate, TopBook,
};
use market_state::MarketState;
use rust_decimal_macros::dec;
use screener::{ClientConfig, NoTransferInfo, ScreenerEngine};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

fn book(bid: Decimal, ask: Decimal, qty: Decimal) -> TopBook {
    TopBook {
        bids: vec![BookLevel::new(bid, qty)],
        asks: vec![BookLevel::new(ask, qty)],
        recv_ts: Instant::now(),
        exch_ts: None,
    }
}

fn cfg() -> ClientConfig {
    let mut c = ClientConfig::default();
    c.require_transferable = false;
    c.require_common_network = false;
    c.min_signal_lifetime_ms = 0;
    c.cooldown_ms = 0;
    c
}

#[test]
fn pipeline_emits_expected_spread() {
    let inst = Instrument::perp("XYZ", "USDT");
    let market = MarketState::new(Duration::from_secs(5));

    // Bybit cheap (buy), OKX rich (sell): buy ~100, sell ~106 => ~6% gross.
    market.apply(MarketUpdate::Book {
        exchange: ExchangeId::Bybit,
        instrument: inst.clone(),
        book: book(dec!(99.9), dec!(100), dec!(1000)),
    });
    market.apply(MarketUpdate::Book {
        exchange: ExchangeId::Okx,
        instrument: inst.clone(),
        book: book(dec!(106), dec!(106.1), dec!(1000)),
    });

    let engine = ScreenerEngine::new(cfg());
    let snap = market.snapshot(&inst, Instant::now());
    assert!(snap.has_pairing());

    let ev = engine
        .on_instrument(&snap, &NoTransferInfo, Instant::now(), 0)
        .expect("expected a signal");
    assert_eq!(ev.spread.buy_exchange, ExchangeId::Bybit);
    assert_eq!(ev.spread.sell_exchange, ExchangeId::Okx);
    assert!(ev.spread.net_pct > dec!(0.05));
    assert!(ev.spread.net_pct < dec!(0.20));
}

#[test]
fn stale_book_produces_no_pairing() {
    let inst = Instrument::perp("XYZ", "USDT");
    let market = MarketState::new(Duration::from_millis(0)); // everything is stale
    market.apply(MarketUpdate::Book {
        exchange: ExchangeId::Bybit,
        instrument: inst.clone(),
        book: book(dec!(99.9), dec!(100), dec!(1000)),
    });
    market.apply(MarketUpdate::Book {
        exchange: ExchangeId::Okx,
        instrument: inst.clone(),
        book: book(dec!(106), dec!(106.1), dec!(1000)),
    });
    // now strictly after recv_ts => age > 0 => stale.
    let snap = market.snapshot(&inst, Instant::now() + Duration::from_millis(1));
    assert!(!snap.has_pairing());
}

#[tokio::test]
async fn mock_connector_streams_updates() {
    let inst = Instrument::perp("XYZ", "USDT");
    let inst2 = inst.clone();
    let gen = Arc::new(move || {
        vec![MarketUpdate::Book {
            exchange: ExchangeId::Bybit,
            instrument: inst2.clone(),
            book: book(dec!(99.9), dec!(100), dec!(10)),
        }]
    });
    let mock = Arc::new(
        MockConnector::new(ExchangeId::Bybit, Duration::from_millis(5), gen).with_max_ticks(3),
    );
    let (tx, mut rx) = mpsc::channel(16);
    let handle = tokio::spawn(mock.run(vec![inst.clone()], tx));

    let mut count = 0;
    while let Some(update) = rx.recv().await {
        assert_eq!(update.exchange(), ExchangeId::Bybit);
        count += 1;
    }
    handle.await.unwrap().unwrap();
    assert_eq!(count, 3);
}
