Here’s the implementation research and a concrete build plan.

## 1. Core finding: build a venue-agnostic agent

Polymarket has two relevant surfaces now:

**Polymarket International** is crypto-based and uses a hybrid CLOB: order matching is off-chain, settlement is on Polygon, orders are EIP-712 signed, and official SDKs exist for TypeScript, Python, and Rust. Public market data, prices, orderbooks, spreads, and Gamma metadata endpoints do not require authentication, while trading requires wallet/API authentication. ([Polymarket Documentation][1])

**Polymarket US** is separate: it is described as fiat-based, CFTC-regulated, trades in USD, and built for U.S. residents. The CFTC lists QCX LLC d/b/a Polymarket US as a designated contract market with designation dated July 9, 2025. Its API is separate: public data is under the public API, authenticated trading is under the authenticated API, and it has its own SDK and WebSocket endpoints. ([Polymarket US Documentation][2])

This matters because the International API docs currently list the United States as blocked for order placement and provide a geoblock endpoint that builders should check before trading. So the implementation should have an `ExchangeAdapter` abstraction and a hard compliance gate before live execution. ([Polymarket Documentation][3])

## 2. Recommended architecture

Use the same agent for paper and live trading, but keep the live broker disabled until the paper system has proven itself under realistic fill assumptions.

```text
                 ┌──────────────────────┐
                 │ Market discovery      │
                 │ Gamma / US public API │
                 └──────────┬───────────┘
                            │
                 ┌──────────▼───────────┐
                 │ Orderbook collector   │
                 │ REST + WebSocket      │
                 └──────────┬───────────┘
                            │
       ┌────────────────────▼────────────────────┐
       │ Evidence / research layer                │
       │ news, sports stats, crypto, finance data │
       └────────────────────┬────────────────────┘
                            │
                 ┌──────────▼───────────┐
                 │ Forecast engine       │
                 │ fair probability p    │
                 └──────────┬───────────┘
                            │
                 ┌──────────▼───────────┐
                 │ Trading policy        │
                 │ edge, sizing, limits  │
                 └──────────┬───────────┘
                            │
        ┌───────────────────▼───────────────────┐
        │ Broker interface                       │
        │ PaperBroker now, LiveBroker later      │
        └───────────────────┬───────────────────┘
                            │
                 ┌──────────▼───────────┐
                 │ Ledger + evaluation   │
                 │ PnL, calibration, risk│
                 └──────────────────────┘
```

The LLM should not have custody or signing authority. It should produce a structured recommendation; a deterministic risk engine should decide whether the recommendation is tradable. Live private keys or API secrets should stay in a separate execution service, never in prompts, logs, notebooks, or the research agent.

## 3. Exchange/data layer

For **Polymarket International**, use:

```text
Market discovery:
  Gamma API: events, markets, tags, sports metadata

Orderbook/prices:
  CLOB API: price, prices, book, books, midpoint, spread, price history

Streaming:
  Market WebSocket: public orderbook, price changes, trades, market events
  User WebSocket: authenticated order/trade updates
```

The docs say the Gamma API is used for events/markets/discovery, the CLOB API is used for prices and orderbooks, and the Data API covers positions, trades, open interest, holders, and analytics. ([Polymarket Documentation][4]) Market discovery should start with `active=true&closed=false`, pagination, and high-volume sorting such as `order=volume_24hr&ascending=false`. ([Polymarket Documentation][5])

For orderbook simulation, use full book snapshots, best bid/ask, tick size, min order size, spread, midpoint, and price history. The CLOB docs expose full orderbooks, batch orderbook requests up to 500 tokens, midpoint, spread, price history, and slippage estimation by walking orderbook depth. ([Polymarket Documentation][6])

For live streams, subscribe to the market WebSocket by asset IDs. It provides full orderbook snapshots, price-level changes, last-trade events, best bid/ask, new-market events, and market-resolution events when custom features are enabled. ([Polymarket Documentation][7])

For **Polymarket US**, use the separate API/SDK. Public endpoints cover events, markets, BBO/orderbooks, series, sports, and search, while authenticated endpoints cover orders, positions, balances, and account state. The US WebSocket API has private streams for orders/positions/balances and market streams for orderbook/trade data. ([Polymarket US Documentation][8])

## 4. Paper-trading engine

Do not paper-trade by merely marking against last price. That will overstate performance. Implement a broker simulator with these rules:

For **marketable orders**, walk the orderbook at decision time. A paper buy fills against asks, a paper sell fills against bids. Apply depth, slippage, min order size, tick size, and fees. Reject the simulated order if the book cannot satisfy size within the agent’s max price.

