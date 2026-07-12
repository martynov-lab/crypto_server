# Exchange endpoints (Phase 1, perp public data)

Reference for every WS/REST endpoint the connectors use. **These are compiled
from public docs but were NOT verified against live endpoints in this build** (no
Rust toolchain on the authoring machine). Treat the ⚠️ rows as "confirm before
production". Bybit and OKX are the reference implementations and the most likely
to be correct as written; the other six follow the same shape and need a live
smoke test to confirm subscribe payloads and message field names.

## Market data (WebSocket)

| Exchange | WS URL | Book channel | Funding source | Book model | Symbol |
|---|---|---|---|---|---|
| Bybit  | `wss://stream.bybit.com/v5/public/linear` | `orderbook.{depth}.{SYM}` | `tickers.{SYM}` | snapshot+delta (`DeltaBook`) | `BTCUSDT` |
| OKX    | `wss://ws.okx.com:8443/ws/v5/public` | `books5` | `funding-rate` | full snapshot | `BTC-USDT-SWAP` |
| MEXC ⚠️ | `wss://contract.mexc.com/edge` | `sub.depth.full`→`push.depth.full` | `push.ticker` | full snapshot | `BTC_USDT` |
| Bitget ⚠️ | `wss://ws.bitget.com/v2/ws/public` | `books5` (instType `USDT-FUTURES`) | `ticker` | full snapshot | `BTCUSDT` |
| Gate ⚠️ | `wss://fx-ws.gateio.ws/v4/ws/usdt` | `futures.book_ticker` (BBO only) | `futures.tickers` | 1-level | `BTC_USDT` |
| CoinEx ⚠️ | `wss://socket.coinex.com/v2/futures` | `depth.subscribe`→`depth.update` | — (not subscribed) | snap/delta (`DeltaBook`) | `BTCUSDT` |
| KuCoin ⚠️ | bootstrapped (see below) | `/contractMarket/level2Depth5:{SYM}` | `/contract/instrument:{SYM}` | full snapshot | `XBTUSDTM` (BTC→XBT) |
| Phemex ⚠️ | `wss://ws.phemex.com` | `orderbook_p.subscribe` | — (not subscribed) | snap/delta (`DeltaBook`) | `BTCUSDT` |

Keepalive frames: Bybit `{"op":"ping"}`; OKX/Bitget literal `ping`; MEXC
`{"method":"ping"}`; Gate `{"channel":"futures.ping"}`; CoinEx
`{"method":"server.ping",...}`; KuCoin `{"type":"ping",...}`; Phemex
`{"method":"server.ping",...}`.

### KuCoin bootstrap

`POST https://api-futures.kucoin.com/api/v1/bullet-public` →
`{ data: { token, instanceServers:[{ endpoint }] } }`; connect to
`{endpoint}?token={token}&connectId=arb-screener`. Implemented in
`Kucoin::resolve_ws_url`.

### Known imprecisions

- **Gate** uses `book_ticker` (best bid/ask only); sizes are in *contracts*, so
  executable notional is approximate. For real depth switch to
  `futures.order_book`/`futures.order_book_update` (snapshot+delta) and apply the
  contract multiplier.
- **Phemex** USDT (`*_p`) channels use real numbers; legacy inverse contracts use
  scaled integers — verify the scale for your symbols.
- **Funding** interval is hard-coded to 8h for annualization; some symbols use
  4h/1h. Read the per-symbol interval from the ticker where available.

## Transfer status (REST, PUBLIC only — no keys in Phase 1)

| Exchange | Endpoint | Status |
|---|---|---|
| Gate   | `GET https://api.gateio.ws/api/v4/spot/currencies` | implemented |
| KuCoin | `GET https://api.kucoin.com/api/v3/currencies` | implemented |
| Bitget | `GET https://api.bitget.com/api/v2/spot/public/coins` | ⚠️ parser TODO |
| CoinEx | `GET https://api.coinex.com/v2/assets/all-deposit-withdraw-config` | ⚠️ parser TODO |
| Phemex | `GET https://api.phemex.com/public/cfg/v2/products` | ⚠️ parser TODO |
| Bybit / OKX / MEXC | authenticated only | not available in Phase 1 |

When a venue's transfer status is unknown, the `require_transferable` /
`require_common_network` filters **fail closed** for that venue. That is why they
default to `false` in `config/default.toml` — otherwise Bybit/OKX pairs (whose
transfer data needs keys) could never signal.
