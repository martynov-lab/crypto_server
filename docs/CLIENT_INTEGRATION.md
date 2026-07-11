# Client integration guide (Phase 1)

What the **client app** must implement to talk to the arbitrage screener server.
Server is Rust/axum; transport is WebSocket for the signal stream plus a few REST
endpoints. Everything here is derived from the running server
(`crates/api/src/session.rs`, `crates/screener/src/config.rs`).

## 0. Wire-format rules (read first)

- **All monetary/ratio numbers are JSON strings, not numbers.** Decimals
  serialize as strings to avoid float precision loss, e.g. `"net_pct":"0.0345"`.
  The client MUST parse them with a decimal/bignum type (e.g. `BigDecimal`,
  `decimal.js`, `Decimal` in Dart/Kotlin) — never `parseFloat` for money.
- **Enums are lowercase strings.** Exchange ids: `bybit, okx, mexc, bitget, gate,
  coinex, kucoin, phemex`. Market kind: `perp`.
- **Percent fields are fractions**, not percentages: `net_pct = "0.03"` means 3%.
- Message envelopes are internally tagged by a `"type"` field.

## 1. Endpoints

| Purpose | Method | Path |
|---|---|---|
| Signal stream | WS | `ws://<host>:8080/ws` |
| Liveness + instrument count | GET | `/healthz` |
| Prometheus metrics | GET | `/metrics` |
| Current best spread per instrument | GET | `/summary` |
| Traded-instrument catalog (coins × venues) | GET | `/instruments` |
| Validate a config without subscribing | POST | `/config/validate` |

Local default host: `127.0.0.1:8080` (see `config/default.toml`).

## 2. WebSocket protocol

### 2.1 Handshake

On connect the client sends exactly one `subscribe` message. `token` and
`config` are both optional; omit `config` to use the server defaults.

Client → server:
```json
{ "type": "subscribe", "token": null, "config": { "min_net_spread_pct": "0.03", "max_net_spread_pct": "0.15" } }
```

Server → client (acknowledgement, echoes the *effective* config with all defaults filled in):
```json
{ "type": "subscribed", "config": { "exchanges": ["bybit","okx", ...], "quote": "USDT", ... } }
```

Immediately after `subscribed`, the server pushes the **traded-instrument
catalog** once (which coins have a USDT perp on which venues, ≥2 venues,
most-covered first):
```json
{ "type": "universe", "instruments": [
  { "base": "BTC", "quote": "USDT", "exchanges": ["bybit","okx","mexc","bitget","gate","coinex","kucoin","phemex"], "coverage": 8 },
  { "base": "QNT", "quote": "USDT", "exchanges": ["bybit","okx","mexc","gate"], "coverage": 4 }
] }
```
This message can be large (hundreds of rows) and may arrive as multiple WS
frames — a normal WS client reassembles them automatically. The full catalog
(including single-venue coins) is also available via `GET /instruments`.

If auth fails or the config is invalid, the server replies with an `error` and
(for auth) closes:
```json
{ "type": "error", "message": "unauthorized" }
```

### 2.2 Signal stream

After `subscribed`, the server pushes an `event` whenever a fresh, filter-passing,
non-duplicate signal appears for a screened instrument:
```json
{
  "type": "event",
  "spread": {
    "instrument": { "base": "ARB", "quote": "USDT", "kind": "perp" },
    "buy_exchange": "mexc",
    "sell_exchange": "kucoin",
    "vwap_buy": "1.2340",
    "vwap_sell": "1.2712",
    "gross_pct": "0.0301",
    "net_pct": "0.0289",
    "executable_notional": "2000",
    "capped_by_depth": false
  },
  "funding": {
    "long_exchange": "okx",
    "short_exchange": "bybit",
    "diff_apr": "0.1832"
  },
  "dynamics": {
    "baseline_pct": "0.0031",
    "stddev_pct": "0.0090",
    "current_pct": "0.0289",
    "z_score": "3.41",
    "sample_count": 120,
    "episode_ms": 1400
  },
  "quality_score": "66.2",
  "ts_ms": 1752230400000
}
```
`funding`, `dynamics`, and `quality_score` are **omitted** when unavailable.
`buy_exchange` is where you buy (lowest ask); `sell_exchange` is where you sell
(highest bid). `net_pct` is already net of taker fees on both legs.

**Spread dynamics** describe the coin's behavior (shared, computed with default
fees), and are the key "real vs mirage" signal:
- `baseline_pct` — median spread over the rolling window. A *tight* baseline
  (near 0–1%) with a large `current_pct` is the healthy, capturable pattern.
- `z_score` — how many stddevs the current spread is above its own mean; a high
  z is a genuine spike, not "it's always wide".
- `episode_ms` — how long the spread has stayed above the reference threshold. A
  large value means it's *not* reverting — likely a structural trap.
- `quality_score` (0–100) — combines tight baseline + strong spike + short
  episode + broad venue coverage. Sort/alert by this to surface the best coins.

### 2.3 Keepalive

- Client may send `{ "type": "ping" }`; server replies `{ "type": "pong" }`.
- The server also answers native WS ping frames. Send an app-level ping every
  ~20–30 s to keep intermediaries from dropping the socket.

### 2.4 Re-subscribe / reconfigure

