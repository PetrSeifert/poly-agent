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
use crate::ledger::{Ledger, TokenBookStatusKind, TokenSuppression};
use crate::policy::{POLICY_VERSION, PolicyConfig, PolicyDecision};
use crate::types::{ExecutionMode, Market, OrderBook, OrderStatus, Outcome};

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
        /// Cap order budget to about $2 and label orders as diagnostics.
        #[arg(long)]
        diagnostic_small_orders: bool,
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
        /// Cap order budget to about $2 and label orders as diagnostics.
        #[arg(long)]
        diagnostic_small_orders: bool,
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
            diagnostic_small_orders,
        } => {
            let universe = select_universe(&ledger, limit)?;
            trade(
                &ledger,
                &exchange,
                &universe,
                min_edge,
                fee_rate,
                diagnostic_small_orders,
            )
            .await
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
            diagnostic_small_orders,
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
                    diagnostic_small_orders,
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
    diagnostic_small_orders: bool,
}

/// Multi-outcome events (e.g. "who wins the World Cup") appear as dozens of
/// near-duplicate markets; cap how many can occupy the tradeable universe.
const MAX_MARKETS_PER_EVENT: usize = 3;
/// Discover this many times more markets than the universe size, so the
/// triage filter has something to choose from beyond the top-volume event.
const DISCOVERY_DEPTH_FACTOR: usize = 10;
/// Fetch books for a wider metadata-filtered candidate set, then keep only
/// markets that are actually executable.
const EXECUTABLE_CANDIDATE_FACTOR: usize = 5;
/// Taker edge is measured in absolute probability points, so books priced
/// near 0 or 1 are poor use of forecast budget in non-arbitrage mode.
const MIN_EXECUTABLE_MIDPOINT: f64 = 0.10;
const MAX_EXECUTABLE_MIDPOINT: f64 = 0.90;
const EXECUTABLE_DEPTH_LEVELS: usize = 3;
const MIN_EXECUTABLE_ASK_DEPTH: f64 = 10.0;
const NOT_FOUND_SUPPRESSION_HOURS: i64 = 12;
const EMPTY_BOOK_SUPPRESSION_MINUTES: i64 = 45;
const ERROR_SUPPRESSION_MINUTES: i64 = 5;

#[derive(Debug, Clone)]
struct SideCandidate {
    outcome: Outcome,
    token_id: String,
    ask: f64,
    spread: f64,
    ask_depth: f64,
}

#[derive(Debug, Clone)]
struct ExecutableMarket {
    market: Market,
    yes_book: OrderBook,
    no_book: OrderBook,
    best_executable_side: SideCandidate,
    midpoint: f64,
    spread: f64,
    depth_score: f64,
    score: f64,
}

#[derive(Debug, Clone, Copy)]
enum MarketRejectReason {
    MissingToken,
    Suppressed,
    BookNotFound,
    EmptyBook,
    ExtremeMidpoint,
    WideSpread,
    InsufficientDepth,
    ClosingSoon,
    BookError,
}

impl MarketRejectReason {
    fn as_str(&self) -> &'static str {
        match self {
            MarketRejectReason::MissingToken => "missing_token",
            MarketRejectReason::Suppressed => "suppressed",
            MarketRejectReason::BookNotFound => "book_404",
            MarketRejectReason::EmptyBook => "empty_book",
            MarketRejectReason::ExtremeMidpoint => "extreme_midpoint",
            MarketRejectReason::WideSpread => "wide_spread",
            MarketRejectReason::InsufficientDepth => "insufficient_depth",
            MarketRejectReason::ClosingSoon => "closing_soon",
            MarketRejectReason::BookError => "book_error",
        }
    }
}

#[derive(Debug, Default)]
struct ExecutableUniverseSummary {
    total_candidates: usize,
    executable_before_cap: usize,
    selected: usize,
    missing_token: usize,
    suppressed: usize,
    book_not_found: usize,
    empty_book: usize,
    extreme_midpoint: usize,
    wide_spread: usize,
    insufficient_depth: usize,
    closing_soon: usize,
    book_error: usize,
}

