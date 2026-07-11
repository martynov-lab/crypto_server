# Crypto Perp Arbitrage Screener (Phase 1)

A Rust server that ingests **perpetual** market data from eight exchanges, keeps
live top-of-book state in memory, computes the best *executable* cross-exchange
spread per instrument (net of taker fees), and pushes signals to clients over
WebSocket. Public data only — **no exchange keys**, no execution (see
[`docs/ARB_SCREENER_SPEC.md`](docs/ARB_SCREENER_SPEC.md) for the full spec and
phase roadmap).

Exchanges: **bybit, okx, mexc, bitget, gate, coinex, kucoin, phemex**.

## Why "executable" spread matters

On mid-tier venues, 2–20% spreads on thin alt perps are common but mostly
*mirages*: a 15% best-bid/best-ask gap backed by $50 of depth, a frozen side, or
a market you can't rebalance margin across. The screener separates real from
mirage by:

- walking the book to a **target size Q** and taking the VWAP actually
  achievable, not the BBO;
- subtracting **taker fees on both legs**;
- **capping the band at 20%** (ghost/delisted territory);
- dropping **stale** books by local monotonic receive time;
- checking **settlement-asset transferability** between venues (margin rebalance
  feasibility) where public data allows.

See the parameter reference in
[`crates/screener/src/config.rs`](crates/screener/src/config.rs) — `ClientConfig`
is exactly what a client sends over WS.

## Workspace layout

```text
crates/
  domain/          canonical types + traits (no exchange deps)
  connectors/      8 exchanges + shared WS driver + mock (common.rs, book.rs)
  ingest/          connector supervision + update routing
  market_state/    in-memory books/funding + staleness-aware snapshots
  screener/        executable VWAP, filters, hysteresis, funding diff (+ tests)
  transfer_status/ public currency-endpoint poller (deposit/withdraw + networks)
  api/             axum WS hub (signal push) + REST (health/metrics/summary)
  auth/            client auth stub (open / static token) — NOT exchange keys
  persistence/     optional CSV sink for signal lifetime analysis
bin/server/        wiring + config + graceful shutdown
config/default.toml
docs/EXCHANGE_ENDPOINTS.md
```

## Build & run

> Requires a Rust toolchain (`rustup`, stable ≥ 1.75). It was **not** installed on
> the authoring machine, so the code is written but not yet compiled — expect to
> fix small issues on first `cargo build`, especially in the six non-reference
> connectors (see `docs/EXCHANGE_ENDPOINTS.md`).

```bash
cargo test            # offline unit + pipeline tests (no network)
cargo run -p server   # starts REST + WS on 127.0.0.1:8080
```

Config is `config/default.toml`, overridable via env (`ARB__SERVER__BIND=...`,
`ARB__INGEST__SYMBOLS=...`). Point `ARB_CONFIG` at a different base file to swap
configs.

## API

- `GET  /healthz` — liveness + instrument count
- `GET  /metrics` — Prometheus text
- `GET  /summary` — current best net spread per instrument (default config)
- `POST /config/validate` — validate a `ClientConfig` body
- `GET  /ws` — WebSocket:
  1. client → `{"type":"subscribe","config":{...ClientConfig...}}` (config optional)
  2. server → `{"type":"subscribed","config":{...}}`
  3. server → `{"type":"event","spread":{...},"funding":{...},"ts_ms":...}` stream

Backpressure: each session has its own screening engine reading shared state; a
slow client only slows its own send loop (the fan-out tolerates lag), never the
core.

## Beyond the base screener

- **Universe auto-discovery.** On startup (and every 15 min) the server polls each
  venue's public contracts endpoint and screens every base listed on ≥ N venues
  (`config/default.toml → [ingest] auto_discover`), instead of a hardcoded list —
  ~800 arb-relevant coins live, where the real 2–20% spreads are. The catalog is
  exposed via `GET /instruments` and pushed to WS clients as a `universe` message.
- **Spread dynamics.** Per-coin rolling statistics (baseline median, spike
  z-score, above-threshold episode duration) filter out *persistently wide*
  spreads (structural traps) and keep the healthy "tight baseline, brief spike"
  pattern. Each signal carries a `dynamics` block and a 0–100 `quality_score`.
- **Liquidity floors.** `min_24h_quote_volume` / `min_open_interest` are enforced
  from ticker data (currently Bybit; other venues report volume in base/contract
  units and are a follow-up).

## Status / caveats

- **Bybit & OKX** connectors are the reference implementations. The other six are
  written to the same pattern but their subscribe payloads / field names need a
  live smoke test — see `docs/EXCHANGE_ENDPOINTS.md`. (All eight *discovery*
  fetchers are live-verified.)
- Transfer-status uses **public** endpoints only (Gate + KuCoin parsed; Bitget /
  CoinEx / Phemex are stubs; Bybit / OKX / MEXC need read-only keys in a later
  phase). Transfer filters default to off so the demo still produces signals.
- Volume/OI ingestion is currently Bybit-only (clean quote-volume + OI fields);
  other venues report volume in base/contract units and need conversion.
