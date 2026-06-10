use anyhow::{Context, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::types::{Market, MarketFilter, MarketResolution, OrderBook, PriceLevel, Venue};

#[derive(Debug, thiserror::Error)]
#[error("clob book for {token_id} returned {status}")]
pub struct ClobBookStatusError {
    pub token_id: String,
    pub status: reqwest::StatusCode,
}

pub fn clob_book_status(error: &anyhow::Error) -> Option<reqwest::StatusCode> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<ClobBookStatusError>()
            .map(|error| error.status)
    })
}

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
    condition_id: Option<String>,
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
    /// JSON-encoded string, e.g. "[\"1\", \"0\"]"; per-share payouts in
    /// clob_token_ids order once the market resolves.
    outcome_prices: Option<String>,
    uma_resolution_status: Option<String>,
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
            condition_id: self.condition_id,
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

impl PolymarketIntl {
    /// Fetch the resolution state of a single market from Gamma. Returns
    /// `None` while the market is still open or its UMA resolution is not
    /// final yet.
    pub async fn get_resolution(
        &self,
        market_id: &str,
    ) -> anyhow::Result<Option<MarketResolution>> {
        let url = format!("{}/markets/{}", self.gamma_base, market_id);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting gamma market by id")?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "gamma market {} returned {}",
                market_id,
                response.status()
            ));
        }
        let raw_json: serde_json::Value = response.json().await.context("decoding gamma market")?;
        let market: GammaMarket =
            serde_json::from_value(raw_json.clone()).context("parsing gamma market")?;

        if !market.closed.unwrap_or(false)
            || market.uma_resolution_status.as_deref() != Some("resolved")
        {
            return Ok(None);
        }
        let payouts: Vec<f64> = match market
            .outcome_prices
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        {
            Some(prices) => prices
                .iter()
                .map(|price| price.parse::<f64>())
                .collect::<Result<_, _>>()
                .context("parsing outcome prices")?,
            None => return Ok(None),
        };
        let (Some(&payout_yes), Some(&payout_no)) = (payouts.first(), payouts.get(1)) else {
            return Ok(None);
        };
        Ok(Some(MarketResolution {
            venue: Venue::PolymarketInternational,
            market_id: market_id.to_string(),
            payout_yes,
            payout_no,
            resolution_source: "gamma_uma".to_string(),
            raw: raw_json,
        }))
    }
}

#[derive(Debug, Deserialize)]
struct ClobBookLevel {
    price: String,
    size: String,
}

#[derive(Debug, Deserialize)]
struct ClobBook {
    /// CLOB condition ID for the market this token belongs to.
    market: Option<String>,
    /// Exchange-side timestamp in milliseconds since the epoch.
    timestamp: Option<String>,
    hash: Option<String>,
    neg_risk: Option<bool>,
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
        // Gamma caps page size at 100, so paginate with offset.
        const PAGE_SIZE: usize = 100;
        let limit = filter.limit.clamp(1, 500);
        let mut markets = Vec::new();
        while markets.len() < limit {
            let page_limit = (limit - markets.len()).min(PAGE_SIZE);
            // Verified against the live API (2026-06): `order=volume24hr`
            // sorts correctly, while the docs' `order=volume_24hr` spelling
            // is silently ignored and returns id-ordered results.
            let mut url = format!(
                "{}/markets?limit={}&offset={}&order=volume24hr&ascending=false",
                self.gamma_base,
                page_limit,
                markets.len()
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
            let page_count = raw.len();
            markets.extend(raw.into_iter().map(GammaMarket::into_market));
            if page_count < page_limit {
                break;
            }
        }
        Ok(markets)
    }

    async fn get_orderbook(&self, token_id: &str) -> anyhow::Result<OrderBook> {
        let url = format!("{}/book?token_id={}", self.clob_base, token_id);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting clob book")?;
        let status = response.status();
        if !status.is_success() {
            return Err(ClobBookStatusError {
                token_id: token_id.to_string(),
                status,
            }
            .into());
        }
        let raw: ClobBook = response.json().await.context("decoding clob book")?;

        let mut bids = parse_levels(raw.bids)?;
        let mut asks = parse_levels(raw.asks)?;
        // CLOB returns levels in ascending price order; normalize to best-first.
        bids.sort_by(|a, b| b.price.total_cmp(&a.price));
        asks.sort_by(|a, b| a.price.total_cmp(&b.price));

        let exchange_ts = raw
            .timestamp
            .as_deref()
            .and_then(|raw| raw.parse::<i64>().ok())
            .and_then(DateTime::<Utc>::from_timestamp_millis);

        Ok(OrderBook {
            token_id: token_id.to_string(),
            ts: Utc::now(),
            condition_id: raw.market,
            exchange_ts,
            hash: raw.hash,
            neg_risk: raw.neg_risk,
            bids,
            asks,
            tick_size: raw.tick_size.and_then(|raw| raw.parse().ok()),
            min_order_size: raw.min_order_size.and_then(|raw| raw.parse().ok()),
        })
    }
}
