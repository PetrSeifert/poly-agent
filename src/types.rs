use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Venue {
    PolymarketInternational,
    PolymarketUs,
}

impl Venue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Venue::PolymarketInternational => "polymarket_intl",
            Venue::PolymarketUs => "polymarket_us",
        }
    }
}

/// Staged execution modes. Only `ReadOnly` and `Paper` are wired up;
/// the live modes exist so the type system forces an explicit decision later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMode {
    ReadOnly,
    Paper,
    ShadowLive,
    HumanConfirmedLive,
    CappedAutoLive,
}

impl ExecutionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionMode::ReadOnly => "read_only",
            ExecutionMode::Paper => "paper",
            ExecutionMode::ShadowLive => "shadow_live",
            ExecutionMode::HumanConfirmedLive => "human_confirmed_live",
            ExecutionMode::CappedAutoLive => "capped_auto_live",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Yes,
    No,
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Yes => "yes",
            Outcome::No => "no",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub venue: Venue,
    pub event_id: Option<String>,
    pub market_id: String,
    pub slug: String,
    pub question: String,
    pub resolution_rules: Option<String>,
    pub close_time: Option<DateTime<Utc>>,
    pub active: bool,
    pub closed: bool,
    pub neg_risk: bool,
    pub yes_token_id: Option<String>,
    pub no_token_id: Option<String>,
    pub volume_24hr: Option<f64>,
    pub liquidity: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub token_id: String,
    pub ts: DateTime<Utc>,
    /// Sorted best-first (descending price).
    pub bids: Vec<PriceLevel>,
    /// Sorted best-first (ascending price).
    pub asks: Vec<PriceLevel>,
    pub tick_size: Option<f64>,
    pub min_order_size: Option<f64>,
}

impl OrderBook {
    pub fn best_bid(&self) -> Option<f64> {
        self.bids.first().map(|level| level.price)
    }

    pub fn best_ask(&self) -> Option<f64> {
        self.asks.first().map(|level| level.price)
    }

    pub fn midpoint(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some((bid + ask) / 2.0),
            _ => None,
        }
    }

    pub fn spread(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some(ask - bid),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit,
}

impl OrderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderType::Market => "market",
            OrderType::Limit => "limit",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewOrder {
    pub market_id: String,
    pub token_id: String,
    pub outcome: Outcome,
    pub side: Side,
    pub order_type: OrderType,
    /// For market orders this is the max price the agent will accept.
    pub limit_price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    Filled,
    PartiallyFilled,
    Rejected,
}

impl OrderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderStatus::Filled => "filled",
            OrderStatus::PartiallyFilled => "partially_filled",
            OrderStatus::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub price: f64,
    pub size: f64,
    pub fee: f64,
    pub slippage: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forecast {
    pub market_id: String,
    pub fair_prob_yes: f64,
    pub confidence: f64,
    pub model_version: String,
    pub rationale: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct MarketFilter {
    pub active_only: bool,
    pub limit: usize,
}
