//! Shared WebSocket driver: connect → subscribe → read loop → normalize, plus
//! reconnect with exponential backoff + jitter and app-level keepalive pings.
//!
//! Each exchange implements the thin [`WsExchange`] trait; [`run_ws_exchange`]
//! provides the whole connection lifecycle so per-exchange files only describe
//! *what* to subscribe to and *how* to parse — never the reconnect plumbing.

use domain::{Decimal, ExchangeId, Instrument, MarketUpdate};
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// Maps an exchange-native symbol string back to the canonical instrument.
/// Built once per connection from the subscribed set, so parsing never has to
/// guess where the base/quote split lands.
pub struct SymbolCtx {
    by_exch: HashMap<String, Instrument>,
    /// Canonical base → base units per contract on this venue.
    contract_sizes: HashMap<String, Decimal>,
}

impl SymbolCtx {
    pub fn lookup(&self, exch_symbol: &str) -> Option<&Instrument> {
        self.by_exch.get(exch_symbol)
    }

    /// Base units represented by one contract of `inst` on this venue. `1` when
    /// the venue quotes book sizes in base units or the size is unknown.
    pub fn contract_size(&self, inst: &Instrument) -> Decimal {
        self.contract_sizes
            .get(&inst.base)
            .copied()
            .unwrap_or(Decimal::ONE)
    }
}

/// Convert a parsed book's level quantities from contracts into base units.
///
/// Applied centrally rather than in each connector: the per-venue `parse`
/// implementations describe the wire format, and whether that format counts
/// coins or contracts is a property of the venue's contract spec, which
/// discovery already knows. A multiplier of 1 leaves the book untouched.
fn to_base_units(update: &mut MarketUpdate, ctx: &SymbolCtx) {
    let MarketUpdate::Book { instrument, book, .. } = update else {
        return;
    };
    let mult = ctx.contract_size(instrument);
    if mult == Decimal::ONE {
        return;
    }
    for lvl in book.bids.iter_mut().chain(book.asks.iter_mut()) {
        lvl.qty *= mult;
    }
}

/// Backoff schedule for reconnects. `next()` grows geometrically up to `max`
/// with additive jitter; `reset()` returns to `initial` after a stable session.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    jitter: Duration,
    current: Duration,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration, jitter: Duration) -> Self {
        Backoff {
            initial,
            max,
            jitter,
            current: initial,
        }
    }

    pub fn reset(&mut self) {
        self.current = self.initial;
    }

    pub fn next_delay(&mut self) -> Duration {
        let jitter_ms = if self.jitter.is_zero() {
            0
        } else {
            rand::thread_rng().gen_range(0..=self.jitter.as_millis() as u64)
        };
        let delay = self.current + Duration::from_millis(jitter_ms);
        // Grow for next time, capped at max.
        self.current = (self.current * 2).min(self.max);
        delay.min(self.max + self.jitter)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff::new(
            Duration::from_millis(500),
            Duration::from_secs(30),
            Duration::from_millis(400),
        )
    }
}

/// The per-exchange contract. Most methods are synchronous and cheap; all the
/// async lifecycle lives in [`run_ws_exchange`]. The one async hook,
/// [`WsExchange::resolve_ws_url`], exists for venues (e.g. KuCoin) that must
/// fetch a signed WS endpoint/token over REST before connecting.
#[async_trait::async_trait]
pub trait WsExchange: Send + Sync + 'static {
    fn id(&self) -> ExchangeId;

    /// Static WebSocket endpoint for perp public streams. Venues needing a
    /// dynamic endpoint override [`resolve_ws_url`] instead.
    fn ws_url(&self) -> String;

    /// Resolve the endpoint to connect to. Default returns [`ws_url`]; override
    /// for token-bootstrapped venues. Called before every (re)connect.
    async fn resolve_ws_url(&self) -> anyhow::Result<String> {
        Ok(self.ws_url())
    }

    /// Canonical instrument → exchange-native symbol (e.g. BTC/USDT perp → "BTCUSDT").
    fn to_symbol(&self, inst: &Instrument) -> String;

    /// Text frames to send right after connect to subscribe to `symbols`
    /// (book/BBO + funding). Multiple frames allowed (some venues cap batch size).
    fn subscribe_frames(&self, symbols: &[Instrument]) -> Vec<String>;

    /// Parse one inbound text frame into zero or more normalized updates.
    /// Unrecognized frames (acks, heartbeats) should return an empty vec.
    fn parse(&self, text: &str, ctx: &SymbolCtx) -> Vec<MarketUpdate>;

    /// Called once right after each (re)connect, before subscribing. Stateful
    /// connectors (those maintaining L2 delta books) reset their book state here
    /// so a stale pre-reconnect book is never mixed with a fresh snapshot.
    fn on_reconnect(&self) {}

    /// Optional application-level keepalive frame (e.g. `{"op":"ping"}`).
    fn ping_frame(&self) -> Option<String> {
        None
    }

    /// How often to send the keepalive frame.
    fn ping_interval(&self) -> Duration {
        Duration::from_secs(20)
    }
}

