use duckdb::{params, Connection, Result};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Database {
    conn: Connection,
}

pub struct PendingTrade {
    pub id: i64,
    pub market_question: String,
    pub market_description: Option<String>,
    pub token_id: String,
    pub side: String,
    pub price: f64,
    pub size: f64,
    pub neg_risk: bool,
    pub outcome_label: Option<String>,
    pub entry_probability: Option<f64>,
    pub entry_edge: Option<f64>,
    pub strategy_note: Option<String>,
}

fn classify_market(question: &str) -> &'static str {
    let q = question.to_lowercase();
    if q.contains("vs.")
        || q.contains("vs ")
        || q.contains("nba")
        || q.contains("nfl")
        || q.contains("nhl")
        || q.contains("mlb")
        || q.contains("stanley cup")
        || q.contains("super bowl")
        || q.contains("lakers")
        || q.contains("celtics")
        || q.contains("mavericks")
        || q.contains("warriors")
        || q.contains("blazers")
        || q.contains("jazz")
        || q.contains("game")
    {
        "Sports"
    } else if q.contains("trump")
        || q.contains("biden")
        || q.contains("election")
        || q.contains("president")
        || q.contains("congress")
        || q.contains("nato")
        || q.contains("ukraine")
        || q.contains("iran")
        || q.contains("deport")
        || q.contains("nominate")
        || q.contains("war")
        || q.contains("sanction")
    {
        "Politics"
    } else if q.contains("fed")
        || q.contains("interest rate")
        || q.contains("gdp")
        || q.contains("inflation")
        || q.contains("market cap")
        || q.contains("ipo")
        || q.contains("stock")
        || q.contains("s&p")
        || q.contains("nasdaq")
    {
        "Finance"
    } else if q.contains("bitcoin")
        || q.contains("ethereum")
        || q.contains("btc")
        || q.contains("eth")
        || q.contains("crypto")
        || q.contains("solana")
    {
        "Crypto"
    } else {
        "Other"
    }
}

impl Database {
    pub fn new() -> Result<Self> {
        let _ = std::fs::create_dir_all("data");
        let db_path = std::env::var("ARGO_DB_PATH").unwrap_or_else(|_| "data/argo_agent.db".into());
        let wal_path = format!("{}.wal", db_path);

        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(open_err) => {
                let err_msg = open_err.to_string().to_lowercase();
                if err_msg.contains("permission denied") {
                    let fallback_db_path = std::env::var("ARGO_DB_FALLBACK_PATH")
                        .unwrap_or_else(|_| "data/argo_agent_rw.db".into());
                    if db_path != fallback_db_path
                        && Path::new(&db_path).exists()
                        && !Path::new(&fallback_db_path).exists()
                    {
                        let _ = std::fs::copy(&db_path, &fallback_db_path);
                    }
                    println!(
                        "⚠️ DB path `{}` is not writable. Falling back to `{}`.",
                        db_path, fallback_db_path
                    );
                    Connection::open(fallback_db_path)?
                } else if Path::new(&wal_path).exists() {
                    let ts = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let backup_path = format!("{}.bak.{}", wal_path, ts);
                    if std::fs::rename(&wal_path, &backup_path).is_err() {
                        let _ = std::fs::remove_file(&wal_path);
                    }
                    println!(
                        "⚠️ DuckDB WAL replay failed. Removed corrupt WAL and retrying open at `{}`. Error: {}",
                        db_path,
                        open_err
                    );
                    Connection::open(&db_path)?
                } else {
                    return Err(open_err);
                }
            }
        };

