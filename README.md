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
  probabilities; the slot where models/LLM research plug in later.

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

# 5. Account state: cash, positions, liquidation- and mid-marked equity
cargo run -- report
```

State lives in `ledger.db` by default (`--db` to override); the starting paper
bankroll is set on first run (`--starting-cash`, default 1000).

## Not yet implemented

Per the staged-mode plan in `RESEARCH.md`: settlement against official
resolution data, limit-order queue simulation, WebSocket streaming,
calibration/evaluation reports, the LLM research layer, and any live adapter.
`ExecutionMode` already enumerates the live modes so enabling them later is an
explicit, typed decision.
