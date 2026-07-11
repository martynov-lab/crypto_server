# Server spec: real-time per-instrument spread chart (Phase 2 request)

Client-side goal: tapping a coin in **Screener → Сигналы** opens a detail view
with a **live spread chart** for that instrument. The chart must be **real-time**
(streaming, not a one-off snapshot), bounded to a rolling time window, and dense
enough that a trader can eyeball the **best entry** (spread widens *and* is
executable) and the **best exit** (spread reverts toward its baseline).

This extends `CLIENT_INTEGRATION.md` — the same wire-format rules apply.

## 0. The core change the server must make

The existing `event` stream cannot drive a real-time chart:

- It is **deduplicated** (hysteresis + cooldown ≥ 2 s, min-lifetime) and coalesced
  under load — so it is sparse and event-driven, useless as a continuous line.
- It only exists **from subscribe time** — no back-history to fill the window.
- Each `event` carries only the **single best** `buy → sell` pair, not per venue.

So the server must expose a **second, raw stream**: the instrument's spread
**sampled at a fixed cadence**, computed continuously and **decoupled from the
alert engine** (no hysteresis / cooldown / lifetime filtering). The chart shows
the true shape of the spread; the alert `event`s remain the "notify me" layer.

Concretely, the server needs:

1. A **rolling raw-spread buffer per screened instrument** (≥ 2 venues), holding
   the last `window_ms` of samples at a fixed `resolution_ms` cadence.
2. An **on-demand per-instrument live subscription** ("watch") that (a) replays
   that buffer once to fill the window immediately, then (b) pushes each new
   sample in real time until the client stops watching.

## 1. Wire-format rules (restated)

- Monetary/ratio numbers are **JSON strings** (decimal-safe); percent fields are
  **fractions** (`0.03` = 3%). Timestamps are epoch **milliseconds** (`ts_ms`).
- Exchange ids are lowercase: `bybit, okx, mexc, bitget, gate, coinex, kucoin,
  phemex`. Messages are internally tagged by `"type"`.
- The watch runs over the **same WebSocket** the client already holds (`/ws`),
  independently of the `subscribe` filter set.

## 2. WebSocket additions (the MVP — this is what unblocks the feature)

### 2.1 Start watching an instrument

