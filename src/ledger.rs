use anyhow::Context;
use chrono::Utc;
use rusqlite::{Connection, params};

use crate::types::{
    ExecutionMode, Fill, Forecast, Market, NewOrder, OrderBook, OrderStatus, Venue,
};

pub struct Ledger {
    connection: Connection,
}

#[derive(Debug, Clone)]
pub struct PositionRow {
    pub market_id: String,
    pub token_id: String,
    pub outcome: String,
    pub shares: f64,
    pub cost_basis: f64,
}

#[derive(Debug, Clone, Default)]
pub struct AccountSummary {
    pub cash: f64,
    pub realized_pnl: f64,
    pub total_fees: f64,
    pub open_positions: Vec<PositionRow>,
}

impl Ledger {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let connection = Connection::open(path).context("opening ledger database")?;
        connection
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS markets(
                    venue TEXT NOT NULL,
                    event_id TEXT,
                    market_id TEXT NOT NULL,
                    slug TEXT,
                    question TEXT,
                    resolution_rules TEXT,
                    close_time TEXT,
                    active INTEGER NOT NULL,
                    closed INTEGER NOT NULL,
                    neg_risk INTEGER NOT NULL,
                    yes_token_id TEXT,
                    no_token_id TEXT,
                    volume_24hr REAL,
                    liquidity REAL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY (venue, market_id)
                );

                CREATE TABLE IF NOT EXISTS orderbook_snapshots(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts TEXT NOT NULL,
                    venue TEXT NOT NULL,
                    market_id TEXT NOT NULL,
                    token_id TEXT NOT NULL,
                    best_bid REAL,
                    best_ask REAL,
                    spread REAL,
                    midpoint REAL,
                    raw_book_json TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS forecasts(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts TEXT NOT NULL,
                    market_id TEXT NOT NULL,
                    fair_prob_yes REAL NOT NULL,
                    confidence REAL NOT NULL,
                    model_version TEXT NOT NULL,
                    rationale_json TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS orders(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts TEXT NOT NULL,
                    mode TEXT NOT NULL,
                    market_id TEXT NOT NULL,
                    token_id TEXT NOT NULL,
                    outcome TEXT NOT NULL,
                    side TEXT NOT NULL,
                    order_type TEXT NOT NULL,
                    limit_price REAL NOT NULL,
                    size REAL NOT NULL,
                    status TEXT NOT NULL,
                    reject_reason TEXT,
                    policy_version TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS fills(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    order_id INTEGER NOT NULL REFERENCES orders(id),
                    ts TEXT NOT NULL,
                    price REAL NOT NULL,
                    size REAL NOT NULL,
                    fee REAL NOT NULL,
                    slippage REAL NOT NULL,
                    liquidity_flag TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS positions(
                    market_id TEXT NOT NULL,
                    token_id TEXT NOT NULL,
                    outcome TEXT NOT NULL,
                    shares REAL NOT NULL,
                    cost_basis REAL NOT NULL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY (market_id, token_id)
                );

                CREATE TABLE IF NOT EXISTS account(
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    cash REAL NOT NULL,
                    realized_pnl REAL NOT NULL,
                    total_fees REAL NOT NULL
                );

                CREATE TABLE IF NOT EXISTS equity_snapshots(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts TEXT NOT NULL,
                    cash REAL NOT NULL,
                    mtm_liquidation REAL NOT NULL,
                    mtm_mid REAL NOT NULL,
                    open_positions INTEGER NOT NULL
                );
                "#,
            )
            .context("creating ledger schema")?;
        Ok(Self { connection })
    }

    pub fn ensure_account(&self, starting_cash: f64) -> anyhow::Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO account(id, cash, realized_pnl, total_fees) VALUES (1, ?1, 0, 0)",
            params![starting_cash],
        )?;
        Ok(())
    }

