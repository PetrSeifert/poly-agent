mod broker;
mod exchange;
mod forecast;
mod ledger;
mod policy;
mod types;

use clap::{Parser, Subcommand};
use tracing::{info, warn};

use crate::broker::{PaperBroker, PaperBrokerConfig};
use crate::exchange::{ExchangeAdapter, PolymarketIntl};
use crate::ledger::Ledger;
use crate::policy::{POLICY_VERSION, PolicyConfig, PolicyDecision};
use crate::types::{ExecutionMode, OrderStatus};

#[derive(Parser)]
#[command(name = "poly-agent", about = "Paper-trading agent for prediction markets")]
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
        #[arg(long, default_value_t = 50)]
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
    },
    /// Show paper account state: cash, fees, open positions, marked equity.
    Report,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let ledger = Ledger::open(&cli.db)?;
    ledger.ensure_account(cli.starting_cash)?;
    let exchange = PolymarketIntl::new()?;

    match cli.command {
        Command::Discover { limit } => discover(&ledger, &exchange, limit).await,
        Command::Record { limit } => record(&ledger, &exchange, limit).await,
        Command::Forecast {
            market_id,
            prob,
            limit,
        } => run_forecast(&ledger, &exchange, market_id, prob, limit).await,
        Command::Trade { limit, min_edge } => trade(&ledger, &exchange, limit, min_edge).await,
        Command::Report => report(&ledger),
    }
}

async fn discover(
    ledger: &Ledger,
    exchange: &PolymarketIntl,
    limit: usize,
) -> anyhow::Result<()> {
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

async fn record(ledger: &Ledger, exchange: &PolymarketIntl, limit: usize) -> anyhow::Result<()> {
    let markets = ledger.markets_with_tokens(limit)?;
    if markets.is_empty() {
        warn!("no markets in ledger; run `discover` first");
        return Ok(());
    }
    let mut recorded = 0;
    for market in &markets {
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
    info!(recorded, markets = markets.len(), "orderbook recording complete");
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
    limit: usize,
    min_edge: f64,
) -> anyhow::Result<()> {
    let policy_config = PolicyConfig {
        min_edge,
        ..PolicyConfig::default()
    };
    let broker = PaperBroker::new(PaperBrokerConfig {
        fee_rate: policy_config.fee_rate,
        allow_partial_fills: false,
    });
    let mode = ExecutionMode::Paper;

    let markets = ledger.markets_with_tokens(limit)?;
    let bankroll = ledger.cash()?;
    info!(bankroll, markets = markets.len(), "starting paper trading pass");

    for market in &markets {
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

        let existing_cost = ledger.position_cost(&market.market_id)?;
        let decision = policy::evaluate(
            &policy_config,
            market,
            &yes_book,
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
            match exchange.get_orderbook(&order.token_id).await {
                Ok(book) => {
                    ledger.insert_snapshot(exchange.venue(), &market.market_id, &book)?;
                    book
                }
                Err(error) => {
                    warn!(market = %market.slug, %error, "skipping, no execution book");
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
        println!("equity (liquidation): {:.2}", summary.cash + mtm_liquidation);
        println!("equity (midpoint):    {:.2}", summary.cash + mtm_mid);
    } else {
        println!("equity: incomplete marks; run `record` to refresh snapshots");
    }
    Ok(())
}
