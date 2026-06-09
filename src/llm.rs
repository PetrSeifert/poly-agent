use anyhow::{Context, anyhow};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tracing::debug;

use crate::triage;
use crate::types::{Forecast, Market, OrderBook};

/// Forecasts via the Codex CLI (`codex exec`), which uses the user's ChatGPT
/// subscription. The model only ever sees public market data and returns a
/// structured forecast; it has no access to keys, orders, or the ledger.
pub struct CodexForecaster {
    pub binary: String,
    pub model: Option<String>,
    /// Thinking level: minimal | low | medium | high (model-dependent).
    pub reasoning_effort: Option<String>,
    pub timeout: std::time::Duration,
}

impl Default for CodexForecaster {
    fn default() -> Self {
        Self {
            binary: "codex".to_string(),
            model: None,
            reasoning_effort: None,
            timeout: std::time::Duration::from_secs(300),
        }
    }
}

pub const MODEL_VERSION_PREFIX: &str = "codex-exec-v1";

#[derive(Debug, Deserialize)]
struct LlmForecastResponse {
    fair_prob_yes: f64,
    confidence: f64,
    #[serde(default)]
    base_rate: Option<f64>,
    #[serde(default)]
    evidence: serde_json::Value,
    #[serde(default)]
    main_uncertainties: serde_json::Value,
    #[serde(default)]
    resolution_risks: serde_json::Value,
    #[serde(default)]
    do_not_trade_reason: Option<String>,
}

impl CodexForecaster {
    pub async fn forecast(
        &self,
        market: &Market,
        yes_book: &OrderBook,
    ) -> anyhow::Result<Forecast> {
        let market_prob = yes_book
            .midpoint()
            .ok_or_else(|| anyhow!("no midpoint for market {}", market.slug))?;
        let prompt = build_prompt(market, market_prob);

        let mut command = tokio::process::Command::new(&self.binary);
        command
            .arg("exec")
            .arg("--sandbox")
            .arg("read-only")
            .arg("--skip-git-repo-check")
            // Forecasting needs no user MCP servers or project rules, and
            // loading them adds latency and noise.
            .arg("--ignore-user-config")
            // Read prompt from stdin to avoid argv length/quoting issues.
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(model) = &self.model {
            command.arg("--model").arg(model);
        }
        if let Some(effort) = &self.reasoning_effort {
            command
                .arg("-c")
                .arg(format!("model_reasoning_effort={effort:?}"));
        }

        let mut child = command.spawn().context(
            "spawning codex; is the Codex CLI installed and logged in? (npm i -g @openai/codex && codex login)",
        )?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open codex stdin"))?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("writing prompt to codex")?;
        drop(stdin);

        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| anyhow!("codex exec timed out after {:?}", self.timeout))?
            .context("waiting for codex")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "codex exec failed ({}): {}",
                output.status,
                stderr.chars().take(500).collect::<String>()
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        debug!(market = %market.slug, raw = %stdout, "codex raw output");
        let response = extract_forecast_json(&stdout)
            .with_context(|| format!("parsing codex output for {}", market.slug))?;

        if !(0.0..=1.0).contains(&response.fair_prob_yes) {
            return Err(anyhow!(
                "model returned out-of-range probability {}",
                response.fair_prob_yes
            ));
        }

        Ok(Forecast {
            market_id: market.market_id.clone(),
            fair_prob_yes: response.fair_prob_yes,
            confidence: response.confidence.clamp(0.0, 1.0),
            model_version: {
                let mut version = MODEL_VERSION_PREFIX.to_string();
                if let Some(model) = &self.model {
                    version.push_str(&format!(":{model}"));
                }
                if let Some(effort) = &self.reasoning_effort {
                    version.push_str(&format!(":{effort}"));
                }
                version
            },
            rationale: json!({
                "source": "codex_exec",
                "market_price_seen": market_prob,
                "base_rate": response.base_rate,
                "evidence": response.evidence,
                "main_uncertainties": response.main_uncertainties,
                "resolution_risks": response.resolution_risks,
                "do_not_trade_reason": response.do_not_trade_reason,
                "question": market.question,
            }),
        })
    }
}

fn build_prompt(market: &Market, market_prob: f64) -> String {
    let close_time = market
        .close_time
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string());
    let rules = market
        .resolution_rules
        .as_deref()
        .unwrap_or("not provided")
        .chars()
        .take(2000)
        .collect::<String>();
    let category = triage::classify(market);
    format!(
        r#"You are a careful probabilistic forecaster for prediction markets.

Estimate the probability that the following market resolves YES.
Reason from base rates and any knowledge you have. Be calibrated:
avoid both overconfidence and anchoring blindly on the market price.

Question: {question}
Detected category: {category}
Resolution rules: {rules}
Market closes: {close_time}
Current date/time: {now}
Current market-implied probability of YES: {market_prob:.4}

Your edge comes from structured, checkable reasoning: exact arithmetic,
base rates, reference prices, and strict reading of the resolution rules.
You do NOT have an edge in rumor-driven narratives. Anchor on quantitative
inputs for crypto/sports/economics/weather questions; for anything driven by
breaking news or insider information, prefer not to trade.

If the resolution criteria are ambiguous, or you lack the domain knowledge or
recent information needed to beat the market, say so via "do_not_trade_reason"
and set confidence to 0.

Respond with ONLY a JSON object (no markdown fences, no other text):
{{
  "fair_prob_yes": <float 0..1>,
  "confidence": <float 0..1, how much you trust your estimate over the market>,
  "base_rate": <float 0..1 or null>,
  "evidence": [{{"source": "...", "claim": "..."}}],
  "main_uncertainties": ["..."],
  "resolution_risks": ["..."],
  "do_not_trade_reason": <string or null>
}}"#,
        question = market.question,
        category = category.as_str(),
        now = Utc::now().to_rfc3339(),
    )
}

/// Codex prints the final agent message to stdout, but it may be preceded by
/// other text or wrapped in markdown fences. Find the last parseable JSON
/// object in the output.
fn extract_forecast_json(stdout: &str) -> anyhow::Result<LlmForecastResponse> {
    let mut last_error = anyhow!("no JSON object found in codex output");
    let starts: Vec<usize> = stdout.match_indices('{').map(|(index, _)| index).collect();
    for start in starts.into_iter().rev() {
        let candidate = &stdout[start..];
        let mut deserializer = serde_json::Deserializer::from_str(candidate);
        match LlmForecastResponse::deserialize(&mut deserializer) {
            Ok(parsed) => return Ok(parsed),
            Err(error) => last_error = error.into(),
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_with_surrounding_noise() {
        let output = r#"Thinking about it...
```json
{"fair_prob_yes": 0.42, "confidence": 0.6, "base_rate": 0.5,
 "evidence": [], "main_uncertainties": ["x"], "resolution_risks": [],
 "do_not_trade_reason": null}
```
done"#;
        let parsed = extract_forecast_json(output).expect("should parse");
        assert!((parsed.fair_prob_yes - 0.42).abs() < 1e-9);
        assert!((parsed.confidence - 0.6).abs() < 1e-9);
    }

    #[test]
    fn fails_on_garbage() {
        assert!(extract_forecast_json("no json here { broken").is_err());
    }
}
