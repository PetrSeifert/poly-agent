use serde_json::json;

use crate::types::{Forecast, Market, OrderBook};

pub const MODEL_VERSION: &str = "stub-market-anchor-v0";

/// Forecast stub per the build plan: anchor on the market midpoint, or accept
/// a manual probability. Replace with real models/LLM research only after the
/// accounting pipeline is proven correct.
pub fn stub_forecast(
    market: &Market,
    yes_book: &OrderBook,
    manual_prob: Option<f64>,
) -> Option<Forecast> {
    let market_prob = yes_book.midpoint()?;
    let (fair_prob_yes, confidence, source) = match manual_prob {
        Some(prob) => (prob.clamp(0.0, 1.0), 0.5, "manual"),
        None => (market_prob, 0.0, "market_midpoint"),
    };
    Some(Forecast {
        market_id: market.market_id.clone(),
        fair_prob_yes,
        confidence,
        model_version: MODEL_VERSION.to_string(),
        rationale: json!({
            "source": source,
            "market_price_seen": market_prob,
            "question": market.question,
        }),
    })
}
