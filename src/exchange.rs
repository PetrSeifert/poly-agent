use anyhow::{Context, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::types::{Market, MarketFilter, OrderBook, PriceLevel, Venue};

#[async_trait::async_trait]
pub trait ExchangeAdapter: Send + Sync {
    fn venue(&self) -> Venue;
    async fn discover_markets(&self, filter: &MarketFilter) -> anyhow::Result<Vec<Market>>;
    async fn get_orderbook(&self, token_id: &str) -> anyhow::Result<OrderBook>;
}

/// Read-only adapter for Polymarket International public APIs:
/// Gamma for market discovery, CLOB for orderbooks. No authentication,
/// no order placement — live execution is intentionally not implemented yet.
pub struct PolymarketIntl {
    http: reqwest::Client,
    gamma_base: String,
    clob_base: String,
}

impl PolymarketIntl {
    pub fn new() -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("poly-agent/0.1 (research; read-only)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("building http client")?;
        Ok(Self {
            http,
            gamma_base: "https://gamma-api.polymarket.com".to_string(),
            clob_base: "https://clob.polymarket.com".to_string(),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    id: String,
    slug: Option<String>,
    question: Option<String>,
    description: Option<String>,
    end_date: Option<String>,
    active: Option<bool>,
    closed: Option<bool>,
    neg_risk: Option<bool>,
    /// Gamma returns this as a JSON-encoded string, e.g. "[\"123\", \"456\"]".
    clob_token_ids: Option<String>,
    #[serde(rename = "volume24hr")]
    volume_24hr: Option<f64>,
    liquidity: Option<serde_json::Value>,
    events: Option<Vec<GammaEventRef>>,
}

#[derive(Debug, Deserialize)]
struct GammaEventRef {
    id: String,
}

fn parse_liquidity(value: &Option<serde_json::Value>) -> Option<f64> {
    match value {
        Some(serde_json::Value::Number(number)) => number.as_f64(),
        Some(serde_json::Value::String(text)) => text.parse().ok(),
        _ => None,
    }
}

impl GammaMarket {
    fn into_market(self) -> Market {
        let token_ids: Vec<String> = self
            .clob_token_ids
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_default();
        let close_time = self
            .end_date
            .as_deref()
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
            .map(|parsed| parsed.with_timezone(&Utc));
        let liquidity = parse_liquidity(&self.liquidity);
        Market {
            venue: Venue::PolymarketInternational,
            event_id: self
                .events
                .as_ref()
                .and_then(|events| events.first())
                .map(|event| event.id.clone()),
            market_id: self.id,
            slug: self.slug.unwrap_or_default(),
            question: self.question.unwrap_or_default(),
            resolution_rules: self.description,
            close_time,
            active: self.active.unwrap_or(false),
            closed: self.closed.unwrap_or(true),
            neg_risk: self.neg_risk.unwrap_or(false),
            yes_token_id: token_ids.first().cloned(),
            no_token_id: token_ids.get(1).cloned(),
            volume_24hr: self.volume_24hr,
            liquidity,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ClobBookLevel {
    price: String,
    size: String,
}

#[derive(Debug, Deserialize)]
struct ClobBook {
    #[serde(default)]
    bids: Vec<ClobBookLevel>,
    #[serde(default)]
    asks: Vec<ClobBookLevel>,
    tick_size: Option<String>,
    min_order_size: Option<String>,
}

fn parse_levels(levels: Vec<ClobBookLevel>) -> anyhow::Result<Vec<PriceLevel>> {
    levels
        .into_iter()
        .map(|level| {
            Ok(PriceLevel {
                price: level
                    .price
                    .parse()
                    .with_context(|| format!("bad price {:?}", level.price))?,
                size: level
                    .size
                    .parse()
                    .with_context(|| format!("bad size {:?}", level.size))?,
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl ExchangeAdapter for PolymarketIntl {
    fn venue(&self) -> Venue {
        Venue::PolymarketInternational
    }

    async fn discover_markets(&self, filter: &MarketFilter) -> anyhow::Result<Vec<Market>> {
        let limit = filter.limit.clamp(1, 500);
        let mut url = format!(
            "{}/markets?limit={}&order=volume24hr&ascending=false",
            self.gamma_base, limit
        );
        if filter.active_only {
            url.push_str("&active=true&closed=false");
        }
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting gamma markets")?;
        if !response.status().is_success() {
            return Err(anyhow!("gamma markets returned {}", response.status()));
        }
        let raw: Vec<GammaMarket> = response.json().await.context("decoding gamma markets")?;
        Ok(raw.into_iter().map(GammaMarket::into_market).collect())
    }

    async fn get_orderbook(&self, token_id: &str) -> anyhow::Result<OrderBook> {
        let url = format!("{}/book?token_id={}", self.clob_base, token_id);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting clob book")?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "clob book for {} returned {}",
                token_id,
                response.status()
            ));
        }
        let raw: ClobBook = response.json().await.context("decoding clob book")?;

        let mut bids = parse_levels(raw.bids)?;
        let mut asks = parse_levels(raw.asks)?;
        // CLOB returns levels in ascending price order; normalize to best-first.
        bids.sort_by(|a, b| b.price.total_cmp(&a.price));
        asks.sort_by(|a, b| a.price.total_cmp(&b.price));

        Ok(OrderBook {
            token_id: token_id.to_string(),
            ts: Utc::now(),
            bids,
            asks,
            tick_size: raw.tick_size.and_then(|raw| raw.parse().ok()),
            min_order_size: raw.min_order_size.and_then(|raw| raw.parse().ok()),
        })
    }
}