For **limit orders**, use a conservative queue model. When the paper order is placed, record the visible size ahead at that price. Only fill once subsequent trade/price-change events plausibly consume that queue. If you cannot model queue priority, assume you are behind all visible resting size at that level.

For **mark-to-market**, track both mid-price equity and liquidation equity. Liquidation equity is more honest: value long YES shares at best bid, not midpoint. Keep unresolved PnL separate from realized PnL.

For **settlement**, resolve positions using official market resolution data. Do not count a strategy as successful just because mark-to-market moved in its favor before resolution.

Minimum ledger tables:

```sql
markets(
  venue,
  event_id,
  market_id,
  slug,
  question,
  resolution_rules,
  close_time,
  active,
  closed,
  neg_risk,
  yes_token_id,
  no_token_id
);

orderbook_snapshots(
  ts,
  venue,
  market_id,
  token_id,
  best_bid,
  best_ask,
  spread,
  midpoint,
  raw_book_json
);

forecasts(
  ts,
  market_id,
  fair_prob_yes,
  confidence,
  model_version,
  evidence_hash,
  rationale_json
);

orders(
  id,
  ts,
  mode,              -- paper | live
  market_id,
  token_id,
  side,
  order_type,
  limit_price,
  size,
  status,
  policy_version
);

fills(
  order_id,
  ts,
  price,
  size,
  fee,
  slippage,
  liquidity_flag     -- maker | taker | simulated
);

positions(
  ts,
  market_id,
  yes_shares,
  no_shares,
  cash,
  realized_pnl,
  mtm_mid_pnl,
  mtm_liquidation_pnl
);
```

## 5. Trading policy

A simple first policy is better than a complex autonomous agent. Start with “forecast edge after execution cost.”

For a YES buy:

```text
fee_per_share = fee_rate * ask * (1 - ask)
edge_per_share = fair_prob_yes - ask - fee_per_share
```

For a NO buy:

```text
fee_per_share = fee_rate * no_ask * (1 - no_ask)
edge_per_share = (1 - fair_prob_yes) - no_ask - fee_per_share
```

Trade only when:

```text
edge_per_share > min_edge
order_size <= max_position_size
market_liquidity >= min_liquidity
spread <= max_spread
time_to_resolution >= min_time
correlated_exposure <= max_category_exposure
```

This fee model is important because Polymarket’s current docs say taker fees are calculated as `C × feeRate × p × (1 - p)`, makers are not charged fees, and fee rates vary by market category. ([Polymarket Documentation][9])

I would initially use small, conservative thresholds, for example:

```text
min_edge:              3–7 percentage points
max_spread:            3–5 cents for taker trades
max_position:          1–2% of bankroll per market
max_category_exposure: 10–20% of bankroll
max_unresolved_pnl:    cap separately from realized PnL
```

Market-making is a separate mode. Polymarket docs describe market makers as liquidity providers who continuously post bid/ask orders, and they warn that crossed or negative-spread quotes lose money on every fill. Only add this after the taker paper-trading engine works. ([Polymarket Documentation][10]) Maker rebates and liquidity rewards exist, but the simulator should treat them as upside only after you can model eligibility and fills accurately. ([Polymarket Documentation][11])

## 6. Forecasting layer

Use the LLM as a research and probability-estimation component, not as the final execution authority.

A forecast object should look like this:

```json
{
  "market_id": "string",
  "question": "Will X happen by date Y?",
  "fair_prob_yes": 0.57,
  "confidence": 0.62,
  "time_horizon_days": 14,
  "evidence": [
    {
      "source": "official/statistical/news source",
      "claim": "short claim",
      "timestamp": "2026-06-09T12:00:00Z"
    }
  ],
  "base_rate": 0.51,
  "market_price_seen": 0.49,
  "main_uncertainties": ["..."],
  "resolution_risks": ["..."],
  "do_not_trade_reason": null
}
```

The policy should then shrink raw model probabilities toward the market until the agent proves it is calibrated:

```text
p_final = w * p_agent + (1 - w) * p_market
```

Start with a small `w`, such as `0.2` or `0.3`. Increase it only after out-of-sample calibration and realized PnL justify it.

Good market filters:

```text
Reject:
  ambiguous resolution criteria
  very wide spreads
  low depth
  markets near resolution unless the data source is real-time
  markets where the agent lacks domain-specific data
  markets with obvious correlated duplicate exposure
```

Useful domains for early experiments:

```text
Better for structured models:
  sports, crypto, weather, macro releases, earnings-style markets

Harder for LLM-only reasoning:
  politics, breaking news, vague culture markets, legal/geopolitical escalation markets
```

## 7. Evaluation criteria before live money

