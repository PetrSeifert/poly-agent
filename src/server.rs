use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use serde_json::json;
use std::sync::Arc;
use tracing::info;

use crate::ledger::Ledger;

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

struct AppState {
    db_path: String,
}

pub async fn serve(db_path: String, port: u16) -> anyhow::Result<()> {
    let state = Arc::new(AppState { db_path });
    let app = axum::Router::new()
        .route("/", get(|| async { Html(DASHBOARD_HTML) }))
        .route("/api/report", get(report))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    info!("dashboard at http://127.0.0.1:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn report(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    build_report(&state.db_path)
        .map(Json)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

/// Opens a fresh read connection per request; WAL mode makes this safe while
/// a simulation run writes from another process.
fn build_report(db_path: &str) -> anyhow::Result<serde_json::Value> {
    let ledger = Ledger::open(db_path)?;
    let summary = ledger.summary()?;
    let (filled, rejected) = ledger.order_counts()?;

    let mut positions = Vec::new();
    let mut mtm_liquidation = 0.0;
    let mut mtm_mid = 0.0;
    for position in &summary.open_positions {
        let quote = ledger.latest_snapshot_quote(&position.token_id)?;
        let (best_bid, midpoint) = match quote {
            Some((bid, mid)) => (Some(bid), Some(mid)),
            None => (None, None),
        };
        let liq_value = best_bid.map(|bid| bid * position.shares);
        let mid_value = midpoint.map(|mid| mid * position.shares);
        mtm_liquidation += liq_value.unwrap_or(0.0);
        mtm_mid += mid_value.unwrap_or(0.0);
        positions.push(json!({
            "market_id": position.market_id,
            "question": ledger.market_question(&position.market_id)?,
            "outcome": position.outcome,
            "shares": position.shares,
            "cost_basis": position.cost_basis,
            "best_bid": best_bid,
            "midpoint": midpoint,
            "liquidation_value": liq_value,
            "midpoint_value": mid_value,
        }));
    }

    let equity_curve: Vec<serde_json::Value> = ledger
        .equity_curve(1000)?
        .into_iter()
        .map(|(ts, liquidation, midpoint, open_positions)| {
            json!({
                "ts": ts,
                "liquidation": liquidation,
                "midpoint": midpoint,
                "open_positions": open_positions,
            })
        })
        .collect();

    Ok(json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "account": {
            "cash": summary.cash,
            "realized_pnl": summary.realized_pnl,
            "total_fees": summary.total_fees,
            "equity_liquidation": summary.cash + mtm_liquidation,
            "equity_midpoint": summary.cash + mtm_mid,
        },
        "orders": { "filled": filled, "rejected": rejected },
        "positions": positions,
        "equity_curve": equity_curve,
        "recent_orders": ledger.recent_orders(50)?,
        "recent_forecasts": ledger.recent_forecasts(50)?,
    }))
}