    pub fn upsert_market(&self, market: &Market) -> anyhow::Result<()> {
        self.connection.execute(
            r#"
            INSERT INTO markets(
                venue, event_id, market_id, slug, question, resolution_rules,
                close_time, active, closed, neg_risk, yes_token_id, no_token_id,
                volume_24hr, liquidity, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
            ON CONFLICT(venue, market_id) DO UPDATE SET
                event_id = excluded.event_id,
                slug = excluded.slug,
                question = excluded.question,
                resolution_rules = excluded.resolution_rules,
                close_time = excluded.close_time,
                active = excluded.active,
                closed = excluded.closed,
                neg_risk = excluded.neg_risk,
                yes_token_id = excluded.yes_token_id,
                no_token_id = excluded.no_token_id,
                volume_24hr = excluded.volume_24hr,
                liquidity = excluded.liquidity,
                updated_at = excluded.updated_at
            "#,
            params![
                market.venue.as_str(),
                market.event_id,
                market.market_id,
                market.slug,
                market.question,
                market.resolution_rules,
                market.close_time.map(|ts| ts.to_rfc3339()),
                market.active,
                market.closed,
                market.neg_risk,
                market.yes_token_id,
                market.no_token_id,
                market.volume_24hr,
                market.liquidity,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn insert_snapshot(
        &self,
        venue: Venue,
        market_id: &str,
        book: &OrderBook,
    ) -> anyhow::Result<()> {
        let raw = serde_json::to_string(book).context("serializing orderbook")?;
        self.connection.execute(
            r#"
            INSERT INTO orderbook_snapshots(
                ts, venue, market_id, token_id, best_bid, best_ask, spread, midpoint, raw_book_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            params![
                book.ts.to_rfc3339(),
                venue.as_str(),
                market_id,
                book.token_id,
                book.best_bid(),
                book.best_ask(),
                book.spread(),
                book.midpoint(),
                raw,
            ],
        )?;
        Ok(())
    }

    pub fn insert_forecast(&self, forecast: &Forecast) -> anyhow::Result<()> {
        let rationale =
            serde_json::to_string(&forecast.rationale).context("serializing rationale")?;
        self.connection.execute(
            r#"
            INSERT INTO forecasts(ts, market_id, fair_prob_yes, confidence, model_version, rationale_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                Utc::now().to_rfc3339(),
                forecast.market_id,
                forecast.fair_prob_yes,
                forecast.confidence,
                forecast.model_version,
                rationale,
            ],
        )?;
        Ok(())
    }

    pub fn insert_order(
        &self,
        order: &NewOrder,
        mode: ExecutionMode,
        status: OrderStatus,
        reject_reason: Option<&str>,
        policy_version: &str,
    ) -> anyhow::Result<i64> {
        self.connection.execute(
            r#"
            INSERT INTO orders(
                ts, mode, market_id, token_id, outcome, side, order_type,
                limit_price, size, status, reject_reason, policy_version
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            "#,
            params![
                Utc::now().to_rfc3339(),
                mode.as_str(),
                order.market_id,
                order.token_id,
                order.outcome.as_str(),
                order.side.as_str(),
                order.order_type.as_str(),
                order.limit_price,
                order.size,
                status.as_str(),
                reject_reason,
                policy_version,
            ],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn insert_fill(&self, order_id: i64, fill: &Fill) -> anyhow::Result<()> {
        self.connection.execute(
            r#"
            INSERT INTO fills(order_id, ts, price, size, fee, slippage, liquidity_flag)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'simulated')
            "#,
            params![
                order_id,
                Utc::now().to_rfc3339(),
                fill.price,
                fill.size,
                fill.fee,
                fill.slippage,
            ],
        )?;
        Ok(())
    }

    pub fn apply_buy_fill(
        &self,
        market_id: &str,
        token_id: &str,
        outcome: &str,
        fill: &Fill,
    ) -> anyhow::Result<()> {
        let cost = fill.price * fill.size + fill.fee;
        let now = Utc::now().to_rfc3339();
        self.connection.execute(
            r#"
            INSERT INTO positions(market_id, token_id, outcome, shares, cost_basis, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(market_id, token_id) DO UPDATE SET
                shares = shares + excluded.shares,
                cost_basis = cost_basis + excluded.cost_basis,
                updated_at = excluded.updated_at
            "#,
            params![market_id, token_id, outcome, fill.size, cost, now],
        )?;
        self.connection.execute(
            "UPDATE account SET cash = cash - ?1, total_fees = total_fees + ?2 WHERE id = 1",
            params![cost, fill.fee],
        )?;
        Ok(())
    }

    pub fn cash(&self) -> anyhow::Result<f64> {
        let cash = self
            .connection
            .query_row("SELECT cash FROM account WHERE id = 1", [], |row| {
                row.get(0)
            })
            .context("reading account cash")?;
        Ok(cash)
    }

    pub fn position_cost(&self, market_id: &str) -> anyhow::Result<f64> {
        let cost: Option<f64> = self
            .connection
            .query_row(
                "SELECT SUM(cost_basis) FROM positions WHERE market_id = ?1",
                params![market_id],
                |row| row.get(0),
            )
            .context("reading position cost")?;
        Ok(cost.unwrap_or(0.0))
    }

    pub fn markets_with_tokens(&self, limit: usize) -> anyhow::Result<Vec<Market>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT venue, event_id, market_id, slug, question, resolution_rules,
                   close_time, active, closed, neg_risk, yes_token_id, no_token_id,
                   volume_24hr, liquidity
            FROM markets
            WHERE yes_token_id IS NOT NULL AND active = 1 AND closed = 0
            ORDER BY volume_24hr DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map(params![limit as i64], |row| {
            let venue_raw: String = row.get(0)?;
            let close_time_raw: Option<String> = row.get(6)?;
            Ok(Market {
                venue: if venue_raw == "polymarket_us" {
                    Venue::PolymarketUs
                } else {
                    Venue::PolymarketInternational
                },
                event_id: row.get(1)?,
                market_id: row.get(2)?,
                slug: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                question: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                resolution_rules: row.get(5)?,
                close_time: close_time_raw
                    .as_deref()
                    .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                    .map(|parsed| parsed.with_timezone(&Utc)),
                active: row.get(7)?,
                closed: row.get(8)?,
                neg_risk: row.get(9)?,
                yes_token_id: row.get(10)?,
                no_token_id: row.get(11)?,
                volume_24hr: row.get(12)?,
                liquidity: row.get(13)?,
            })
        })?;
        let mut markets = Vec::new();
        for row in rows {
            markets.push(row?);
        }
        Ok(markets)
    }

    pub fn latest_forecast(&self, market_id: &str) -> anyhow::Result<Option<Forecast>> {
        let result = self.connection.query_row(
            r#"
            SELECT market_id, fair_prob_yes, confidence, model_version, rationale_json
            FROM forecasts WHERE market_id = ?1
            ORDER BY ts DESC LIMIT 1
            "#,
            params![market_id],
            |row| {
                let rationale_raw: String = row.get(4)?;
                Ok(Forecast {
                    market_id: row.get(0)?,
                    fair_prob_yes: row.get(1)?,
                    confidence: row.get(2)?,
                    model_version: row.get(3)?,
                    rationale: serde_json::from_str(&rationale_raw)
                        .unwrap_or(serde_json::Value::Null),
                })
            },
        );
        match result {
            Ok(forecast) => Ok(Some(forecast)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn summary(&self) -> anyhow::Result<AccountSummary> {
        let (cash, realized_pnl, total_fees) = self.connection.query_row(
            "SELECT cash, realized_pnl, total_fees FROM account WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let mut statement = self.connection.prepare(
            "SELECT market_id, token_id, outcome, shares, cost_basis FROM positions WHERE shares > 0",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(PositionRow {
                market_id: row.get(0)?,
                token_id: row.get(1)?,
                outcome: row.get(2)?,
                shares: row.get(3)?,
                cost_basis: row.get(4)?,
            })
        })?;
        let mut open_positions = Vec::new();
        for row in rows {
            open_positions.push(row?);
        }
        Ok(AccountSummary {
            cash,
            realized_pnl,
            total_fees,
            open_positions,
        })
    }

    /// Age of the newest forecast for a market, if any.
    pub fn forecast_age(&self, market_id: &str) -> anyhow::Result<Option<chrono::Duration>> {
        let result: Result<String, _> = self.connection.query_row(
            "SELECT ts FROM forecasts WHERE market_id = ?1 ORDER BY ts DESC LIMIT 1",
            params![market_id],
            |row| row.get(0),
        );
        match result {
            Ok(raw) => {
                let ts = chrono::DateTime::parse_from_rfc3339(&raw)
                    .context("parsing forecast timestamp")?
                    .with_timezone(&Utc);
                Ok(Some(Utc::now() - ts))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Mark all open positions against the latest snapshots and persist an
    /// equity point. Returns (liquidation_equity, midpoint_equity).
    pub fn record_equity_snapshot(&self) -> anyhow::Result<(f64, f64)> {
        let summary = self.summary()?;
        let mut mtm_liquidation = 0.0;
        let mut mtm_mid = 0.0;
        for position in &summary.open_positions {
            if let Some((best_bid, midpoint)) = self.latest_snapshot_quote(&position.token_id)? {
                mtm_liquidation += position.shares * best_bid;
                mtm_mid += position.shares * midpoint;
            }
        }
        self.connection.execute(
            r#"
            INSERT INTO equity_snapshots(ts, cash, mtm_liquidation, mtm_mid, open_positions)
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            params![
                Utc::now().to_rfc3339(),
                summary.cash,
                mtm_liquidation,
                mtm_mid,
                summary.open_positions.len() as i64,
            ],
        )?;
        Ok((
            summary.cash + mtm_liquidation,
            summary.cash + mtm_mid,
        ))
    }

    pub fn equity_curve(&self, limit: usize) -> anyhow::Result<Vec<(String, f64, f64, i64)>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT ts, cash + mtm_liquidation, cash + mtm_mid, open_positions
            FROM equity_snapshots ORDER BY ts DESC LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map(params![limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        let mut points = Vec::new();
        for row in rows {
            points.push(row?);
        }
        points.reverse();
        Ok(points)
    }

    pub fn order_counts(&self) -> anyhow::Result<(i64, i64)> {
        let filled = self.connection.query_row(
            "SELECT COUNT(*) FROM orders WHERE status != 'rejected'",
            [],
            |row| row.get(0),
        )?;
        let rejected = self.connection.query_row(
            "SELECT COUNT(*) FROM orders WHERE status = 'rejected'",
            [],
            |row| row.get(0),
        )?;
        Ok((filled, rejected))
    }

    pub fn latest_snapshot_quote(&self, token_id: &str) -> anyhow::Result<Option<(f64, f64)>> {
        let result = self.connection.query_row(
            r#"
            SELECT best_bid, midpoint FROM orderbook_snapshots
            WHERE token_id = ?1 AND best_bid IS NOT NULL AND midpoint IS NOT NULL
            ORDER BY ts DESC LIMIT 1
            "#,
            params![token_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        match result {
            Ok(quote) => Ok(Some(quote)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}