Use both forecast metrics and trading metrics.

Forecast quality:

```text
Brier score
log loss
calibration by probability bucket
closing-line value versus later market prices
performance by category
performance by time-to-resolution
```

Trading quality:

```text
realized PnL
liquidation-marked PnL
max drawdown
fee drag
slippage
average edge at entry
PnL by category
PnL by strategy version
PnL from resolved vs unresolved markets
capital-weighted return
return per dollar-day of capital
```

A reasonable live-readiness gate:

```text
No live trading until:
  at least hundreds of paper decisions are logged
  at least dozens of paper trades are completed
  paper fills include realistic slippage and fees
  realized resolved PnL is positive, not just unresolved MTM
  calibration is acceptable across probability buckets
  the strategy beats simple baselines:
    - no trade
    - buy market favorites
    - follow midpoint momentum
    - random trade with same sizing
  a kill switch and max-loss cap exist
```

## 8. Live execution safety model

When switching from imaginary budget to real money, do not make it fully autonomous immediately. Use staged modes:

```text
Mode 0: read-only collector
Mode 1: paper trading only
Mode 2: live shadow mode, logs intended orders but sends none
Mode 3: human-confirmed live orders
Mode 4: capped autonomous live orders
```

The live executor should enforce:

```text
daily loss limit
per-market max loss
per-category max exposure
max order size
max slippage
allowlist of market categories
denylist of ambiguous markets
cooldown after failed orders
cancel-all kill switch
secret isolation
complete audit log
```

For International, trading authentication uses L1 private-key signing to create/derive API credentials and L2 HMAC credentials for trading requests; order creation still requires signing order payloads. Polymarket’s docs explicitly warn not to commit private keys and to use environment variables or secure key management. ([Polymarket Documentation][12]) For the authenticated user WebSocket, the docs also warn not to expose API credentials client-side and to use it only from server environments. ([Polymarket Documentation][13])

## 9. Language/stack choice

Given your background, I would split it this way:

```text
Rust:
  exchange adapter
  orderbook normalization
  paper/live broker
  risk engine
  deterministic execution service

Python:
  research/forecasting experiments
  notebooks
  calibration analysis
  model evaluation

Postgres + TimescaleDB or DuckDB:
  market snapshots
  fills
  forecasts
  PnL
  evaluation

Grafana/Metabase/Streamlit:
  dashboard
```

For International, the official docs list TypeScript, Python, and Rust SDKs. ([Polymarket Documentation][14]) For Polymarket US, the quickstart currently shows TypeScript/Python SDK usage and separate authenticated/public APIs, so a Rust implementation may need to wrap REST/WebSocket directly unless a Rust SDK appears. ([Polymarket US Documentation][8])

A Rust trait shape:

```rust
#[async_trait::async_trait]
pub trait ExchangeAdapter {
    async fn discover_markets(&self, filter: MarketFilter) -> anyhow::Result<Vec<Market>>;
    async fn get_orderbook(&self, token_id: &str) -> anyhow::Result<OrderBook>;
    async fn stream_market_data(&self, tokens: Vec<String>) -> anyhow::Result<MarketStream>;
    async fn submit_order(&self, order: NewOrder, mode: ExecutionMode) -> anyhow::Result<OrderReceipt>;
    async fn cancel_order(&self, order_id: &str) -> anyhow::Result<()>;
    async fn positions(&self) -> anyhow::Result<Vec<Position>>;
}
```

The broker mode should be an enum:

```rust
pub enum ExecutionMode {
    ReadOnly,
    Paper,
    ShadowLive,
    HumanConfirmedLive,
    CappedAutoLive,
}
```

## 10. First implementation target

Build this first:

```text
1. Market collector
   - Fetch active markets.
   - Store market metadata, token IDs, close time, liquidity, spread.

2. Orderbook recorder
   - Poll REST initially.
   - Add WebSocket once storage schema is stable.

3. Paper broker
   - Simulate marketable orders by walking the book.
   - Apply fees, spread, depth, and rejection rules.

4. Forecast stub
   - Start with manual or simple model forecasts.
   - Do not add complex LLM reasoning until the accounting is correct.

5. Trading policy
   - Trade only when fair probability minus execution cost exceeds threshold.

6. Dashboard
   - Equity curve.
   - Open exposure.
   - Forecast calibration.
   - Realized vs unresolved PnL.
   - Biggest losing markets.

7. LLM research layer
   - Structured JSON forecasts.
   - Source tracking.
   - No key access.
   - Risk engine remains deterministic.

8. Live adapter
   - Implement but keep disabled.
   - Run in shadow mode before any real order submission.
```