Send another `subscribe` at any time to change filters. The server rebuilds the
session's screening engine (hysteresis/lifetime state resets) and returns a fresh
`subscribed` ack.

### 2.5 Reconnect

On socket close, reconnect with exponential backoff + jitter and re-send
`subscribe`. Treat the stream as at-least-once but lossy under load: the server
coalesces to the latest state per instrument when a client lags, so the client
should render the newest event per instrument and not assume every tick arrives.

## 3. `ClientConfig` reference (all fields optional on subscribe)

| Field | Type | Default | Meaning |
|---|---|---|---|
| `exchanges` | string[] | all 8 | Venues to include |
| `quote` | string | `"USDT"` | Settlement asset |
| `allow_symbols` | string[] | `[]` (all) | Base-asset allow list |
| `deny_symbols` | string[] | `[]` | Base-asset deny list |
| `min_24h_quote_volume` | decimal-str | `"0"` | Volume floor (not yet enforced) |
| `min_open_interest` | decimal-str? | `null` | OI floor (not yet enforced) |
| `min_net_spread_pct` | decimal-str | `"0.02"` | Lower band = 2% |
| `max_net_spread_pct` | decimal-str | `"0.20"` | Upper band = 20% (ghost cap) |
| `target_notional_q` | decimal-str | `"2000"` | USDT size the VWAP spread is measured at |
| `min_executable_notional` | decimal-str | `"500"` | Required real depth on both legs |
| `depth_levels_n` | int | `20` | Book levels walked for VWAP |
| `taker_fee` | map<exch,decimal-str> | per-venue | Taker fee fractions |
| `include_funding_diff` | bool | `true` | Emit funding differential |
| `min_funding_diff_apr` | decimal-str | `"0.15"` | Min annualized funding diff |
| `funding_hold_hours` | decimal-str | `"8"` | Assumed hold for funding calc |
| `require_transferable` | bool | `false` | Settlement asset transferable both legs |
| `require_common_network` | bool | `false` | Shared enabled chain both legs |
| `max_book_age_ms` | int | `3000` | Staleness cutoff |
| `enable_dynamics` | bool | `true` | Master switch for the spread-dynamics filters |
| `max_baseline_spread_pct` | decimal-str | `"0.01"` | Reject persistently-wide coins (baseline above this) |
| `min_spike_z` | decimal-str | `"3"` | Require current spread ≥ this many stddevs above its mean |
| `max_spread_duration_ms` | int | `300000` | Reject spreads that stay wide longer than this |
| `min_dynamics_samples` | int | `20` | Warmup before dynamics filters apply |
| `hysteresis_step_pct` | decimal-str | `"0.005"` | Re-alert only if spread widens this much |
| `min_signal_lifetime_ms` | int | `1500` | Suppress until opportunity persists this long |
| `cooldown_ms` | int | `2000` | Min gap between emits per instrument |
| `max_signals_per_min` | int? | `120` | Global per-session rate cap |

Validate a config before using it: `POST /config/validate` with the JSON body →
`{ "valid": true }` or `{ "valid": false, "error": "..." }`.

## 4. `/summary` (REST) shape

Array, highest net spread first:
```json
[ { "instrument": "BTC/USDT", "buy_exchange": "mexc", "sell_exchange": "kucoin",
    "net_pct": "-0.0004", "coverage": 7 } ]
```
`coverage` = number of venues with a usable book. Use this for a dashboard/table
without holding a WS open.

## 5. Client TODO checklist

- [ ] WS client with auto-reconnect (exp backoff + jitter) and app-level ping.
- [ ] Send `subscribe` on connect and on every filter change; handle `subscribed`,
      `universe`, `event`, `pong`, `error`.
- [ ] Store the `universe` catalog (or fetch `GET /instruments`) to show which
      coins trade on which venues; let the user browse/pick from it.
- [ ] Sort/highlight events by `quality_score`, and show the `dynamics` block
      (baseline vs current, z-score, episode age) so the user can see *why* a
      coin is a good arb candidate.
- [ ] **Decimal-safe parsing** for every price/ratio field (no floats for money).
- [ ] A config/filters UI mapping 1:1 to `ClientConfig` — the 2–20% band,
      `target_notional_q`, and (later) volume/OI filters are the key knobs.
- [ ] Render events as a live table keyed by instrument (newest wins), showing
      buy/sell venue, `net_pct`, `executable_notional`, `capped_by_depth`, and the
      funding signal when present.
- [ ] Optional: poll `/summary` for a cold-start snapshot before the first event.
- [ ] Optional: local notifications (push/sound) when `net_pct` crosses a
      user threshold — the server already dedups via hysteresis, so alert on
      every `event`.
- [ ] Surface `capped_by_depth = true` prominently — it means the book couldn't
      supply the full `target_notional_q` (thinner, riskier opportunity).

## 6. Gotchas

- Signals are already de-duplicated server-side (hysteresis + cooldown); the
  client should NOT add its own aggressive dedup or it will hide re-widenings.
- `require_transferable`/`require_common_network` default off in Phase 1 (public
  transfer data is partial). Leave them off unless the operator has populated the
  transfer store for the relevant venues.
- Only `perp` markets exist in Phase 1. `kind` is always `"perp"`.