/// A connection is considered "stable" (worth resetting backoff) once it has
/// stayed up this long.
const STABLE_AFTER: Duration = Duration::from_secs(30);

/// Drive an exchange forever: connect, subscribe, read, reconnect. Returns only
/// when `tx` is closed (server shutdown).
pub async fn run_ws_exchange<E: WsExchange>(
    exchange: &E,
    symbols: Vec<Instrument>,
    tx: mpsc::Sender<MarketUpdate>,
    mut backoff: Backoff,
    contract_sizes: HashMap<String, Decimal>,
) -> anyhow::Result<()> {
    let ctx = SymbolCtx {
        by_exch: symbols
            .iter()
            .map(|i| (exchange.to_symbol(i), i.clone()))
            .collect(),
        contract_sizes,
    };
    let id = exchange.id();

    loop {
        if tx.is_closed() {
            info!(exchange = %id, "tx closed, stopping connector");
            return Ok(());
        }

        match connect_once(exchange, &symbols, &ctx, &tx).await {
            Ok(session_len) => {
                if session_len >= STABLE_AFTER {
                    backoff.reset();
                }
                warn!(exchange = %id, "connection closed, reconnecting");
            }
            Err(e) => {
                warn!(exchange = %id, error = %e, "connection error, reconnecting");
            }
        }

        if tx.is_closed() {
            return Ok(());
        }
        let delay = backoff.next_delay();
        debug!(exchange = %id, ?delay, "backing off before reconnect");
        tokio::select! {
            _ = time::sleep(delay) => {}
            _ = tx.closed() => return Ok(()),
        }
    }
}