impl ExecutableUniverseSummary {
    fn reject(&mut self, reason: MarketRejectReason) {
        match reason {
            MarketRejectReason::MissingToken => self.missing_token += 1,
            MarketRejectReason::Suppressed => self.suppressed += 1,
            MarketRejectReason::BookNotFound => self.book_not_found += 1,
            MarketRejectReason::EmptyBook => self.empty_book += 1,
            MarketRejectReason::ExtremeMidpoint => self.extreme_midpoint += 1,
            MarketRejectReason::WideSpread => self.wide_spread += 1,
            MarketRejectReason::InsufficientDepth => self.insufficient_depth += 1,
            MarketRejectReason::ClosingSoon => self.closing_soon += 1,
            MarketRejectReason::BookError => self.book_error += 1,
        }
    }
}

#[derive(Debug, Default)]
struct ForecastSelectionSummary {
    universe: usize,
    executable: usize,
    stale: usize,
    selected: usize,
    skipped_fresh: usize,
}

/// Build the tradeable universe: volume-ordered markets filtered down to
/// categories where the agent has repeatable edge, with rules text present
/// and near-duplicate outcomes of the same event capped.
fn select_universe(ledger: &Ledger, limit: usize) -> anyhow::Result<Vec<Market>> {
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

async fn build_executable_universe(
    ledger: &Ledger,
    exchange: &PolymarketIntl,
    markets: &[Market],
    limit: usize,
    policy_config: &PolicyConfig,
    forecast_refresh_age: chrono::Duration,
) -> anyhow::Result<(Vec<ExecutableMarket>, ExecutableUniverseSummary)> {
    let mut summary = ExecutableUniverseSummary {
        total_candidates: markets.len(),
        ..ExecutableUniverseSummary::default()
    };
    let min_close = chrono::Utc::now()
        + chrono::Duration::seconds((policy_config.min_hours_to_close * 3600.0) as i64);
    let mut executable = Vec::new();

    for market in markets {
        if let Some(close_time) = market.close_time
            && close_time < min_close
        {
            summary.reject(MarketRejectReason::ClosingSoon);
            info!(market = %market.slug, reason = MarketRejectReason::ClosingSoon.as_str(), "market rejected from executable universe");
            continue;
        }

        let (Some(yes_token), Some(no_token)) = (&market.yes_token_id, &market.no_token_id) else {
            summary.reject(MarketRejectReason::MissingToken);
            info!(market = %market.slug, reason = MarketRejectReason::MissingToken.as_str(), "market rejected from executable universe");
            continue;
        };

        let yes_book = match fetch_book_for_universe(ledger, exchange, market, yes_token).await {
            Ok(book) => book,
            Err(reason) => {
                summary.reject(reason);
                info!(market = %market.slug, token_id = %yes_token, reason = reason.as_str(), "market rejected from executable universe");
                continue;
            }
        };
        let no_book = match fetch_book_for_universe(ledger, exchange, market, no_token).await {
            Ok(book) => book,
            Err(reason) => {
                summary.reject(reason);
                info!(market = %market.slug, token_id = %no_token, reason = reason.as_str(), "market rejected from executable universe");
                continue;
            }
        };

        let (Some(midpoint), Some(yes_spread), Some(no_spread)) =
            (yes_book.midpoint(), yes_book.spread(), no_book.spread())
        else {
            summary.reject(MarketRejectReason::EmptyBook);
            info!(market = %market.slug, reason = MarketRejectReason::EmptyBook.as_str(), "market rejected from executable universe");
            continue;
        };
        if !(MIN_EXECUTABLE_MIDPOINT..=MAX_EXECUTABLE_MIDPOINT).contains(&midpoint) {
            summary.reject(MarketRejectReason::ExtremeMidpoint);
            info!(
                market = %market.slug,
                midpoint = format!("{midpoint:.3}"),
                reason = MarketRejectReason::ExtremeMidpoint.as_str(),
                "market rejected from executable universe"
            );
            continue;
        }

        let spread = yes_spread.max(no_spread);
        if spread > policy_config.max_spread {
            summary.reject(MarketRejectReason::WideSpread);
            info!(
                market = %market.slug,
                spread = format!("{spread:.3}"),
                max_spread = format!("{:.3}", policy_config.max_spread),
                reason = MarketRejectReason::WideSpread.as_str(),
                "market rejected from executable universe"
            );
            continue;
        }

        let yes_ask_depth = top_ask_depth(&yes_book, EXECUTABLE_DEPTH_LEVELS);
        let no_ask_depth = top_ask_depth(&no_book, EXECUTABLE_DEPTH_LEVELS);
        let depth_score = yes_ask_depth.min(no_ask_depth);
        if depth_score < MIN_EXECUTABLE_ASK_DEPTH {
            summary.reject(MarketRejectReason::InsufficientDepth);
            info!(
                market = %market.slug,
                depth_score = format!("{depth_score:.2}"),
                min_depth = format!("{MIN_EXECUTABLE_ASK_DEPTH:.2}"),
                reason = MarketRejectReason::InsufficientDepth.as_str(),
                "market rejected from executable universe"
            );
            continue;
        }

        let (Some(yes_side), Some(no_side)) = (
            side_candidate(
                Outcome::Yes,
                yes_token,
                &yes_book,
                yes_spread,
                yes_ask_depth,
            ),
            side_candidate(Outcome::No, no_token, &no_book, no_spread, no_ask_depth),
        ) else {
            summary.reject(MarketRejectReason::EmptyBook);
            info!(market = %market.slug, reason = MarketRejectReason::EmptyBook.as_str(), "market rejected from executable universe");
            continue;
        };
        let best_executable_side = best_side_candidate(yes_side, no_side);
        let profile = triage::profile(market);
        let forecast_age = ledger
            .forecast_age(&market.market_id, Some(llm::MODEL_VERSION_PREFIX))?
            .unwrap_or(chrono::Duration::days(3650));
        let score = executable_score(
            &profile,
            market,
            midpoint,
            spread,
            depth_score,
            forecast_age,
            forecast_refresh_age,
        );
        executable.push(ExecutableMarket {
            market: market.clone(),
            yes_book,
            no_book,
            best_executable_side,
            midpoint,
            spread,
            depth_score,
            score,
        });
    }

    summary.executable_before_cap = executable.len();
    executable.sort_by(|a, b| b.score.total_cmp(&a.score));
    executable.truncate(limit);
    summary.selected = executable.len();
    Ok((executable, summary))
}

async fn fetch_book_for_universe(
    ledger: &Ledger,
    exchange: &PolymarketIntl,
    market: &Market,
    token_id: &str,
) -> Result<OrderBook, MarketRejectReason> {
    match ledger.token_suppression(token_id) {
        Ok(Some(suppression)) => {
            log_token_suppression(market, token_id, &suppression);
            return Err(MarketRejectReason::Suppressed);
        }
        Ok(None) => {}
        Err(error) => {
            warn!(market = %market.slug, %token_id, %error, "failed to read token suppression status");
        }
    }

    match exchange.get_orderbook(token_id).await {
        Ok(book) => {
            if book.bids.is_empty() || book.asks.is_empty() {
                if let Err(error) = ledger.record_token_book_status(
                    &market.market_id,
                    token_id,
                    TokenBookStatusKind::Empty,
                    Some("book missing bids or asks"),
                    Some(chrono::Duration::minutes(EMPTY_BOOK_SUPPRESSION_MINUTES)),
                ) {
                    warn!(market = %market.slug, %token_id, %error, "failed to record empty book status");
                }
                Err(MarketRejectReason::EmptyBook)
            } else {
                if let Err(error) = ledger.record_token_book_status(
                    &market.market_id,
                    token_id,
                    TokenBookStatusKind::Ok,
                    None,
                    None,
                ) {
                    warn!(market = %market.slug, %token_id, %error, "failed to record ok book status");
                }
                if let Err(error) =
                    ledger.insert_snapshot(exchange.venue(), &market.market_id, &book)
                {
                    warn!(market = %market.slug, %token_id, %error, "failed to insert orderbook snapshot");
                }
                Ok(book)
            }
        }
        Err(error) => {
            let (status, suppress_for, reason) =
                if exchange::clob_book_status(&error) == Some(reqwest::StatusCode::NOT_FOUND) {
                    (
                        TokenBookStatusKind::NotFound,
                        chrono::Duration::hours(NOT_FOUND_SUPPRESSION_HOURS),
                        MarketRejectReason::BookNotFound,
                    )
                } else {
                    (
                        TokenBookStatusKind::Error,
                        chrono::Duration::minutes(ERROR_SUPPRESSION_MINUTES),
                        MarketRejectReason::BookError,
                    )
                };
            let error_text = error.to_string();
            if let Err(status_error) = ledger.record_token_book_status(
                &market.market_id,
                token_id,
                status,
                Some(&error_text),
                Some(suppress_for),
            ) {
                warn!(market = %market.slug, %token_id, %status_error, "failed to record book error status");
            }
            warn!(market = %market.slug, %token_id, %error, "book fetch failed");
            Err(reason)
        }
    }
}

fn log_token_suppression(market: &Market, token_id: &str, suppression: &TokenSuppression) {
    info!(
        market = %market.slug,
        %token_id,
        status = %suppression.last_status,
        suppress_until = %suppression.suppress_until.to_rfc3339(),
        "skipping suppressed token book"
    );
}

fn top_ask_depth(book: &OrderBook, levels: usize) -> f64 {
    book.asks.iter().take(levels).map(|level| level.size).sum()
}

fn side_candidate(
    outcome: Outcome,
    token_id: &str,
    book: &OrderBook,
    spread: f64,
    ask_depth: f64,
) -> Option<SideCandidate> {
    Some(SideCandidate {
        outcome,
        token_id: token_id.to_string(),
        ask: book.best_ask()?,
        spread,
        ask_depth,
    })
}

fn best_side_candidate(yes: SideCandidate, no: SideCandidate) -> SideCandidate {
    let yes_score = side_actionability_score(yes.spread, yes.ask_depth, yes.ask);
    let no_score = side_actionability_score(no.spread, no.ask_depth, no.ask);
    if yes_score >= no_score { yes } else { no }
}

fn side_actionability_score(spread: f64, ask_depth: f64, ask: f64) -> f64 {
    let spread_score = (1.0 - spread / PolicyConfig::default().max_spread).clamp(0.0, 1.0);
    let depth_score = (ask_depth / 100.0).clamp(0.0, 1.0);
    let price_score = midpoint_preference_score(ask);
    0.45 * spread_score + 0.35 * depth_score + 0.20 * price_score
}

fn executable_score(
    profile: &triage::TriageProfile,
    market: &Market,
    midpoint: f64,
    spread: f64,
    depth_score: f64,
    forecast_age: chrono::Duration,
    forecast_refresh_age: chrono::Duration,
) -> f64 {
    let liquidity_score = (market.liquidity.unwrap_or(0.0) / 50_000.0).clamp(0.0, 1.0);
    let spread_score = (1.0 - spread / PolicyConfig::default().max_spread).clamp(0.0, 1.0);
    let midpoint_score = midpoint_preference_score(midpoint);
    let domain_score = profile.forecast_priority.clamp(0.0, 1.0);
    let time_to_close_score = market
        .close_time
        .map(|close_time| {
            let hours_left = (close_time - chrono::Utc::now()).num_minutes() as f64 / 60.0;
            (hours_left / (24.0 * 14.0)).clamp(0.0, 1.0)
        })
        .unwrap_or(0.5);
    let staleness_score = if forecast_age > forecast_refresh_age {
        1.0
    } else {
        (forecast_age.num_minutes() as f64 / forecast_refresh_age.num_minutes().max(1) as f64)
            .clamp(0.0, 1.0)
    };
    let depth_score = (depth_score / 100.0).clamp(0.0, 1.0);

    0.20 * liquidity_score
        + 0.20 * spread_score
        + 0.20 * midpoint_score
        + 0.15 * domain_score
        + 0.10 * time_to_close_score
        + 0.10 * depth_score
        + 0.05 * staleness_score
}

fn midpoint_preference_score(price: f64) -> f64 {
    (1.0 - ((price - 0.5).abs() / 0.4)).clamp(0.0, 1.0)
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
        diagnostic_small_orders = config.diagnostic_small_orders,
        "starting simulation run"
    );

    loop {
        cycle += 1;
        let cycle_started = std::time::Instant::now();
        info!(cycle, "cycle start");

        let discovery_limit = config.markets * EXECUTABLE_CANDIDATE_FACTOR * DISCOVERY_DEPTH_FACTOR;
        if let Err(error) = discover(ledger, exchange, discovery_limit).await {
            error!(%error, "discovery failed, continuing with stored markets");
        }
        let candidate_limit = config.markets * EXECUTABLE_CANDIDATE_FACTOR;
        let candidate_universe = select_universe(ledger, candidate_limit)?;
        info!(
            size = candidate_universe.len(),
            desired_executable = config.markets,
            "candidate universe selected (triage-filtered, event-capped)"
        );

        let policy_config = PolicyConfig {
            min_edge: config.min_edge,
            fee_rate: config.fee_rate,
            ..PolicyConfig::default()
        };
        let (executable_universe, executable_summary) = build_executable_universe(
            ledger,
            exchange,
            &candidate_universe,
            config.markets,
            &policy_config,
            refresh_age,
        )
        .await?;
        info!(
            total_candidates = executable_summary.total_candidates,
            executable_before_cap = executable_summary.executable_before_cap,
            executable = executable_summary.selected,
            skipped_missing_token = executable_summary.missing_token,
            skipped_suppressed = executable_summary.suppressed,
            skipped_404 = executable_summary.book_not_found,
            skipped_empty = executable_summary.empty_book,
            skipped_extreme = executable_summary.extreme_midpoint,
            skipped_wide_spread = executable_summary.wide_spread,
            skipped_insufficient_depth = executable_summary.insufficient_depth,
            skipped_closing_soon = executable_summary.closing_soon,
            skipped_error = executable_summary.book_error,
            "executable universe summary"
        );

        let mut forecast_summary = ForecastSelectionSummary {
            universe: candidate_universe.len(),
            executable: executable_universe.len(),
            ..ForecastSelectionSummary::default()
        };
        let mut stale: Vec<(chrono::Duration, &ExecutableMarket)> = Vec::new();
        for executable in &executable_universe {
            // Stub forecasts (confidence 0) must not satisfy the refresh
            // window, or the agent never spends its LLM budget.
            let age = ledger
                .forecast_age(
                    &executable.market.market_id,
                    Some(llm::MODEL_VERSION_PREFIX),
                )?
                .unwrap_or(chrono::Duration::days(3650));
            if age > refresh_age {
                stale.push((age, executable));
            } else {
                forecast_summary.skipped_fresh += 1;
            }
        }
        forecast_summary.stale = stale.len();
        stale.sort_by(|(_, left), (_, right)| right.score.total_cmp(&left.score));

        let mut candidates = Vec::new();
        for (_age, executable) in stale {
            if candidates.len() >= config.max_llm_calls {
                break;
            }
            let no_midpoint = executable.no_book.midpoint().unwrap_or_default();
            info!(
                market = %executable.market.slug,
                category = triage::classify(&executable.market).as_str(),
                score = format!("{:.3}", executable.score),
                yes_midpoint = format!("{:.3}", executable.midpoint),
                no_midpoint = format!("{no_midpoint:.3}"),
                max_spread = format!("{:.3}", executable.spread),
                depth_score = format!("{:.1}", executable.depth_score),
                best_side = executable.best_executable_side.outcome.as_str(),
                best_side_token = %executable.best_executable_side.token_id,
                best_side_ask = format!("{:.3}", executable.best_executable_side.ask),
                best_side_spread = format!("{:.3}", executable.best_executable_side.spread),
                best_side_depth = format!("{:.1}", executable.best_executable_side.ask_depth),
                "selected for forecast"
            );
            candidates.push((&executable.market, &executable.yes_book));
        }
        forecast_summary.selected = candidates.len();
        info!(
            universe = forecast_summary.universe,
            executable = forecast_summary.executable,
            stale = forecast_summary.stale,
            selected = forecast_summary.selected,
            skipped_fresh = forecast_summary.skipped_fresh,
            skipped_missing_token = executable_summary.missing_token,
            skipped_suppressed = executable_summary.suppressed,
            skipped_404 = executable_summary.book_not_found,
            skipped_empty = executable_summary.empty_book,
            skipped_extreme = executable_summary.extreme_midpoint,
            skipped_wide_spread = executable_summary.wide_spread,
            skipped_insufficient_depth = executable_summary.insufficient_depth,
            skipped_closing_soon = executable_summary.closing_soon,
            skipped_error = executable_summary.book_error,
            "forecast budget summary"
        );

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

        let trade_markets: Vec<Market> = executable_universe
            .iter()
            .map(|executable| executable.market.clone())
            .collect();
        if let Err(error) = trade(
            ledger,
            exchange,
            &trade_markets,
            config.min_edge,
            config.fee_rate,
            config.diagnostic_small_orders,
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
    markets: &[Market],
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
            match fetch_book_for_universe(ledger, exchange, market, token_id).await {
                Ok(_) => recorded += 1,
                Err(reason) => {
                    warn!(market = %market.slug, %token_id, reason = reason.as_str(), "snapshot skipped");
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
        let book = match fetch_book_for_universe(ledger, exchange, market, yes_token).await {
            Ok(book) => book,
            Err(reason) => {
                warn!(market = %market.slug, reason = reason.as_str(), "skipping forecast, no book");
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
    markets: &[Market],
    min_edge: f64,
    fee_rate: f64,
    diagnostic_small_orders: bool,
) -> anyhow::Result<()> {
    let mut policy_config = PolicyConfig {
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
    if diagnostic_small_orders && bankroll > 0.0 {
        policy_config.max_position_fraction =
            policy_config.max_position_fraction.min(2.0 / bankroll);
    }
    info!(
        bankroll,
        markets = markets.len(),
        diagnostic_small_orders,
        max_position_fraction = policy_config.max_position_fraction,
        "starting paper trading pass"
    );

    for market in markets {
        let Some(forecast) = ledger.latest_forecast(&market.market_id)? else {
            continue;
        };
        let Some(yes_token) = &market.yes_token_id else {
            continue;
        };
        let yes_book = match fetch_book_for_universe(ledger, exchange, market, yes_token).await {
            Ok(book) => book,
            Err(reason) => {
                warn!(market = %market.slug, reason = reason.as_str(), "skipping, no yes book");
                continue;
            }
        };

        // Fetch the NO book up front so the policy prices the NO side from
        // the book it would actually execute against, not 1 - YES bid.
        let no_book = match &market.no_token_id {
            Some(no_token) => {
                match fetch_book_for_universe(ledger, exchange, market, no_token).await {
                    Ok(book) => Some(book),
                    Err(reason) => {
                        warn!(market = %market.slug, reason = reason.as_str(), "no NO book; evaluating YES side only");
                        None
                    }
                }
            }
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
        let policy_version = if diagnostic_small_orders {
            "taker-edge-v3-dual-book-diagnostic"
        } else {
            POLICY_VERSION
        };
        let order_id = ledger.insert_order(
            &order,
            mode,
            result.status,
            result.reject_reason.as_deref(),
            policy_version,
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
