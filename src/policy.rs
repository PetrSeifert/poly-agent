use chrono::Utc;

use crate::triage;
use crate::types::{Forecast, Market, NewOrder, OrderBook, OrderType, Outcome, Side};

pub const POLICY_VERSION: &str = "taker-edge-v3-dual-book";

#[derive(Debug, Clone)]
pub struct PolicyConfig {
    /// Minimum edge per share after fees, in probability points.
    pub min_edge: f64,
    /// Maximum acceptable bid/ask spread.
    pub max_spread: f64,
    /// Maximum fraction of bankroll committed to a single market.
    pub max_position_fraction: f64,
    /// Taker fee rate used in the edge calculation.
    pub fee_rate: f64,
    /// Minimum hours until market close.
    pub min_hours_to_close: f64,
    /// Shrinkage weight toward the market price: p = w * p_agent + (1 - w) * p_market.
    pub forecast_weight: f64,
    /// Maximum number of shares per order.
    pub max_order_shares: f64,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            min_edge: 0.05,
            max_spread: 0.04,
            max_position_fraction: 0.02,
            // Conservative Polymarket-style taker fee assumption; markets
            // without fees will look slightly worse than reality, which is
            // the safe direction for paper-trading validation.
            fee_rate: 0.05,
            min_hours_to_close: 24.0,
            forecast_weight: 0.25,
            max_order_shares: 100.0,
        }
    }
}

#[derive(Debug)]
pub enum PolicyDecision {
    Trade { order: NewOrder, edge: f64 },
    NoTrade { reason: String },
}

/// A side of the market the policy could buy, priced from the actual book of
/// the token that would be bought.
struct SideQuote {
    outcome: Outcome,
    token_id: String,
    ask: f64,
    edge: f64,
}

/// Price one side from its own orderbook. Returns Err with the reason the
/// side is untradeable (missing book, missing quote, wide spread).
fn quote_side(
    config: &PolicyConfig,
    outcome: Outcome,
    token_id: Option<&String>,
    book: Option<&OrderBook>,
    fair_prob_for_side: f64,
) -> Result<SideQuote, String> {
    let token_id = token_id.ok_or_else(|| format!("missing {} token id", outcome.as_str()))?;
    let book = book.ok_or_else(|| format!("missing {} orderbook", outcome.as_str()))?;
    let (Some(best_bid), Some(best_ask)) = (book.best_bid(), book.best_ask()) else {
        return Err(format!("missing {} best bid or ask", outcome.as_str()));
    };
    let spread = best_ask - best_bid;
    if spread > config.max_spread {
        return Err(format!(
            "{} spread {spread:.3} exceeds max {:.3}",
            outcome.as_str(),
            config.max_spread
        ));
    }
    let fee = config.fee_rate * best_ask * (1.0 - best_ask);
    Ok(SideQuote {
        outcome,
        token_id: token_id.clone(),
        ask: best_ask,
        edge: fair_prob_for_side - best_ask - fee,
    })
}