/// A single connect→subscribe→read session. Returns how long the session lasted
/// (so the caller can decide whether to reset backoff), or an error.
async fn connect_once<E: WsExchange>(
    exchange: &E,
    symbols: &[Instrument],
    ctx: &SymbolCtx,
    tx: &mpsc::Sender<MarketUpdate>,
) -> anyhow::Result<Duration> {
    let id = exchange.id();
    let url = exchange.resolve_ws_url().await?;
    let started = Instant::now();

    let (ws, _resp) = tokio_tungstenite::connect_async(&url).await?;
    info!(exchange = %id, %url, "connected");
    exchange.on_reconnect(); // reset any stateful book before (re)subscribing
    let (mut write, mut read) = ws.split();

    // Subscribe (resubscribe on every reconnect — the trait rebuilds frames).
    for frame in exchange.subscribe_frames(symbols) {
        write.send(Message::Text(frame)).await?;
    }

    let mut ping_timer = time::interval(exchange.ping_interval());
    ping_timer.tick().await; // consume immediate first tick

    loop {
        tokio::select! {
            biased;

            _ = tx.closed() => {
                let _ = write.send(Message::Close(None)).await;
                return Ok(started.elapsed());
            }

            _ = ping_timer.tick() => {
                if let Some(p) = exchange.ping_frame() {
                    if write.send(Message::Text(p)).await.is_err() {
                        return Ok(started.elapsed());
                    }
                }
            }

            msg = read.next() => {
                let Some(msg) = msg else {
                    return Ok(started.elapsed()); // stream ended
                };
                match msg? {
                    Message::Text(txt) => {
                        for mut upd in exchange.parse(&txt, ctx) {
                            to_base_units(&mut upd, ctx);
                            if tx.send(upd).await.is_err() {
                                return Ok(started.elapsed()); // consumer gone
                            }
                        }
                    }
                    Message::Binary(bin) => {
                        // Some venues send text-as-binary; try UTF-8, ignore otherwise.
                        if let Ok(txt) = std::str::from_utf8(&bin) {
                            for mut upd in exchange.parse(txt, ctx) {
                                to_base_units(&mut upd, ctx);
                                if tx.send(upd).await.is_err() {
                                    return Ok(started.elapsed());
                                }
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        let _ = write.send(Message::Pong(payload)).await;
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => return Ok(started.elapsed()),
                    Message::Frame(_) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{BookLevel, TopBook};
    use std::time::Instant as StdInstant;

    fn ctx_with(base: &str, size: Decimal) -> SymbolCtx {
        SymbolCtx {
            by_exch: HashMap::new(),
            contract_sizes: HashMap::from([(base.to_string(), size)]),
        }
    }

    fn book_update(inst: Instrument, qty: Decimal) -> MarketUpdate {
        MarketUpdate::Book {
            exchange: ExchangeId::Gate,
            instrument: inst,
            book: TopBook {
                bids: vec![BookLevel::new(Decimal::from(10), qty)],
                asks: vec![BookLevel::new(Decimal::from(11), qty)],
                recv_ts: StdInstant::now(),
                exch_ts: None,
            },
        }
    }

    fn qtys(update: &MarketUpdate) -> (Decimal, Decimal) {
        let MarketUpdate::Book { book, .. } = update else {
            panic!("expected a book update");
        };
        (book.bids[0].qty, book.asks[0].qty)
    }

    #[test]
    fn contract_sizes_convert_book_levels_to_base_units() {
        // Gate quotes 3 contracts; one contract is 1000 PEPE => 3000 PEPE.
        let inst = Instrument::perp("PEPE", "USDT");
        let ctx = ctx_with("PEPE", Decimal::from(1000));
        let mut upd = book_update(inst, Decimal::from(3));
        to_base_units(&mut upd, &ctx);
        assert_eq!(qtys(&upd), (Decimal::from(3000), Decimal::from(3000)));
    }

    #[test]
    fn unknown_or_unit_contract_size_leaves_the_book_alone() {
        let inst = Instrument::perp("SOL", "USDT");
        // No entry for SOL at all.
        let ctx = ctx_with("PEPE", Decimal::from(1000));
        let mut upd = book_update(inst.clone(), Decimal::from(3));
        to_base_units(&mut upd, &ctx);
        assert_eq!(qtys(&upd), (Decimal::from(3), Decimal::from(3)));

        // Explicit multiplier of 1.
        let ctx = ctx_with("SOL", Decimal::ONE);
        let mut upd = book_update(inst, Decimal::from(3));
        to_base_units(&mut upd, &ctx);
        assert_eq!(qtys(&upd), (Decimal::from(3), Decimal::from(3)));
    }

    #[test]
    fn non_book_updates_are_untouched() {
        let ctx = ctx_with("PEPE", Decimal::from(1000));
        let mut upd = MarketUpdate::Ticker {
            exchange: ExchangeId::Gate,
            instrument: Instrument::perp("PEPE", "USDT"),
            quote_volume_24h: Some(Decimal::from(5)),
            open_interest: None,
        };
        to_base_units(&mut upd, &ctx);
        let MarketUpdate::Ticker { quote_volume_24h, .. } = upd else {
            panic!("expected a ticker update");
        };
        assert_eq!(quote_volume_24h, Some(Decimal::from(5)));
    }

    #[test]
    fn backoff_grows_then_caps() {
        let mut b = Backoff::new(
            Duration::from_millis(100),
            Duration::from_millis(800),
            Duration::ZERO,
        );
        assert_eq!(b.next_delay(), Duration::from_millis(100));
        assert_eq!(b.next_delay(), Duration::from_millis(200));
        assert_eq!(b.next_delay(), Duration::from_millis(400));
        assert_eq!(b.next_delay(), Duration::from_millis(800));
        assert_eq!(b.next_delay(), Duration::from_millis(800)); // capped
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
    }
}
