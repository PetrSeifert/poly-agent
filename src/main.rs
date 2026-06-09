mod broker;
mod exchange;
mod forecast;
mod ledger;
mod llm;
mod policy;
mod server;
mod triage;
mod types;

use clap::{Parser, Subcommand};
use tracing::{error, info, warn};

use crate::broker::{PaperBroker, PaperBrokerConfig};
use crate::exchange::{ExchangeAdapter, PolymarketIntl};
use crate::ledger::Ledger;
use crate::policy::{POLICY_VERSION, PolicyConfig, PolicyDecision};
use crate::types::{ExecutionMode, OrderStatus};

#[derive(Parser)]
#[command(
    name = "poly-agent",
    about = "Paper-trading agent for prediction markets"
)]
struct Cli {
    /// Path to the SQLite ledger database.
    #[arg(long, default_value = "ledger.db")]
    db: String,

    /// Starting paper bankroll, used when the ledger is first created.
    #[arg(long, default_value_t = 1000.0)]
    starting_cash: f64,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch active markets from Gamma and store metadata in the ledger.
    Discover {
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Record orderbook snapshots for stored markets.
    Record {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Generate stub forecasts (market-anchored, or manual for one market).
    Forecast {
        /// Manually set fair P(YES) for a single market.
        #[arg(long, requires = "prob")]
        market_id: Option<String>,
        #[arg(long)]
        prob: Option<f64>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Run the trading policy over forecasted markets and paper-trade.
    Trade {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Minimum post-fee edge in probability points.
        #[arg(long, default_value_t = 0.05)]
        min_edge: f64,
        /// Taker fee rate; per-share fee is `fee_rate * p * (1 - p)`.
        #[arg(long, default_value_t = 0.05)]
        fee_rate: f64,
    },
    /// Check open positions against official resolutions and realize PnL.
    Settle,
    /// Show paper account state: cash, fees, open positions, marked equity.
    Report,
    /// Serve a live web dashboard for reviewing results in realtime.
    Serve {
        #[arg(long, default_value_t = 8420)]
        port: u16,
    },
    /// Run the full simulation loop for several hours using Codex forecasts:
    /// discover -> snapshot -> LLM forecast -> paper trade -> equity snapshot.
    Run {
        /// Total duration of the simulation in hours (fractions allowed).
        #[arg(long, default_value_t = 4.0)]
        hours: f64,
        /// Minutes between cycles.
        #[arg(long, default_value_t = 15.0)]
        cycle_minutes: f64,
        /// Number of top-volume markets to track.
        #[arg(long, default_value_t = 20)]
        markets: usize,
        /// Maximum Codex forecast calls per cycle (they are slow).
        #[arg(long, default_value_t = 5)]
        max_llm_calls: usize,
        /// Re-forecast a market only when its forecast is older than this.
        #[arg(long, default_value_t = 2.0)]
        forecast_refresh_hours: f64,
        /// Minimum post-fee edge in probability points.
        #[arg(long, default_value_t = 0.05)]
        min_edge: f64,
        /// Taker fee rate; per-share fee is `fee_rate * p * (1 - p)`.
        #[arg(long, default_value_t = 0.05)]
        fee_rate: f64,
        /// Codex model override (e.g. gpt-5-codex); defaults to the CLI default.
        #[arg(long)]
        model: Option<String>,
        /// Thinking level for the model.
        #[arg(long, default_value = "medium", value_parser = ["minimal", "low", "medium", "high", "xhigh"])]
        reasoning_effort: String,
        /// Codex binary to invoke.
        #[arg(long, default_value = "codex")]
        codex_bin: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let ledger = Ledger::open(&cli.db)?;
    ledger.ensure_account(cli.starting_cash)?;
    let exchange = PolymarketIntl::new()?;

    match cli.command {
        Command::Discover { limit } => discover(&ledger, &exchange, limit).await,
        Command::Record { limit } => {
            let universe = select_universe(&ledger, limit)?;
            record(&ledger, &exchange, &universe).await
        }
        Command::Forecast {
            market_id,
            prob,
            limit,
        } => run_forecast(&ledger, &exchange, market_id, prob, limit).await,
        Command::Trade {
            limit,
            min_edge,
            fee_rate,
        } => {
            let universe = select_universe(&ledger, limit)?;
            trade(&ledger, &exchange, &universe, min_edge, fee_rate).await
        }
        Command::Settle => settle(&ledger, &exchange).await,
        Command::Report => report(&ledger),
        Command::Serve { port } => server::serve(cli.db.clone(), port).await,
        Command::Run {
            hours,
            cycle_minutes,
            markets,
            max_llm_calls,
            forecast_refresh_hours,
            min_edge,
            fee_rate,
            model,
            reasoning_effort,
            codex_bin,
        } => {
            let forecaster = llm::CodexForecaster {
                binary: codex_bin,
                model,
                reasoning_effort: Some(reasoning_effort),
                ..llm::CodexForecaster::default()
            };
            run_simulation(
                &ledger,
                &exchange,
                &forecaster,
                RunConfig {
                    hours,
                    cycle_minutes,
                    markets,
                    max_llm_calls,
                    forecast_refresh_hours,
                    min_edge,
                    fee_rate,
                },
            )
            .await
        }
    }
}

struct RunConfig {
    hours: f64,
    cycle_minutes: f64,
    markets: usize,
    max_llm_calls: usize,
    forecast_refresh_hours: f64,
    min_edge: f64,
    fee_rate: f64,
}

/// Multi-outcome events (e.g. "who wins the World Cup") appear as dozens of
/// near-duplicate markets; cap how many can occupy the tradeable universe.
const MAX_MARKETS_PER_EVENT: usize = 3;
/// Discover this many times more markets than the universe size, so the
/// triage filter has something to choose from beyond the top-volume event.
const DISCOVERY_DEPTH_FACTOR: usize = 10;
/// Taker edge is measured in absolute probability points, so books priced
/// near 0 or 1 cannot clear any meaningful edge threshold. Don't waste
/// forecast budget on them.
const MIN_TRADEABLE_MIDPOINT: f64 = 0.05;
const MAX_TRADEABLE_MIDPOINT: f64 = 0.95;

/// Build the tradeable universe: volume-ordered markets filtered down to
/// categories where the agent has repeatable edge, with rules text present
/// and near-duplicate outcomes of the same event capped.
fn select_universe(ledger: &Ledger, limit: usize) -> anyhow::Result<Vec<types::Market>> {
    let candidates = ledger.markets_with_tokens(limit * DISCOVERY_DEPTH_FACTOR)?;
    let mut per_event: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut universe = Vec::new();
    for market in candidates {
        let profile = triage::profile(&market);
        if profile.trade_blocked || profile.forecast_priority <= 0.0 {
            continue;
        }
        if let Some(event_id) = &market.event_id {
            let count = per_event.entry(event_id.clone()).or_insert(0);
            if *count >= MAX_MARKETS_PER_EVENT {
                continue;
            }
            *count += 1;
        }
        universe.push(market);
        if universe.len() >= limit {
            break;
        }
    }
    Ok(universe)
}

async fn run_simulation(
    ledger: &Ledger,
    exchange: &PolymarketIntl,
    forecaster: &llm::CodexForecaster,
    config: RunConfig,
) -> anyhow::Result<()> {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs_f64(config.hours * 3600.0);
    let cycle_duration = std::time::Duration::from_secs_f64(config.cycle_minutes * 60.0);
    let refresh_age = chrono::Duration::seconds((config.forecast_refresh_hours * 3600.0) as i64);
    let mut cycle = 0u32;

    info!(
        hours = config.hours,
        cycle_minutes = config.cycle_minutes,
        markets = config.markets,
        max_llm_calls = config.max_llm_calls,
        "starting simulation run"
    );

    loop {
        cycle += 1;
        let cycle_started = std::time::Instant::now();
        info!(cycle, "cycle start");

        if let Err(error) =
            discover(ledger, exchange, config.markets * DISCOVERY_DEPTH_FACTOR).await
        {
            error!(%error, "discovery failed, continuing with stored markets");
        }
        let universe = select_universe(ledger, config.markets)?;
        info!(
            size = universe.len(),
            "tradeable universe selected (triage-filtered, event-capped)"
        );
        if let Err(error) = record(ledger, exchange, &universe).await {
            error!(%error, "snapshot recording failed");
        }

        // Forecast the markets with the stalest forecasts first, within budget.
        let min_close = chrono::Utc::now()
            + chrono::Duration::seconds(
                (PolicyConfig::default().min_hours_to_close * 3600.0) as i64,
            );
        let mut stale: Vec<(chrono::Duration, &types::Market, triage::TriageProfile)> = Vec::new();
        for market in &universe {
            // Don't spend LLM calls on markets the policy will reject anyway.
            if let Some(close_time) = market.close_time
                && close_time < min_close
            {
                continue;
            }
            let profile = triage::profile(market);
            // Stub forecasts (confidence 0) must not satisfy the refresh
            // window, or the agent never spends its LLM budget.
            let age = ledger
                .forecast_age(&market.market_id, Some(llm::MODEL_VERSION_PREFIX))?
                .unwrap_or(chrono::Duration::days(3650));
            if age > refresh_age {
                stale.push((age, market, profile));
            }
        }

        // Rank candidates by opportunity score so the limited LLM budget goes
        // to the domains where the agent has the most repeatable edge, not
        // just to whatever forecast happens to be stalest.
        let mut scored = Vec::new();
        for (age, market, profile) in &stale {
            let Some(yes_token) = &market.yes_token_id else {
                continue;
            };
            let yes_book = match exchange.get_orderbook(yes_token).await {
                Ok(book) => book,
                Err(error) => {
                    warn!(market = %market.slug, %error, "no book, skipping forecast");
                    continue;
                }
            };
            let Some(midpoint) = yes_book.midpoint() else {
                warn!(market = %market.slug, "empty book, skipping forecast");
                continue;
            };
            if !(MIN_TRADEABLE_MIDPOINT..=MAX_TRADEABLE_MIDPOINT).contains(&midpoint) {
                info!(
                    market = %market.slug,
                    midpoint = format!("{midpoint:.3}"),
                    "skipping forecast: price too extreme for taker edge"
                );
                continue;
            }
            let age_hours = age.num_minutes() as f64 / 60.0;
            let score = triage::opportunity_score(profile, market, &yes_book, age_hours);
            scored.push((score, *market, yes_book));
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));

        let mut candidates = Vec::new();
        for (score, market, yes_book) in scored {
            if candidates.len() >= config.max_llm_calls {
                break;
            }
            info!(
                market = %market.slug,
                category = triage::classify(market).as_str(),
                score = format!("{score:.3}"),
                "selected for forecast"
            );
            candidates.push((market, yes_book));
        }

        // Each forecast is an independent codex process, so run them all
        // concurrently instead of paying 30-90s per market sequentially.
        if !candidates.is_empty() {
            info!(
                count = candidates.len(),
                "requesting codex forecasts concurrently"
            );
        }
        let results = futures::future::join_all(candidates.iter().map(|(market, yes_book)| {
            let forecaster = &forecaster;
            async move { (market, forecaster.forecast(market, yes_book).await) }
        }))
        .await;

        let mut llm_calls = 0;
        for (market, result) in results {
            match result {
                Ok(forecast) => {
                    info!(
                        market = %market.slug,
                        fair_prob_yes = forecast.fair_prob_yes,
                        confidence = forecast.confidence,
                        "codex forecast stored"
                    );
                    ledger.insert_forecast(&forecast)?;
                    llm_calls += 1;
                }
                Err(error) => {
                    error!(market = %market.slug, %error, "codex forecast failed");
                }
            }
        }

        if let Err(error) = trade(
            ledger,
            exchange,
            &universe,
            config.min_edge,
            config.fee_rate,
        )
        .await
        {
            error!(%error, "trade pass failed");
        }

        // Settle resolved markets before marking equity, so realized PnL is
        // separated from mark-to-market noise.
        if let Err(error) = settle(ledger, exchange).await {
            error!(%error, "settlement pass failed");
        }

        match ledger.record_equity_snapshot() {
            Ok((liquidation, midpoint)) => {
                info!(
                    cycle,
                    llm_calls,
                    equity_liquidation = format!("{liquidation:.2}"),
                    equity_midpoint = format!("{midpoint:.2}"),
                    "cycle complete"
                );
            }
            Err(error) => error!(%error, "equity snapshot failed"),
        }

        if std::time::Instant::now() >= deadline {
            break;
        }
        let elapsed = cycle_started.elapsed();
        if elapsed < cycle_duration {
            let sleep_duration = cycle_duration - elapsed;
            // Don't oversleep past the deadline.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::time::sleep(sleep_duration.min(remaining)).await;
        }
    }

    info!(cycles = cycle, "simulation run finished");
    report(ledger)
}

async fn discover(ledger: &Ledger, exchange: &PolymarketIntl, limit: usize) -> anyhow::Result<()> {
    let filter = types::MarketFilter {
        active_only: true,
        limit,
    };
    let markets = exchange.discover_markets(&filter).await?;
    let mut stored = 0;
    for market in &markets {
        if market.yes_token_id.is_none() {
            continue;
        }
        ledger.upsert_market(market)?;
        stored += 1;
    }
    info!(fetched = markets.len(), stored, "market discovery complete");
    Ok(())
}

async fn record(
    ledger: &Ledger,
    exchange: &PolymarketIntl,
    markets: &[types::Market],
) -> anyhow::Result<()> {
    if markets.is_empty() {
        warn!("no markets selected; run `discover` first");
        return Ok(());
    }
    let mut recorded = 0;
    for market in markets {
        for token_id in [&market.yes_token_id, &market.no_token_id]
            .into_iter()
            .flatten()
        {
            match exchange.get_orderbook(token_id).await {
                Ok(book) => {
                    ledger.insert_snapshot(exchange.venue(), &market.market_id, &book)?;
                    recorded += 1;
                }
                Err(error) => {
                    warn!(market = %market.slug, %token_id, %error, "snapshot failed");
                }
            }
        }
    }
    info!(
        recorded,
        markets = markets.len(),
        "orderbook recording complete"
    );
    Ok(())
}

async fn run_forecast(
    ledger: &Ledger,
    exchange: &PolymarketIntl,
    market_id: Option<String>,
    prob: Option<f64>,
    limit: usize,
) -> anyhow::Result<()> {
    let markets = ledger.markets_with_tokens(limit)?;
    let mut count = 0;
    for market in &markets {
        if let Some(target) = &market_id
            && &market.market_id != target
        {
            continue;
        }
        let Some(yes_token) = &market.yes_token_id else {
            continue;
        };
        let book = match exchange.get_orderbook(yes_token).await {
            Ok(book) => book,
            Err(error) => {
                warn!(market = %market.slug, %error, "skipping forecast, no book");
                continue;
            }
        };
        let manual = if market_id.as_deref() == Some(market.market_id.as_str()) {
            prob
        } else {
            None
        };
        if let Some(forecast) = forecast::stub_forecast(market, &book, manual) {
            ledger.insert_forecast(&forecast)?;
            count += 1;
            info!(
                market = %market.slug,
                fair_prob_yes = forecast.fair_prob_yes,
                "forecast stored"
            );
        }
    }
    if let Some(target) = market_id
        && count == 0
    {
        warn!(%target, "market not found among stored active markets");
    }
    info!(count, "forecasting complete");
    Ok(())
}

async fn trade(
    ledger: &Ledger,
    exchange: &PolymarketIntl,
    markets: &[types::Market],
    min_edge: f64,
    fee_rate: f64,
) -> anyhow::Result<()> {
    let policy_config = PolicyConfig {
        min_edge,
        fee_rate,
        ..PolicyConfig::default()
    };
    let broker = PaperBroker::new(PaperBrokerConfig {
        fee_rate: policy_config.fee_rate,
        allow_partial_fills: false,
    });
    let mode = ExecutionMode::Paper;

    let bankroll = ledger.cash()?;
    info!(
        bankroll,
        markets = markets.len(),
        "starting paper trading pass"
    );

    for market in markets {
        let Some(forecast) = ledger.latest_forecast(&market.market_id)? else {
            continue;
        };
        let Some(yes_token) = &market.yes_token_id else {
            continue;
        };
        let yes_book = match exchange.get_orderbook(yes_token).await {
            Ok(book) => book,
            Err(error) => {
                warn!(market = %market.slug, %error, "skipping, no yes book");
                continue;
            }
        };
        ledger.insert_snapshot(exchange.venue(), &market.market_id, &yes_book)?;

        // Fetch the NO book up front so the policy prices the NO side from
        // the book it would actually execute against, not 1 - YES bid.
        let no_book = match &market.no_token_id {
            Some(no_token) => match exchange.get_orderbook(no_token).await {
                Ok(book) => {
                    ledger.insert_snapshot(exchange.venue(), &market.market_id, &book)?;
                    Some(book)
                }
                Err(error) => {
                    warn!(market = %market.slug, %error, "no NO book; evaluating YES side only");
                    None
                }
            },
            None => None,
        };

        let existing_cost = ledger.position_cost(&market.market_id)?;
        let decision = policy::evaluate(
            &policy_config,
            market,
            &yes_book,
            no_book.as_ref(),
            &forecast,
            bankroll,
            existing_cost,
        );
        let (order, edge) = match decision {
            PolicyDecision::Trade { order, edge } => (order, edge),
            PolicyDecision::NoTrade { reason } => {
                info!(market = %market.slug, reason, "no trade");
                continue;
            }
        };

        // Execute against the book of the token actually being bought.
        let execution_book = if order.token_id == yes_book.token_id {
            yes_book
        } else {
            match no_book {
                Some(book) if order.token_id == book.token_id => book,
                _ => {
                    warn!(market = %market.slug, "skipping, no execution book");
                    continue;
                }
            }
        };

        let result = broker.execute(&order, &execution_book);
        let order_id = ledger.insert_order(
            &order,
            mode,
            result.status,
            result.reject_reason.as_deref(),
            POLICY_VERSION,
        )?;
        match result.status {
            OrderStatus::Rejected => {
                info!(
                    market = %market.slug,
                    reason = result.reject_reason.as_deref().unwrap_or("unknown"),
                    "paper order rejected"
                );
            }
            _ => {
                for fill in &result.fills {
                    ledger.insert_fill(order_id, fill)?;
                    ledger.apply_buy_fill(
                        &order.market_id,
                        &order.token_id,
                        order.outcome.as_str(),
                        fill,
                    )?;
                }
                let total_size: f64 = result.fills.iter().map(|fill| fill.size).sum();
                let total_cost: f64 = result
                    .fills
                    .iter()
                    .map(|fill| fill.price * fill.size + fill.fee)
                    .sum();
                info!(
                    market = %market.slug,
                    outcome = order.outcome.as_str(),
                    edge = format!("{edge:.4}"),
                    shares = total_size,
                    cost = format!("{total_cost:.2}"),
                    "paper order filled"
                );
            }
        }
    }
    Ok(())
}

/// Poll Gamma for resolutions of markets with open positions and settle any
/// that resolved: pay out shares, realize PnL, and close the positions.
async fn settle(ledger: &Ledger, exchange: &PolymarketIntl) -> anyhow::Result<()> {
    let market_ids = ledger.unsettled_position_markets()?;
    if market_ids.is_empty() {
        info!("no open positions awaiting resolution");
        return Ok(());
    }
    let mut settled = 0;
    for market_id in &market_ids {
        match exchange.get_resolution(market_id).await {
            Ok(Some(resolution)) => {
                let (payout, realized_pnl) = ledger.settle_market(&resolution)?;
                settled += 1;
                info!(
                    market_id,
                    payout = format!("{payout:.2}"),
                    realized_pnl = format!("{realized_pnl:.2}"),
                    "market settled"
                );
            }
            Ok(None) => {}
            Err(error) => {
                warn!(market_id, %error, "resolution check failed");
            }
        }
    }
    info!(
        checked = market_ids.len(),
        settled, "settlement pass complete"
    );
    Ok(())
}

fn report(ledger: &Ledger) -> anyhow::Result<()> {
    let summary = ledger.summary()?;
    println!("cash:          {:.2}", summary.cash);
    println!("realized pnl:  {:.2}", summary.realized_pnl);
    println!("total fees:    {:.2}", summary.total_fees);
    println!("open positions: {}", summary.open_positions.len());

    let mut mtm_liquidation = 0.0;
    let mut mtm_mid = 0.0;
    let mut have_marks = true;
    for position in &summary.open_positions {
        let quote = ledger.latest_snapshot_quote(&position.token_id)?;
        let mark = match quote {
            Some((best_bid, midpoint)) => {
                mtm_liquidation += position.shares * best_bid;
                mtm_mid += position.shares * midpoint;
                format!("bid {best_bid:.3} / mid {midpoint:.3}")
            }
            None => {
                have_marks = false;
                "no snapshot".to_string()
            }
        };
        println!(
            "  {} {} x{:.0} cost {:.2} mark [{}]",
            position.market_id, position.outcome, position.shares, position.cost_basis, mark
        );
    }
    if have_marks {
        println!(
            "equity (liquidation): {:.2}",
            summary.cash + mtm_liquidation
        );
        println!("equity (midpoint):    {:.2}", summary.cash + mtm_mid);
    } else {
        println!("equity: incomplete marks; run `record` to refresh snapshots");
    }

    let (filled, rejected) = ledger.order_counts()?;
    println!("orders: {filled} filled, {rejected} rejected");

    let settlements = ledger.recent_settlements(20)?;
    if !settlements.is_empty() {
        println!("\nrecent settlements:");
        for settlement in &settlements {
            println!(
                "  {}  {} {} x{:.0} payout {:.2} pnl {:+.2}",
                &settlement.settled_at[..19.min(settlement.settled_at.len())],
                settlement.question,
                settlement.outcome,
                settlement.shares,
                settlement.payout,
                settlement.realized_pnl,
            );
        }
    }

    let curve = ledger.equity_curve(500)?;
    if !curve.is_empty() {
        println!("\nequity curve (liquidation-marked):");
        let values: Vec<f64> = curve.iter().map(|point| point.1).collect();
        let low = values.iter().cloned().fold(f64::INFINITY, f64::min);
        let high = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let span = (high - low).max(1e-9);
        for (ts, liquidation, midpoint, open_positions) in &curve {
            let width = 40usize;
            let filled_width = (((liquidation - low) / span) * width as f64).round() as usize;
            let bar: String = "#".repeat(filled_width.min(width));
            println!(
                "  {}  {:>9.2} liq / {:>9.2} mid  ({} pos) |{:<width$}|",
                &ts[..19.min(ts.len())],
                liquidation,
                midpoint,
                open_positions,
                bar,
                width = width
            );
        }
    }
    Ok(())
}