        conn.execute(
            "CREATE TABLE IF NOT EXISTS analysis (
                id INTEGER PRIMARY KEY,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
                market_question TEXT,
                probability DOUBLE,
                reasoning TEXT,
                cost DOUBLE
            )",
            [],
        )?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS trades (
                id INTEGER PRIMARY KEY,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
                market_question TEXT,
                market_description TEXT,
                token_id TEXT,
                side TEXT,
                price DOUBLE,
                size DOUBLE,
                neg_risk BOOLEAN DEFAULT FALSE,
                outcome_label TEXT,
                entry_probability DOUBLE,
                entry_edge DOUBLE,
                strategy_note TEXT,
                status TEXT,
                outcome TEXT DEFAULT 'PENDING',
                payout DOUBLE DEFAULT 0.0,
                resolved_at DATETIME
            )",
            [],
        )?;
        let _ = conn.execute(
            "ALTER TABLE trades ADD COLUMN IF NOT EXISTS neg_risk BOOLEAN DEFAULT FALSE",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE trades ADD COLUMN IF NOT EXISTS market_description TEXT",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE trades ADD COLUMN IF NOT EXISTS outcome_label TEXT",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE trades ADD COLUMN IF NOT EXISTS entry_probability DOUBLE",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE trades ADD COLUMN IF NOT EXISTS entry_edge DOUBLE",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE trades ADD COLUMN IF NOT EXISTS strategy_note TEXT",
            [],
        );

        conn.execute(
            "CREATE TABLE IF NOT EXISTS balance_history (
                id INTEGER PRIMARY KEY,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
                balance DOUBLE
            )",
            [],
        )?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS api_costs (
                id INTEGER PRIMARY KEY,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
                cost DOUBLE,
                source TEXT
            )",
            [],
        )?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS runtime_state (
                key TEXT PRIMARY KEY,
                value_double DOUBLE,
                value_text TEXT,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
            )",
            [],
        )?;

        Ok(Self { conn })
    }

    pub fn log_analysis(&self, question: &str, prob: f64, reason: &str, cost: f64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO analysis (market_question, probability, reasoning, cost) VALUES (?, ?, ?, ?)",
            params![question, prob, reason, cost],
        )?;
        Ok(())
    }

    pub fn log_trade(
        &self,
        question: &str,
        description: Option<&str>,
        token_id: &str,
        side: &str,
        price: f64,
        size: f64,
        neg_risk: bool,
        outcome_label: Option<&str>,
        entry_probability: Option<f64>,
        entry_edge: Option<f64>,
        strategy_note: Option<&str>,
        status: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO trades (
                market_question, market_description, token_id, side, price, size, neg_risk,
                outcome_label, entry_probability, entry_edge, strategy_note, status
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                question,
                description,
                token_id,
                side,
                price,
                size,
                neg_risk,
                outcome_label,
                entry_probability,
                entry_edge,
                strategy_note,
                status
            ],
        )?;
        Ok(())
    }

    pub fn has_pending_trade_for_token(&self, token_id: &str) -> bool {
        self.conn
            .prepare(
                "SELECT COUNT(*) FROM trades WHERE token_id = ? AND status = 'SUCCESS' AND outcome = 'PENDING'",
            )
            .and_then(|mut s| s.query_row(params![token_id], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false)
    }

    /// Get pending trades that need resolution check
    pub fn get_pending_trades(&self) -> Vec<PendingTrade> {
        let mut results = Vec::new();
        if let Ok(mut stmt) = self.conn.prepare(
            "SELECT
                id,
                market_question,
                market_description,
                token_id,
                side,
                price,
                size,
                COALESCE(neg_risk, FALSE),
                outcome_label,
                entry_probability,
                entry_edge,
                strategy_note
             FROM trades
             WHERE status = 'SUCCESS' AND outcome = 'PENDING'",
        ) {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok(PendingTrade {
                    id: row.get::<_, i64>(0)?,
                    market_question: row.get::<_, String>(1)?,
                    market_description: row.get::<_, Option<String>>(2)?,
                    token_id: row.get::<_, String>(3)?,
                    side: row.get::<_, String>(4)?,
                    price: row.get::<_, f64>(5)?,
                    size: row.get::<_, f64>(6)?,
                    neg_risk: row.get::<_, bool>(7)?,
                    outcome_label: row.get::<_, Option<String>>(8)?,
                    entry_probability: row.get::<_, Option<f64>>(9)?,
                    entry_edge: row.get::<_, Option<f64>>(10)?,
                    strategy_note: row.get::<_, Option<String>>(11)?,
                })
            }) {
                for row in rows.flatten() {
                    results.push(row);
                }
            }
        }
        results
    }

    /// Update trade outcome after market resolves
    pub fn resolve_trade(&self, trade_id: i64, outcome: &str, payout: f64) -> Result<()> {
        self.conn.execute(
            "UPDATE trades SET outcome = ?, payout = ?, resolved_at = CURRENT_TIMESTAMP WHERE id = ?",
            params![outcome, payout, trade_id],
        )?;
        Ok(())
    }

    pub fn log_api_cost(&self, cost: f64, source: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO api_costs (cost, source) VALUES (?, ?)",
            params![cost, source],
        )?;
        Ok(())
    }

    pub fn get_total_api_cost(&self) -> f64 {
        self.conn
            .prepare("SELECT COALESCE(SUM(cost), 0) FROM api_costs")
            .and_then(|mut s| s.query_row([], |row| row.get(0)))
            .unwrap_or(0.0)
    }

    /// Realized profit from resolved successful trades.
    /// Pending positions are excluded.
    pub fn get_realized_profit(&self) -> f64 {
        self.conn
            .prepare(
                "SELECT COALESCE(SUM(payout - size), 0)
                 FROM trades
                 WHERE status = 'SUCCESS' AND outcome <> 'PENDING'",
            )
            .and_then(|mut s| s.query_row([], |row| row.get(0)))
            .unwrap_or(0.0)
    }

    pub fn set_runtime_f64(&self, key: &str, value: f64) -> Result<()> {
        self.conn
            .execute("DELETE FROM runtime_state WHERE \"key\" = ?", params![key])?;
        self.conn.execute(
            "INSERT INTO runtime_state (\"key\", value_double, updated_at)
             VALUES (?, ?, CURRENT_TIMESTAMP)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_runtime_f64(&self, key: &str) -> Option<f64> {
        self.conn
            .prepare("SELECT value_double FROM runtime_state WHERE \"key\" = ?")
            .and_then(|mut s| s.query_row(params![key], |row| row.get(0)))
            .ok()
    }

    /// Hourly portfolio summary for Telegram
    pub fn get_portfolio_summary(&self) -> String {
        // 1. All positions
        let mut positions: Vec<(String, String, f64, f64, String, f64)> = Vec::new();
        if let Ok(mut stmt) = self.conn.prepare(
            "SELECT market_question, side, price, size, outcome, payout FROM trades WHERE status = 'SUCCESS' ORDER BY timestamp DESC"
        ) {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, f64>(5)?,
                ))
            }) {
                for row in rows.flatten() {
                    positions.push(row);
                }
            }
        }

        if positions.is_empty() {
            return "No positions yet.".to_string();
        }

        let total_invested: f64 = positions.iter().map(|p| p.3).sum();
        let total_trades = positions.len();
        let pending = positions.iter().filter(|p| p.4 == "PENDING").count();
        let wins = positions.iter().filter(|p| p.4 == "WIN").count();
        let losses = positions.iter().filter(|p| p.4 == "LOSS").count();
        let total_payout: f64 = positions.iter().map(|p| p.5).sum();
        let pnl = total_payout
            - positions
                .iter()
                .filter(|p| p.4 != "PENDING")
                .map(|p| p.3)
                .sum::<f64>();

        let pos_lines: Vec<String> = positions
            .iter()
            .take(10)
            .map(|p| {
                let short_q: String = p.0.chars().take(30).collect();
                let status_icon = match p.4.as_str() {
                    "WIN" => "✅",
                    "LOSS" => "❌",
                    _ => "⏳",
                };
                let payout_str = if p.4 != "PENDING" {
                    format!(" → $`{:.2}`", p.5)
                } else {
                    String::new()
                };
                format!(
                    "{} `{}` {} @ `{:.2}` ($`{:.2}`){}",
                    status_icon, short_q, p.1, p.2, p.3, payout_str
                )
            })
            .collect();

        let api_spent = self.get_total_api_cost();

        format!(
            "📊 *Portfolio Summary*\n\
            Trades: `{}` | Invested: `${:.2}`\n\
            Results: ✅`{}` ❌`{}` ⏳`{}`\n\
            P&L: `${:+.2}` | API Spent: `${:.4}`\n\n\
            *Positions:*\n{}",
            total_trades,
            total_invested,
            wins,
            losses,
            pending,
            pnl,
            api_spent,
            pos_lines.join("\n")
        )
    }

    pub fn log_balance(&self, balance: f64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO balance_history (balance) VALUES (?)",
            params![balance],
        )?;
        Ok(())
    }

    /// Compact learning summary for AI prompt
    pub fn get_learning_summary(&self) -> String {
        // 1. Trade count
        let total_trades: i64 = self
            .conn
            .prepare("SELECT COUNT(*) FROM trades WHERE status = 'SUCCESS'")
            .and_then(|mut s| s.query_row([], |row| row.get(0)))
            .unwrap_or(0);

        if total_trades == 0 {
            return "No trades yet.".to_string();
        }

        // 2. Total spent
        let total_size: f64 = self
            .conn
            .prepare("SELECT COALESCE(SUM(size), 0) FROM trades WHERE status = 'SUCCESS'")
            .and_then(|mut s| s.query_row([], |row| row.get(0)))
            .unwrap_or(0.0);

        // 3. API cost
        let total_api_cost: f64 = self
            .conn
            .prepare("SELECT COALESCE(SUM(cost), 0) FROM analysis")
            .and_then(|mut s| s.query_row([], |row| row.get(0)))
            .unwrap_or(0.0);

        // 4. Categorize recent trades
        let mut categories: HashMap<&str, (i32, f64)> = HashMap::new(); // (count, total_size)
        if let Ok(mut stmt) = self.conn.prepare(
            "SELECT market_question, size FROM trades WHERE status = 'SUCCESS' ORDER BY timestamp DESC LIMIT 30"
        ) {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                ))
            }) {
                for row in rows.flatten() {
                    let cat = classify_market(&row.0);
                    let entry = categories.entry(cat).or_insert((0, 0.0));
                    entry.0 += 1;
                    entry.1 += row.1;
                }
            }
        }

        // 5. Balance trend
        let mut trend = String::new();
        if let Ok(mut stmt) = self
            .conn
            .prepare("SELECT balance FROM balance_history ORDER BY timestamp DESC LIMIT 6")
        {
            let balances: Vec<f64> = stmt
                .query_map([], |row| row.get(0))
                .ok()
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default();

            if balances.len() >= 2 {
                let now = balances[0];
                let prev = balances[balances.len() - 1];
                let diff = now - prev;
                let arrow = if diff > 0.5 {
                    "UP"
                } else if diff < -0.5 {
                    "DOWN"
                } else {
                    "FLAT"
                };
                trend = format!(" | Recent: ${:.0}->${:.0}({})", prev, now, arrow);
            }
        }

        // 6. Recent trade list (last 5)
        let mut recent_list = Vec::new();
        if let Ok(mut stmt) = self.conn.prepare(
            "SELECT market_question, side, price FROM trades WHERE status = 'SUCCESS' ORDER BY timestamp DESC LIMIT 5"
        ) {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            }) {
                for row in rows.flatten() {
                    let short_q: String = row.0.chars().take(40).collect();
                    recent_list.push(format!("{}@{:.2}({})", short_q, row.2, row.1));
                }
            }
        }

        // Build compact summary
        let cat_str: String = categories
            .iter()
            .map(|(k, (cnt, sz))| format!("{}:{}(${:.0})", k, cnt, sz))
            .collect::<Vec<_>>()
            .join(", ");

        let recent_str = if recent_list.is_empty() {
            String::new()
        } else {
            format!("\nRecent: {}", recent_list.join(" | "))
        };

        format!(
            "Trades:{} Invested:${:.0} API:${:.2} | By type: [{}]{}{}",
            total_trades, total_size, total_api_cost, cat_str, trend, recent_str
        )
    }
}
