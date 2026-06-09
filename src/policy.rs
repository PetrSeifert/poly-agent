use chrono::Utc;

use crate::types::{Forecast, Market, NewOrder, Outcome, OrderBook, OrderType, Side};

pub const POLICY_VERSION: &str = "taker-edge-v1";

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
            fee_rate: 0.0,
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

/// Deterministic gate between forecasts and order submission. The forecast
/// (eventually LLM-produced) never places orders directly; this function does.
pub fn evaluate(
    config: &PolicyConfig,
    market: &Market,
    yes_book: &OrderBook,
    forecast: &Forecast,
    bankroll: f64,
    existing_position_cost: f64,
) -> PolicyDecision {
    if let Some(close_time) = market.close_time {
        let hours_left = (close_time - Utc::now()).num_minutes() as f64 / 60.0;
        if hours_left < config.min_hours_to_close {
            return no_trade(format!(
                "only {hours_left:.1}h to close, minimum is {:.1}h",
                config.min_hours_to_close
            ));
        }
    }

    let (Some(best_bid), Some(best_ask)) = (yes_book.best_bid(), yes_book.best_ask()) else {
        return no_trade("missing best bid or ask".to_string());
    };
    let spread = best_ask - best_bid;
    if spread > config.max_spread {
        return no_trade(format!(
            "spread {spread:.3} exceeds max {:.3}",
            config.max_spread
        ));
    }

    let market_prob = (best_bid + best_ask) / 2.0;
    // Shrink toward the market until the agent has proven calibration.
    // Scaling by the model's own confidence means a confidence of 0
    // (e.g. "do not trade") collapses to the market price and produces no edge.
    let weight = (config.forecast_weight * forecast.confidence.clamp(0.0, 1.0)).clamp(0.0, 1.0);
    let fair_prob = weight * forecast.fair_prob_yes + (1.0 - weight) * market_prob;

    let yes_fee = config.fee_rate * best_ask * (1.0 - best_ask);
    let yes_edge = fair_prob - best_ask - yes_fee;

    // Buying NO at (1 - bid) is equivalent to selling YES at the bid.
    let no_price = 1.0 - best_bid;
    let no_fee = config.fee_rate * no_price * (1.0 - no_price);
    let no_edge = (1.0 - fair_prob) - no_price - no_fee;

    let (outcome, token_id, price, edge) = if yes_edge >= no_edge {
        let Some(token) = market.yes_token_id.clone() else {
            return no_trade("missing yes token id".to_string());
        };
        (Outcome::Yes, token, best_ask, yes_edge)
    } else {
        let Some(token) = market.no_token_id.clone() else {
            return no_trade("missing no token id".to_string());
        };
        (Outcome::No, token, no_price, no_edge)
    };

    if edge < config.min_edge {
        return no_trade(format!(
            "best edge {edge:.4} below minimum {:.4}",
            config.min_edge
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
