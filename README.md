# poly-agent

A paper-trading agent for Polymarket prediction markets, implementing the first
build target from `IMPLEMENTATION_RESEARCH.md`. Read-only against public APIs; no live
execution, no keys, no authentication.

## Components

- **Exchange adapter** (`src/exchange.rs`): `ExchangeAdapter` trait plus a
  read-only Polymarket International implementation (Gamma API for market
  discovery with offset pagination, CLOB API for orderbooks).
- **Triage** (`src/triage.rs`): implements the edge-preference ranking from
  `MARKET_AGENT_RESEARCH.md`. Classifies each market into a domain by keyword
  (crypto, sports, economics, weather, politics, geopolitics, culture, other)
  and derives a profile: structured-data domains get full forecast trust,
  "other" needs 1.5x the minimum edge at reduced trust, and narrative-heavy
  domains (politics, geopolitics, culture) are blocked outright. Missing
  resolution rules make a market untradeable; thin rules raise the required
  edge. An opportunity score (category prior, liquidity, spread tightness,
  forecast staleness) ranks markets for the scarce LLM forecast budget.
- **Ledger** (`src/ledger.rs`): SQLite schema for markets, orderbook
  snapshots (including CLOB condition ID, exchange timestamp, and book hash),
  forecasts, orders, fills, positions, resolutions, settlements, and the
  paper account.
- **Paper broker** (`src/broker.rs`): simulates marketable orders by walking
  the live book — fills against real depth, applies the `feeRate * p * (1 - p)`
  taker fee model, tracks slippage, and rejects orders the book cannot satisfy
  within the limit price or below market minimum size.
- **Trading policy** (`src/policy.rs`): deterministic gate — trades only when
  post-fee edge exceeds a threshold (scaled by the triage category and rules
  clarity), with spread, time-to-close, and per-market position-size limits.
  Each side is priced from the orderbook of the token that would actually be
  bought (the NO edge uses the real NO ask, never `1 - YES bid`), and the
  spread gate applies to the selected token's book.
  Shrinks forecasts toward the market price
  (`p = w * p_agent + (1 - w) * p_market`), where `w` includes both the
  model's confidence and the triage trust factor for the domain.
- **Forecast stub** (`src/forecast.rs`): market-anchored or manual
  probabilities, useful for testing the accounting pipeline.
- **LLM forecaster** (`src/llm.rs`): shells out to the Codex CLI
  (`codex exec --sandbox read-only`), which runs on the ChatGPT subscription —
  no API key. The model sees only public market data and returns a structured
  JSON forecast (probability, confidence, evidence, do-not-trade reason); it
  never touches keys, orders, or the ledger. Forecast confidence scales the
  shrinkage weight, so a "do not trade" forecast collapses to the market price.

## Usage

```bash
cargo build

# 1. Fetch active markets (sorted by 24h volume, paginated)
cargo run -- discover --limit 200

# 2. Record orderbook snapshots for the tradeable universe
cargo run -- record --limit 20

# 3. Generate forecasts (market-anchored stub, or manual for one market)
cargo run -- forecast --limit 20
cargo run -- forecast --market-id 558969 --prob 0.40

# 4. Run the policy and paper-trade (taker fee rate is configurable)
cargo run -- trade --limit 20 --min-edge 0.05 --fee-rate 0.05

# 5. Settle open positions against official (UMA) resolutions
cargo run -- settle

# 6. Account state: cash, realized PnL, positions, settlements, equity curve
cargo run -- report
```

`record` and `trade` operate on the *tradeable universe*, not the raw
top-volume list: discovery digs 10x deeper than the universe size, then triage
drops avoided categories and rule-less markets and caps near-duplicate
outcomes of the same event (e.g. dozens of "will X win the World Cup" markets)
at 3 slots, so one big event cannot crowd out everything else.

## Multi-hour LLM simulation

Requires the Codex CLI, logged in with a ChatGPT subscription:

```bash
npm install -g @openai/codex   # or: npm install -g --prefix ~/.local @openai/codex
codex login
```

Then run the loop (discover → snapshot → Codex forecast → paper trade →
settle resolved markets → equity snapshot, repeated each cycle):

```bash
cargo run --release -- run \
    --hours 6 \
    --cycle-minutes 15 \
    --markets 20 \
    --max-llm-calls 5 \
    --forecast-refresh-hours 2 \
    --min-edge 0.05
```

Notes:

- Codex forecasts run concurrently (one `codex exec` process each), so a
  batch costs roughly one forecast's latency (30–90s). `--max-llm-calls`
  caps calls (and thus concurrent processes) per cycle. The budget goes to
  the highest opportunity-score markets among those with stale forecasts
  (only real Codex forecasts count toward freshness, not stubs). Markets
  closing within the policy's minimum time-to-close, and books priced
  outside 0.05–0.95 (no achievable taker edge), are never sent to the LLM.
- `--model` overrides the Codex model and `--reasoning-effort` the thinking
  level (`minimal|low|medium|high|xhigh`, default `medium`). On a
  ChatGPT-subscription login,
  `gpt-5.5` (default) and `gpt-5.4` are available; API-only slugs like
  `*-codex` and `*-mini` are rejected. Both are recorded in the forecast's
  `model_version` (e.g. `codex-exec-v1:gpt-5.4:low`) so runs can be compared.
- `--codex-bin` sets the binary path (e.g. `~/.local/bin/codex` if it is not
  on `PATH`).
- Watch results live with `cargo run -- report` from another terminal; the
  equity curve (liquidation- and midpoint-marked) accumulates one point per
  cycle in `equity_snapshots`.

## Web dashboard

For realtime review during a run, serve the dashboard from a second terminal
against the same database:

```bash
cargo run --release -- serve            # http://127.0.0.1:8420
cargo run --release -- serve --port 9000
```

It auto-refreshes every 3 seconds and shows the equity curve (liquidation and
midpoint marks), account cards with session PnL, open positions with
unrealized PnL, recent forecasts (model probability vs market, confidence,
do-not-trade reasons), and recent orders with fills and rejection reasons.
The ledger runs in WAL mode, so the server reads safely while a simulation
writes from another process.

State lives in `ledger.db` by default (`--db` to override); the starting paper
bankroll is set on first run (`--starting-cash`, default 1000).

## Not yet implemented

Per the staged-mode plan in `IMPLEMENTATION_RESEARCH.md`: per-market fee
parameter lookup from CLOB market info (a flat `--fee-rate` is used instead),
limit-order queue simulation, WebSocket streaming, calibration/evaluation
reports, the external-evidence layer for the LLM (structured reference
prices, sportsbook lines, weather data), and any live adapter.
`ExecutionMode` already enumerates the live modes so enabling them later is an
explicit, typed decision.

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
See `LICENSE-APACHE` and `LICENSE-MIT`.