Client → server (sent when the user opens the coin's chart):

```json
{ "type": "watch",
  "instrument": { "base": "ARB", "quote": "USDT", "kind": "perp" },
  "window_ms": 900000,
  "resolution_ms": 1000 }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `instrument` | object | — (required) | `base`/`quote`/`kind` |
| `window_ms` | int | `900000` | Rolling window to backfill/keep (e.g. 15 min) |
| `resolution_ms` | int | server picks | Desired sample cadence; server may clamp |

### 2.2 Immediate backfill (fills the window on open)

Server → client, once, right after `watch` — replays the buffered window so the
chart is full instantly instead of drawing in from the right edge:

```json
{ "type": "watch_snapshot",
  "instrument": { "base": "ARB", "quote": "USDT", "kind": "perp" },
  "resolution_ms": 1000,
  "window_ms": 900000,
  "points": [
    { "ts_ms": 1752230400000, "net_pct": "0.0031", "baseline_pct": "0.0030",
      "buy_exchange": "mexc", "sell_exchange": "kucoin",
      "executable_notional": "2000", "capped_by_depth": false },
    { "ts_ms": 1752230401000, "net_pct": "0.0042", "baseline_pct": "0.0030",
      "buy_exchange": "mexc", "sell_exchange": "kucoin",
      "executable_notional": "2000", "capped_by_depth": false }
  ] }
```

### 2.3 Live ticks (the real-time part)

Server → client, one per sample at `resolution_ms`, until `unwatch`/disconnect:

```json
{ "type": "spread_tick",
  "instrument": { "base": "ARB", "quote": "USDT", "kind": "perp" },
  "point": { "ts_ms": 1752230402000, "net_pct": "0.0289", "baseline_pct": "0.0031",
             "buy_exchange": "mexc", "sell_exchange": "kucoin",
             "executable_notional": "2000", "capped_by_depth": false } }
```

The client appends each tick to the line and drops points older than `window_ms`.

### 2.4 Point fields

| Field | Type | Req. | Chart use |
| --- | --- | --- | --- |
| `ts_ms` | int | yes | X axis |
| `net_pct` | decimal-str | yes | **primary line** (net-of-default-fees spread) |
| `baseline_pct` | decimal-str | no | reference band (rolling median; same as `dynamics.baseline_pct`) |
| `gross_pct` | decimal-str | no | optional secondary line |
| `buy_exchange` / `sell_exchange` | string | no | which venues form the best spread *at that instant* (can change) |
| `executable_notional` | decimal-str | no | **entry quality**: real depth available on both legs right now |
| `capped_by_depth` | bool | no | `true` = book can't supply full size → spread is a thinner/mirage entry |

`executable_notional` + `capped_by_depth` are what let the user distinguish a
**real entry** (wide *and* executable) from a mirage. Include them if cheap — they
are the difference between "looks great" and "actually capturable".

### 2.5 Stop watching

```json
{ "type": "unwatch", "instrument": { "base": "ARB", "quote": "USDT", "kind": "perp" } }
```

Client sends this when the chart closes. Server must also auto-drop all watches
on socket close.

### 2.6 Errors

Instrument not covered (< 2 venues) or unknown → the server replies once and does
not start a stream:

```json
{ "type": "error", "message": "no live spread for ARB/USDT" }
```

## 3. Server responsibilities / constraints

- **Decouple from the alert engine.** The tick series is the raw sampled spread
  with **default/shared fees** (same basis as the `dynamics` block) — no
  hysteresis, cooldown, lifetime, or per-session filtering. It is therefore
  instrument-global and cacheable/shareable across sessions.
- **Cadence.** Sample at a fixed `resolution_ms` (suggest 500–1000 ms). Clamp the
  client's request to a safe range and **echo the actual value** in
  `watch_snapshot`. Don't push faster than you compute.
- **Window / retention.** Keep a ring buffer of `window_ms` per screened
  instrument (suggest cap ~30 min). Backfill from it on `watch`.
- **Backpressure.** If a client lags, coalesce to the latest sample (drop
  intermediate ticks) rather than queueing unbounded — same "latest wins" policy
  as the main stream.
- **Limits.** Cap concurrent watches per session (e.g. ≤ 3) and reject extras
  with an `error`, so one client can't pin every instrument's stream.
- **Reconnect.** Watches are per-connection; on reconnect the client re-sends
  `watch`. A `subscribe`/reconfigure of the alert filters must **not** cancel
  active watches (they're independent).

## 4. Optional REST equivalent (fallback / cold cache)

Not required if §2 lands, but a `GET /spread/history?base=…&quote=…&window_ms=…&
resolution_ms=…` returning the same `points[]` shape is a useful fallback (e.g.
if we ever render a chart without an open socket). The WS `watch_snapshot`
already covers the on-open case, so this is low priority.

## 5. Optional Phase 3 — per-venue breakdown

Only if we later want to show *where* the spread lives (a line/bar per exchange).
Add either a `venues[]` array (`exchange`, `bid`, `ask`, `mid`, `book_age_ms`)
onto each tick, or a separate `venue_tick`. Heavier (grows with venue count) —
defer until the single-line real-time chart is shipped and validated.

## 6. Priority for the server team

1. **`watch` → `watch_snapshot` + `spread_tick` stream (§2)** — the whole
   real-time feature depends only on this.
2. `executable_notional` / `capped_by_depth` per point (§2.4) — big UX win for
   entry/exit timing; include with the MVP if feasible.
3. REST `/spread/history` (§4) and per-venue (§5) — optional, later.

## 7. What the client will build once §2 lands

- Tappable signal cards → instrument detail screen.
- On open: send `watch`, render `watch_snapshot` with **fl_chart** (`net_pct`
  line + `baseline_pct` band, markers where `capped_by_depth` flips), then append
  each `spread_tick` live and trim to `window_ms`.
- On close/back: send `unwatch`. Re-send `watch` after a reconnect.
