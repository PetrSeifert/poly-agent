use crate::types::{Fill, NewOrder, OrderBook, OrderStatus, Side};

#[derive(Debug, Clone)]
pub struct PaperFillResult {
    pub status: OrderStatus,
    pub fills: Vec<Fill>,
    pub reject_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PaperBrokerConfig {
    /// Taker fee rate; per-share fee is `fee_rate * p * (1 - p)`.
    pub fee_rate: f64,
    pub allow_partial_fills: bool,
}

impl Default for PaperBrokerConfig {
    fn default() -> Self {
        Self {
            fee_rate: 0.0,
            allow_partial_fills: false,
        }
    }
}

pub struct PaperBroker {
    config: PaperBrokerConfig,
}

impl PaperBroker {
    pub fn new(config: PaperBrokerConfig) -> Self {
        Self { config }
    }

    /// Simulate a marketable order by walking the live book at decision time.
    /// Buys consume asks, sells consume bids. The order is rejected if the
    /// book cannot satisfy the size within the order's limit price.
    pub fn execute(&self, order: &NewOrder, book: &OrderBook) -> PaperFillResult {
        if order.size <= 0.0 {
            return reject("order size must be positive");
        }
        if !(0.0..=1.0).contains(&order.limit_price) {
            return reject("limit price must be within [0, 1]");
        }
        if let Some(min_size) = book.min_order_size
            && order.size < min_size
        {
            return reject(&format!(
                "size {} below market minimum {}",
                order.size, min_size
            ));
        }

        let levels = match order.side {
            Side::Buy => &book.asks,
            Side::Sell => &book.bids,
        };
        if levels.is_empty() {
            return reject("no liquidity on the relevant side of the book");
        }
        let reference_price = match levels.first() {
            Some(level) => level.price,
            None => return reject("no liquidity on the relevant side of the book"),
        };

        let mut remaining = order.size;
        let mut fills = Vec::new();
        for level in levels {
            let price_ok = match order.side {
                Side::Buy => level.price <= order.limit_price,
                Side::Sell => level.price >= order.limit_price,
            };
            if !price_ok {
                break;
            }
            let take = remaining.min(level.size);
            if take <= 0.0 {
                break;
            }
            let fee = self.config.fee_rate * level.price * (1.0 - level.price) * take;
            fills.push(Fill {
                price: level.price,
                size: take,
                fee,
                slippage: (level.price - reference_price).abs(),
            });
            remaining -= take;
            if remaining <= f64::EPSILON {
                break;
            }
        }

        if fills.is_empty() {
            return reject("book cannot fill any size within limit price");
        }
        if remaining > f64::EPSILON && !self.config.allow_partial_fills {
            return reject(&format!(
                "book can only fill {:.2} of {:.2} within limit price",
                order.size - remaining,
                order.size
            ));
        }

        let status = if remaining > f64::EPSILON {
            OrderStatus::PartiallyFilled
        } else {
            OrderStatus::Filled
        };
        PaperFillResult {
            status,
            fills,
            reject_reason: None,
        }
    }
}

fn reject(reason: &str) -> PaperFillResult {
    PaperFillResult {
        status: OrderStatus::Rejected,
        fills: Vec::new(),
        reject_reason: Some(reason.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Outcome, OrderType, PriceLevel};
    use chrono::Utc;

    fn book(asks: Vec<(f64, f64)>) -> OrderBook {
        OrderBook {
            token_id: "t".to_string(),
            ts: Utc::now(),
            bids: vec![],
            asks: asks
                .into_iter()
                .map(|(price, size)| PriceLevel { price, size })
                .collect(),
            tick_size: Some(0.01),
            min_order_size: Some(5.0),
        }
    }

    fn buy(size: f64, limit_price: f64) -> NewOrder {
        NewOrder {
            market_id: "m".to_string(),
            token_id: "t".to_string(),
            outcome: Outcome::Yes,
            side: Side::Buy,
            order_type: OrderType::Market,
            limit_price,
            size,
        }
    }

    #[test]
    fn fills_across_levels_with_fees_and_slippage() {
        let broker = PaperBroker::new(PaperBrokerConfig {
            fee_rate: 0.1,
            allow_partial_fills: false,
        });
        let result = broker.execute(&buy(15.0, 0.60), &book(vec![(0.50, 10.0), (0.55, 10.0)]));
        assert_eq!(result.status, OrderStatus::Filled);
        assert_eq!(result.fills.len(), 2);
        assert!((result.fills[1].slippage - 0.05).abs() < 1e-9);
        assert!((result.fills[0].fee - 0.1 * 0.5 * 0.5 * 10.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_when_depth_insufficient_within_limit() {
        let broker = PaperBroker::new(PaperBrokerConfig::default());
        let result = broker.execute(&buy(50.0, 0.52), &book(vec![(0.50, 10.0), (0.60, 100.0)]));
        assert_eq!(result.status, OrderStatus::Rejected);
    }

    #[test]
    fn rejects_below_min_order_size() {
        let broker = PaperBroker::new(PaperBrokerConfig::default());
        let result = broker.execute(&buy(1.0, 0.99), &book(vec![(0.50, 10.0)]));
        assert_eq!(result.status, OrderStatus::Rejected);
    }
}