/// Deterministic gate between forecasts and order submission. The forecast
/// (eventually LLM-produced) never places orders directly; this function does.
/// Both books are required so each side is priced from the book it would
/// actually execute against, not a synthetic reciprocal of the other side.
pub fn evaluate(
    config: &PolicyConfig,
    market: &Market,
    yes_book: &OrderBook,
    no_book: Option<&OrderBook>,
    forecast: &Forecast,
    bankroll: f64,
    existing_position_cost: f64,
) -> PolicyDecision {
    let profile = triage::profile(market);
    if profile.trade_blocked {
        return no_trade(format!(
            "category '{}' is on the avoid list (narrative-heavy, no repeatable edge)",
            profile.category.as_str()
        ));
    }
    if profile.rules_clarity == triage::RulesClarity::Missing {
        return no_trade("no resolution rules text; clarity gate failed".to_string());
    }

    if let Some(close_time) = market.close_time {
        let hours_left = (close_time - Utc::now()).num_minutes() as f64 / 60.0;
        if hours_left < config.min_hours_to_close {
            return no_trade(format!(
                "only {hours_left:.1}h to close, minimum is {:.1}h",
                config.min_hours_to_close
            ));
        }
    }

    let (Some(yes_bid), Some(yes_ask)) = (yes_book.best_bid(), yes_book.best_ask()) else {
        return no_trade("missing yes best bid or ask".to_string());
    };

    let market_prob = (yes_bid + yes_ask) / 2.0;
    // Shrink toward the market until the agent has proven calibration.
    // Scaling by the model's own confidence means a confidence of 0
    // (e.g. "do not trade") collapses to the market price and produces no
    // edge. The triage trust factor additionally shrinks domains where the
    // agent has no repeatable advantage.
    let weight =
        (config.forecast_weight * profile.forecast_trust * forecast.confidence.clamp(0.0, 1.0))
            .clamp(0.0, 1.0);
    let fair_prob = weight * forecast.fair_prob_yes + (1.0 - weight) * market_prob;

    let yes_quote = quote_side(
        config,
        Outcome::Yes,
        market.yes_token_id.as_ref(),
        Some(yes_book),
        fair_prob,
    );
    let no_quote = quote_side(
        config,
        Outcome::No,
        market.no_token_id.as_ref(),
        no_book,
        1.0 - fair_prob,
    );

    let best = match (yes_quote, no_quote) {
        (Ok(yes), Ok(no)) => {
            if yes.edge >= no.edge {
                yes
            } else {
                no
            }
        }
        (Ok(yes), Err(_)) => yes,
        (Err(_), Ok(no)) => no,
        (Err(yes_reason), Err(no_reason)) => {
            return no_trade(format!("no tradeable side: {yes_reason}; {no_reason}"));
        }
    };
    let SideQuote {
        outcome,
        token_id,
        ask: price,
        edge,
    } = best;

    // Weaker categories and thin rules must clear a higher bar.
    let required_edge = config.min_edge * profile.min_edge_multiplier;
    if edge < required_edge {
        return no_trade(format!(
            "best edge {edge:.4} below minimum {required_edge:.4} (base {:.4} x {:.2} for category '{}', rules {:?})",
            config.min_edge,
            profile.min_edge_multiplier,
            profile.category.as_str(),
            profile.rules_clarity,
        ));
    }

    let max_cost = bankroll * config.max_position_fraction - existing_position_cost;
    if max_cost <= 0.0 {
        return no_trade("market already at maximum position size".to_string());
    }
    let size = (max_cost / price).min(config.max_order_shares).floor();
    if size < 1.0 {
        return no_trade("position budget too small for one share".to_string());
    }

    PolicyDecision::Trade {
        order: NewOrder {
            market_id: market.market_id.clone(),
            token_id,
            outcome,
            side: Side::Buy,
            order_type: OrderType::Market,
            limit_price: price,
            size,
        },
        edge,
    }
}

