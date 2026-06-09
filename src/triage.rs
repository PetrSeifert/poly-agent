use crate::types::{Market, OrderBook};

/// Market domains ranked by how repeatable the agent's edge is, per
/// MARKET_AGENT_RESEARCH.md: structured-data domains (crypto, sports,
/// economics, weather) are preferred; narrative-heavy domains (politics,
/// geopolitics, culture) are avoided until the agent has proven calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketCategory {
    Crypto,
    Sports,
    Economics,
    Weather,
    Politics,
    Geopolitics,
    Culture,
    Other,
}

impl MarketCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            MarketCategory::Crypto => "crypto",
            MarketCategory::Sports => "sports",
            MarketCategory::Economics => "economics",
            MarketCategory::Weather => "weather",
            MarketCategory::Politics => "politics",
            MarketCategory::Geopolitics => "geopolitics",
            MarketCategory::Culture => "culture",
            MarketCategory::Other => "other",
        }
    }
}

/// How the policy and the forecast scheduler should treat a market.
#[derive(Debug, Clone)]
pub struct TriageProfile {
    pub category: MarketCategory,
    /// Categories the research says to avoid early: trades are blocked.
    pub trade_blocked: bool,
    /// Multiplier on the policy's forecast shrinkage weight. Structured
    /// domains keep full trust; everything else gets shrunk harder.
    pub forecast_trust: f64,
    /// Multiplier on the policy's minimum required edge.
    pub min_edge_multiplier: f64,
    /// Base priority for spending LLM forecast budget on this market.
    pub forecast_priority: f64,
    /// Rough resolution-clarity signal from the rules text.
    pub rules_clarity: RulesClarity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RulesClarity {
    Missing,
    Thin,
    Detailed,
}

const CRYPTO_KEYWORDS: &[&str] = &[
    "bitcoin", "btc", "ethereum", " eth ", "solana", " sol ", "crypto", "xrp", "dogecoin",
    "stablecoin", "binance", "coinbase", "market cap of", "token", "altcoin",
];

const SPORTS_KEYWORDS: &[&str] = &[
    "nba", "nfl", "mlb", "nhl", "ufc", " mls ", "premier league", "champions league",
    "world cup", "super bowl", "grand prix", " f1 ", "formula 1", "wimbledon", "us open",
    "playoff", "stanley cup", "world series", "la liga", "serie a", "bundesliga",
    "heavyweight", " vs. ", " vs ", "match", "tournament", "olympic",
];

const ECONOMICS_KEYWORDS: &[&str] = &[
    "fed ", "fomc", "rate cut", "rate hike", "interest rate", "cpi", "inflation", "gdp",
    "unemployment", "jobs report", "nonfarm", "recession", "treasury yield", "earnings",
    "ipo", "s&p 500", "nasdaq", "stock price",
];

const WEATHER_KEYWORDS: &[&str] = &[
    "temperature", "hurricane", "rainfall", "snowfall", "weather", "highest temp",
    "degrees fahrenheit", "degrees celsius", "tropical storm", "heat wave",
];

const GEOPOLITICS_KEYWORDS: &[&str] = &[
    "ceasefire", "invasion", "invade", "missile", "airstrike", "nato", "nuclear weapon",
    "sanction", "ukraine", "gaza", "israel", "iran", "taiwan", "war ", "hostage",
    "hormuz", "peace deal", "blockade", "troops",
];

const POLITICS_KEYWORDS: &[&str] = &[
    "election", "president", "presidential", "senate", "congress", "governor", "trump",
    "biden", "nominee", "nomination", "impeach", "parliament", "prime minister", "cabinet",
    "supreme court", "mayor", "ballot", "primary", "approval rating", "executive order",
    "white house", "democrat", "republican",
];

const CULTURE_KEYWORDS: &[&str] = &[
    "oscar", "grammy", "emmy", "album", "box office", "movie", "billboard", "taylor swift",
    "kanye", "mrbeast", "tiktok", "spotify", "person of the year", "celebrity", "rotten tomatoes",
    "netflix", "song",
];

fn matches_any(haystack: &str, keywords: &[&str]) -> bool {
    keywords.iter().any(|keyword| haystack.contains(keyword))
}

pub fn classify(market: &Market) -> MarketCategory {
    // Pad with spaces so word-boundary-ish keywords like " eth " can match
    // at the start and end of the text.
    let text = format!(
        " {} {} ",
        market.question.to_lowercase(),
        market.slug.to_lowercase().replace('-', " ")
    );
    // Structured domains are checked first so e.g. "Will BTC win the race to
    // $100k before the election?" lands in crypto, not politics.
    if matches_any(&text, CRYPTO_KEYWORDS) {
        return MarketCategory::Crypto;
    }
    if matches_any(&text, WEATHER_KEYWORDS) {
        return MarketCategory::Weather;
    }
    if matches_any(&text, ECONOMICS_KEYWORDS) {
        return MarketCategory::Economics;
    }
    if matches_any(&text, SPORTS_KEYWORDS) {
        return MarketCategory::Sports;
    }
    if matches_any(&text, GEOPOLITICS_KEYWORDS) {
        return MarketCategory::Geopolitics;
    }
    if matches_any(&text, POLITICS_KEYWORDS) {
        return MarketCategory::Politics;
    }
    if matches_any(&text, CULTURE_KEYWORDS) {
        return MarketCategory::Culture;
    }
    MarketCategory::Other
}

pub fn rules_clarity(market: &Market) -> RulesClarity {
    match market.resolution_rules.as_deref().map(str::trim) {
        None | Some("") => RulesClarity::Missing,
        Some(rules) if rules.len() < 120 => RulesClarity::Thin,
        Some(_) => RulesClarity::Detailed,
    }
}

pub fn profile(market: &Market) -> TriageProfile {
    let category = classify(market);
    let clarity = rules_clarity(market);

    let (trade_blocked, forecast_trust, mut min_edge_multiplier, mut forecast_priority) =
        match category {
            MarketCategory::Crypto
            | MarketCategory::Sports
            | MarketCategory::Economics
            | MarketCategory::Weather => (false, 1.0, 1.0, 1.0),
            MarketCategory::Other => (false, 0.6, 1.5, 0.5),
            MarketCategory::Politics | MarketCategory::Geopolitics | MarketCategory::Culture => {
                (true, 0.0, f64::INFINITY, 0.0)
            }
        };

    match clarity {
        RulesClarity::Detailed => {}
        RulesClarity::Thin => {
            min_edge_multiplier *= 1.5;
            forecast_priority *= 0.7;
        }
        RulesClarity::Missing => {
            // The research gate requires clear rules; without any rules text
            // the market is untradeable and not worth an LLM call.
            min_edge_multiplier = f64::INFINITY;
            forecast_priority = 0.0;
        }
    }

    TriageProfile {
        category,
        trade_blocked,
        forecast_trust,
        min_edge_multiplier,
        forecast_priority,
        rules_clarity: clarity,
    }
}

/// Score for ranking which markets deserve scarce LLM forecast calls, loosely
/// following the research's opportunity_score: category edge prior, liquidity,
/// resolution clarity, spread tightness, and forecast staleness.
pub fn opportunity_score(
    profile: &TriageProfile,
    market: &Market,
    yes_book: &OrderBook,
    forecast_age_hours: f64,
) -> f64 {
    if profile.forecast_priority <= 0.0 {
        return 0.0;
    }
    let liquidity_score = (market.liquidity.unwrap_or(0.0) / 50_000.0).clamp(0.0, 1.0);
    let spread_score = match yes_book.spread() {
        Some(spread) => (1.0 - spread / 0.10).clamp(0.0, 1.0),
        None => 0.0,
    };
    let staleness_score = (forecast_age_hours / 24.0).clamp(0.0, 1.0);
    0.40 * profile.forecast_priority
        + 0.20 * liquidity_score
        + 0.20 * spread_score
        + 0.20 * staleness_score
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Venue;

    fn market(question: &str, rules: Option<&str>) -> Market {
        Market {
            venue: Venue::PolymarketInternational,
            event_id: None,
            market_id: "m1".to_string(),
            slug: String::new(),
            question: question.to_string(),
            resolution_rules: rules.map(str::to_string),
            close_time: None,
            active: true,
            closed: false,
            neg_risk: false,
            yes_token_id: Some("yes".to_string()),
            no_token_id: Some("no".to_string()),
            volume_24hr: Some(10_000.0),
            liquidity: Some(20_000.0),
        }
    }

    const DETAILED_RULES: &str = "This market resolves YES if the official closing price \
        reported by the designated source exceeds the threshold at the stated time, \
        otherwise it resolves NO. See the linked source for details.";

    #[test]
    fn classifies_structured_domains() {
        assert_eq!(
            classify(&market("Will Bitcoin close above $100,000 on June 30?", None)),
            MarketCategory::Crypto
        );
        assert_eq!(
            classify(&market("Will the Lakers win the NBA Finals?", None)),
            MarketCategory::Sports
        );
        assert_eq!(
            classify(&market("Will CPI inflation exceed 3% in July?", None)),
            MarketCategory::Economics
        );
        assert_eq!(
            classify(&market("Highest temperature in NYC on Friday above 90?", None)),
            MarketCategory::Weather
        );
    }

    #[test]
    fn classifies_avoid_domains() {
        assert_eq!(
            classify(&market("Will Trump win the 2028 election?", None)),
            MarketCategory::Politics
        );
        assert_eq!(
            classify(&market("Will there be a ceasefire by August?", None)),
            MarketCategory::Geopolitics
        );
        assert_eq!(
            classify(&market("Will Taylor Swift release an album this year?", None)),
            MarketCategory::Culture
        );
    }

    #[test]
    fn crypto_beats_politics_on_mixed_text() {
        assert_eq!(
            classify(&market(
                "Will Bitcoin hit $150k before the presidential election?",
                None
            )),
            MarketCategory::Crypto
        );
    }

    #[test]
    fn avoid_categories_block_trading() {
        let profile = profile(&market(
            "Will Trump win the 2028 election?",
            Some(DETAILED_RULES),
        ));
        assert!(profile.trade_blocked);
        assert_eq!(profile.forecast_priority, 0.0);
    }

    #[test]
    fn structured_category_with_detailed_rules_is_full_trust() {
        let profile = profile(&market(
            "Will Bitcoin close above $100,000 on June 30?",
            Some(DETAILED_RULES),
        ));
        assert!(!profile.trade_blocked);
        assert_eq!(profile.forecast_trust, 1.0);
        assert_eq!(profile.min_edge_multiplier, 1.0);
        assert_eq!(profile.rules_clarity, RulesClarity::Detailed);
    }

    #[test]
    fn missing_rules_make_market_untradeable() {
        let profile = profile(&market("Will Bitcoin close above $100,000?", None));
        assert!(profile.min_edge_multiplier.is_infinite());
        assert_eq!(profile.forecast_priority, 0.0);
    }

    #[test]
    fn thin_rules_raise_required_edge() {
        let profile = profile(&market(
            "Will Bitcoin close above $100,000?",
            Some("Resolves per Coinbase."),
        ));
        assert_eq!(profile.rules_clarity, RulesClarity::Thin);
        assert!(profile.min_edge_multiplier > 1.0);
        assert!(profile.min_edge_multiplier.is_finite());
    }
}
