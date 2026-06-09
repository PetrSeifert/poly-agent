# poly-agent

A paper-trading agent for Polymarket prediction markets, implementing the first
build target from `RESEARCH.md`. Read-only against public APIs; no live
execution, no keys, no authentication.

## Components

- **Exchange adapter** (`src/exchange.rs`): `ExchangeAdapter` trait plus a
  read-only Polymarket International implementation (Gamma API for market
  discovery, CLOB API for orderbooks).
- **Ledger** (`src/ledger.rs`): SQLite schema for markets, orderbook
  snapshots, forecasts, orders, fills, positions, and the paper account.
- **Paper broker** (`src/broker.rs`): simulates marketable orders by walking
  the live book — fills against real depth, applies the `feeRate * p * (1 - p)`
  taker fee model, tracks slippage, and rejects orders the book cannot satisfy
  within the limit price or below market minimum size.
- **Trading policy** (`src/policy.rs`): deterministic gate — trades only when
  post-fee edge exceeds a threshold, with spread, time-to-close, and
  per-market position-size limits. Shrinks forecasts toward the market price
  (`p = w * p_agent + (1 - w) * p_market`).
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

# 1. Fetch active markets (sorted by 24h volume)
cargo run -- discover --limit 50

# 2. Record orderbook snapshots
cargo run -- record --limit 20

# 3. Generate forecasts (market-anchored stub, or manual for one market)
cargo run -- forecast --limit 20
cargo run -- forecast --market-id 558969 --prob 0.40

# 4. Run the policy and paper-trade
cargo run -- trade --limit 20 --min-edge 0.05

# 5. Account state: cash, positions, equity curve, order counts
cargo run -- report
```

## Multi-hour LLM simulation

Requires the Codex CLI, logged in with a ChatGPT subscription:

```bash
npm install -g @openai/codex   # or: npm install -g --prefix ~/.local @openai/codex
codex login
```

Then run the loop (discover → snapshot → Codex forecast → paper trade →
equity snapshot, repeated each cycle):

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

- Each Codex forecast takes 30–90 seconds; `--max-llm-calls` caps calls per
  cycle and stale forecasts are refreshed first. Markets closing within the
  policy's minimum time-to-close are never sent to the LLM.
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

State lives in `ledger.db` by default (`--db` to override); the starting paper
bankroll is set on first run (`--starting-cash`, default 1000).

## Not yet implemented

Per the staged-mode plan in `RESEARCH.md`: settlement against official
resolution data, limit-order queue simulation, WebSocket streaming,
calibration/evaluation reports, the LLM research layer, and any live adapter.
`ExecutionMode` already enumerates the live modes so enabling them later is an
explicit, typed decision.