fn no_trade(reason: String) -> PolicyDecision {
    PolicyDecision::NoTrade { reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PriceLevel, Venue};
    use chrono::Duration;

    const DETAILED_RULES: &str = "This market resolves YES if the official closing price \
        reported by the designated source exceeds the threshold at the stated time, \
        otherwise it resolves NO. See the linked source for details.";

    fn market() -> Market {
        Market {
            venue: Venue::PolymarketInternational,
            event_id: None,
            market_id: "m1".to_string(),
            condition_id: None,
            slug: String::new(),
            question: "Will Bitcoin close above $100,000 on June 30?".to_string(),
            resolution_rules: Some(DETAILED_RULES.to_string()),
            close_time: Some(Utc::now() + Duration::days(30)),
            active: true,
            closed: false,
            neg_risk: false,
            yes_token_id: Some("yes-token".to_string()),
            no_token_id: Some("no-token".to_string()),
            volume_24hr: Some(10_000.0),
            liquidity: Some(20_000.0),
        }
    }

    fn book(token_id: &str, bid: f64, ask: f64) -> OrderBook {
        OrderBook {
            token_id: token_id.to_string(),
            ts: Utc::now(),
            condition_id: None,
            exchange_ts: None,
            hash: None,
            neg_risk: None,
            bids: vec![PriceLevel {
                price: bid,
                size: 1000.0,
            }],
            asks: vec![PriceLevel {
                price: ask,
                size: 1000.0,
            }],
            tick_size: Some(0.01),
            min_order_size: Some(5.0),
        }
    }

    fn forecast(fair_prob_yes: f64, confidence: f64) -> Forecast {
        Forecast {
            market_id: "m1".to_string(),
            fair_prob_yes,
            confidence,
            model_version: "test".to_string(),
            rationale: serde_json::Value::Null,
        }
    }

    fn config() -> PolicyConfig {
        PolicyConfig {
            fee_rate: 0.05,
            ..PolicyConfig::default()
        }
    }

    #[test]
    fn no_side_uses_actual_no_ask_not_synthetic_reciprocal() {
        // YES bid 0.60 would imply a synthetic NO price of 0.40, but the
        // actual NO book asks 0.48; the real edge is ~0 so no trade.
        let yes_book = book("yes-token", 0.60, 0.62);
        let no_book = book("no-token", 0.46, 0.48);
        let decision = evaluate(
            &config(),
            &market(),
            &yes_book,
            Some(&no_book),
            &forecast(0.20, 1.0),
            10_000.0,
            0.0,
        );
        assert!(
            matches!(decision, PolicyDecision::NoTrade { .. }),
            "synthetic 1 - bid pricing would have produced a false edge"
        );
    }

    #[test]
    fn no_side_trades_when_actual_no_ask_is_cheap() {
        let yes_book = book("yes-token", 0.60, 0.62);
        let no_book = book("no-token", 0.38, 0.40);
        let decision = evaluate(
            &config(),
            &market(),
            &yes_book,
            Some(&no_book),
            &forecast(0.20, 1.0),
            10_000.0,
            0.0,
        );
        match decision {
            PolicyDecision::Trade { order, .. } => {
                assert_eq!(order.outcome, Outcome::No);
                assert_eq!(order.token_id, "no-token");
                assert!((order.limit_price - 0.40).abs() < 1e-9);
            }
            PolicyDecision::NoTrade { reason } => panic!("expected NO trade, got: {reason}"),
        }
    }

    #[test]
    fn fee_rate_reduces_edge() {
        let yes_book = book("yes-token", 0.60, 0.62);
        let no_book = book("no-token", 0.38, 0.40);
        // min_edge 0 so both configs trade and the edges are comparable.
        let cheap = PolicyConfig {
            fee_rate: 0.0,
            min_edge: 0.0,
            ..PolicyConfig::default()
        };
        let expensive = PolicyConfig {
            fee_rate: 0.2,
            min_edge: 0.0,
            ..PolicyConfig::default()
        };
        let edge_without_fees = match evaluate(
            &cheap,
            &market(),
            &yes_book,
            Some(&no_book),
            &forecast(0.20, 1.0),
            10_000.0,
            0.0,
        ) {
            PolicyDecision::Trade { edge, .. } => edge,
            PolicyDecision::NoTrade { reason } => panic!("expected trade, got: {reason}"),
        };
        match evaluate(
            &expensive,
            &market(),
            &yes_book,
            Some(&no_book),
            &forecast(0.20, 1.0),
            10_000.0,
            0.0,
        ) {
            PolicyDecision::Trade { edge, .. } => {
                assert!(edge < edge_without_fees, "fees must reduce the edge");
            }
            PolicyDecision::NoTrade { reason } => panic!("expected trade, got: {reason}"),
        }
    }

    #[test]
    fn wide_no_spread_blocks_no_trades() {
        let yes_book = book("yes-token", 0.60, 0.62);
        let no_book = book("no-token", 0.20, 0.40);
        let decision = evaluate(
            &config(),
            &market(),
            &yes_book,
            Some(&no_book),
            &forecast(0.20, 1.0),
            10_000.0,
            0.0,
        );
        assert!(matches!(decision, PolicyDecision::NoTrade { .. }));
    }

    #[test]
    fn existing_position_blocks_additional_buys() {
        let yes_book = book("yes-token", 0.60, 0.62);
        let no_book = book("no-token", 0.38, 0.40);
        // max_position_fraction 0.02 * 10_000 = 200 budget, fully used.
        let decision = evaluate(
            &config(),
            &market(),
            &yes_book,
            Some(&no_book),
            &forecast(0.20, 1.0),
            10_000.0,
            200.0,
        );
        match decision {
            PolicyDecision::NoTrade { reason } => {
                assert!(reason.contains("maximum position size"), "got: {reason}");
            }
            PolicyDecision::Trade { .. } => panic!("expected position cap to block the trade"),
        }
    }

    #[test]
    fn market_closing_soon_is_rejected() {
        let mut market = market();
        market.close_time = Some(Utc::now() + Duration::hours(1));
        let yes_book = book("yes-token", 0.60, 0.62);
        let no_book = book("no-token", 0.38, 0.40);
        let decision = evaluate(
            &config(),
            &market,
            &yes_book,
            Some(&no_book),
            &forecast(0.20, 1.0),
            10_000.0,
            0.0,
        );
        match decision {
            PolicyDecision::NoTrade { reason } => {
                assert!(reason.contains("to close"), "got: {reason}");
            }
            PolicyDecision::Trade { .. } => panic!("expected close-time gate to block the trade"),
        }
    }

    #[test]
    fn missing_rules_block_trading() {
        let mut market = market();
        market.resolution_rules = None;
        let yes_book = book("yes-token", 0.60, 0.62);
        let no_book = book("no-token", 0.38, 0.40);
        let decision = evaluate(
            &config(),
            &market,
            &yes_book,
            Some(&no_book),
            &forecast(0.20, 1.0),
            10_000.0,
            0.0,
        );
        assert!(matches!(decision, PolicyDecision::NoTrade { .. }));
    }

    #[test]
    fn zero_confidence_forecast_produces_no_trade() {
        let yes_book = book("yes-token", 0.60, 0.62);
        let no_book = book("no-token", 0.38, 0.40);
        let decision = evaluate(
            &config(),
            &market(),
            &yes_book,
            Some(&no_book),
            &forecast(0.20, 0.0),
            10_000.0,
            0.0,
        );
        assert!(
            matches!(decision, PolicyDecision::NoTrade { .. }),
            "confidence 0 must collapse to the market price and produce no edge"
        );
    }
}
