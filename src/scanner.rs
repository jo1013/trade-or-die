use anyhow::Result;
use ethers::prelude::Address;
use reqwest::Client;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::time::Duration;
use tracing::{debug, warn};

fn de_opt_f64_any<'de, D>(deserializer: D) -> std::result::Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<Value>::deserialize(deserializer)?;
    Ok(raw.and_then(|v| match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }))
}

fn parse_jsonish_array(value: &Option<Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(arr)) => arr.clone(),
        Some(Value::String(raw)) => serde_json::from_str::<Vec<Value>>(raw).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn is_yes_label(label: &str) -> bool {
    matches!(
        label.trim().to_ascii_lowercase().as_str(),
        "yes" | "true" | "up" | "higher" | "above"
    )
}

fn is_no_label(label: &str) -> bool {
    matches!(
        label.trim().to_ascii_lowercase().as_str(),
        "no" | "false" | "down" | "lower" | "below"
    )
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Market {
    pub question: String,
    pub description: Option<String>,
    pub outcomes: Option<Value>, // [ "Yes", "No" ] or JSON string
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<Value>, // [ "id1", "id2" ] or JSON string
    #[serde(rename = "outcomePrices")]
    pub outcome_prices: Option<Value>, // [0.5, 0.5] or JSON string
    pub active: bool,
    pub closed: bool,
    #[serde(rename = "negRisk", default)]
    pub neg_risk: bool,
    #[serde(rename = "acceptingOrders", default)]
    pub accepting_orders: bool,
    #[serde(rename = "enableOrderBook", default)]
    pub enable_order_book: bool,
    #[serde(rename = "volume24hr", default, deserialize_with = "de_opt_f64_any")]
    pub volume_24hr: Option<f64>,
    #[serde(rename = "volume", default, deserialize_with = "de_opt_f64_any")]
    pub volume_total: Option<f64>,
    #[serde(rename = "liquidity", default, deserialize_with = "de_opt_f64_any")]
    pub liquidity: Option<f64>,
    #[serde(
        rename = "oneDayPriceChange",
        default,
        deserialize_with = "de_opt_f64_any"
    )]
    pub one_day_price_change: Option<f64>,
    #[serde(rename = "endDate", default)]
    pub end_date: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Event {
    pub markets: Option<Vec<Market>>,
}

// 우리가 내부적으로 사용할 변환된 시장 구조체
pub struct CleanMarket {
    pub question: String,
    pub description: Option<String>,
    pub tokens: Vec<CleanToken>,
    pub neg_risk: bool,
    pub volume_24hr: f64,
    pub volume_total: f64,
    pub liquidity: f64,
    pub one_day_price_change: f64,
    pub end_date: Option<String>,
}

pub struct CleanToken {
    pub token_id: String,
    pub price: Option<f64>,
    pub outcome_label: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Position {
    pub asset: Option<String>,
    pub title: Option<String>,
    pub outcome: Option<String>,
    #[serde(rename = "avgPrice")]
    pub avg_price: Option<f64>,
    #[serde(rename = "curPrice")]
    pub cur_price: Option<f64>,
    #[serde(rename = "initialValue")]
    pub initial_value: Option<f64>,
    #[serde(rename = "currentValue")]
    pub current_value: Option<f64>,
    pub redeemable: Option<bool>,
    #[serde(rename = "negativeRisk")]
    pub negative_risk: Option<bool>,
    pub size: Option<f64>,
    pub closed: Option<bool>,
}

pub struct Scanner {
    client: Client,
}

impl Scanner {
    pub fn new() -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { client }
    }

    /// GET request with retry + exponential backoff for transient errors
    async fn get_with_retry(&self, url: &str) -> Result<reqwest::Response> {
        let max_retries = 2u32;
        let mut last_err = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let backoff_ms = 300 * 2u64.pow(attempt - 1);
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }

            match self.client.get(url).send().await {
                Ok(resp) => {
                    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || resp.status().is_server_error()
                    {
                        last_err = Some(anyhow::anyhow!(
                            "HTTP {} from {}",
                            resp.status(),
                            url
                        ));
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    let kind = if e.is_timeout() {
                        "timeout"
                    } else if e.is_connect() {
                        "connection"
                    } else {
                        "request"
                    };
                    warn!(kind, attempt = attempt + 1, max = max_retries + 1, err = %e, url, "Scanner request error");
                    last_err = Some(anyhow::anyhow!("Scanner {} error: {}", kind, e));
                    continue;
                }
            }
        }

        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("GET failed after {} retries: {}", max_retries, url)))
    }

    /// 500~1000개 시장을 페이지네이션으로 스캔한 뒤,
    /// 가격이 mispricing 가능성이 높은 구간(0.05~0.95)인 것만 필터링해서 반환
    pub async fn get_active_markets(&self) -> Result<Vec<CleanMarket>> {
        let mut all_markets: Vec<Market> = Vec::new();
        let mut offset = 0;
        let limit = 50;
        let mut consecutive_failures = 0u32;

        // Use events endpoint and flatten event.markets (Polymarket docs recommendation).
        loop {
            let url = format!(
                "https://gamma-api.polymarket.com/events?active=true&closed=false&limit={}&offset={}&order=volume24hr&ascending=false",
                limit, offset
            );

            let batch: Vec<Event> = match self.get_with_retry(&url).await {
                Ok(resp) => {
                    match resp.json().await {
                        Ok(v) => {
                            consecutive_failures = 0;
                            v
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            warn!(offset, fail = consecutive_failures, err = %e, "Market JSON parse failed");
                            if consecutive_failures >= 3 {
                                break;
                            }
                            continue;
                        }
                    }
                }
                Err(e) => {
                    consecutive_failures += 1;
                    warn!(fail = consecutive_failures, err = %e, "Market fetch failed");
                    if consecutive_failures >= 3 {
                        break;
                    }
                    continue;
                }
            };

            let batch_len = batch.len();
            for event in batch {
                if let Some(markets) = event.markets {
                    all_markets.extend(markets);
                }
            }

            if batch_len < limit || all_markets.len() >= 1200 {
                break;
            }
            offset += limit;
        }

        debug!(count = all_markets.len(), "Fetched markets from Gamma events API");

        let mut result = Vec::new();

        for m in all_markets {
            if m.closed
                || !m.active
                || !m.accepting_orders
                || !m.enable_order_book
                || m.clob_token_ids.is_none()
                || m.outcome_prices.is_none()
            {
                continue;
            }

            let ids: Vec<String> = parse_jsonish_array(&m.clob_token_ids)
                .iter()
                .filter_map(value_to_string)
                .collect();
            let prices: Vec<f64> = parse_jsonish_array(&m.outcome_prices)
                .iter()
                .filter_map(value_to_f64)
                .collect();
            let outcomes: Vec<String> = parse_jsonish_array(&m.outcomes)
                .iter()
                .filter_map(value_to_string)
                .collect();

            // Strategy/Kelly assumes binary complements (A/B = 1-p), so restrict to
            // two-outcome markets with recognizable yes/no labels.
            if ids.len() != 2 || prices.len() != 2 || outcomes.len() != 2 {
                continue;
            }

            let outcome_a_idx = outcomes
                .iter()
                .position(|o| is_yes_label(o))
                .unwrap_or(usize::MAX);
            let outcome_b_idx = outcomes
                .iter()
                .position(|o| is_no_label(o))
                .unwrap_or(usize::MAX);
            if outcome_a_idx == usize::MAX
                || outcome_b_idx == usize::MAX
                || outcome_a_idx == outcome_b_idx
            {
                continue;
            }

            let first_price = prices
                .get(outcome_a_idx)
                .or_else(|| prices.first())
                .copied()
                .unwrap_or(0.0);
            if !(0.05..=0.95).contains(&first_price) {
                continue;
            }

            let mut clean_tokens = Vec::new();
            for (i, id) in ids.iter().enumerate() {
                let price = prices.get(i).copied();
                let outcome_label = outcomes.get(i).cloned();
                clean_tokens.push(CleanToken {
                    token_id: id.clone(),
                    price,
                    outcome_label,
                });
            }

            result.push(CleanMarket {
                question: m.question,
                description: m.description,
                tokens: clean_tokens,
                neg_risk: m.neg_risk,
                volume_24hr: m.volume_24hr.unwrap_or(0.0),
                volume_total: m.volume_total.unwrap_or(0.0),
                liquidity: m.liquidity.unwrap_or(0.0),
                one_day_price_change: m.one_day_price_change.unwrap_or(0.0),
                end_date: m.end_date,
            });
        }

        result.sort_by(|a, b| {
            b.volume_24hr
                .partial_cmp(&a.volume_24hr)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        debug!(count = result.len(), "Tradable markets passed filters");
        Ok(result)
    }

    /// Fetch real positions from Polymarket data API
    pub async fn fetch_positions(&self, proxy_address: Address) -> Result<Vec<Position>> {
        let url = format!(
            "https://data-api.polymarket.com/positions?user={:?}",
            proxy_address
        );
        let resp = self.get_with_retry(&url).await?;
        let positions: Vec<Position> = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!(err = %e, "Positions JSON parse failed");
                Vec::new()
            }
        };
        Ok(positions)
    }

    /// Build Telegram portfolio report from real on-chain positions
    pub async fn build_portfolio_report(
        &self,
        proxy_address: Address,
        usdc_balance: f64,
        api_left: f64,
    ) -> String {
        let positions = match self.fetch_positions(proxy_address).await {
            Ok(p) => p,
            Err(e) => return format!("Failed to fetch positions: {}", e),
        };

        if positions.is_empty() {
            return format!(
                "📊 *Argo Agent - Portfolio Report*\n\nNo open positions.\nUSDC: `${:.2}` | API Left: `${:.4}`",
                usdc_balance, api_left
            );
        }

        // Filter out dead positions (current_value == 0 and cur_price == 0)
        let positions: Vec<_> = positions
            .into_iter()
            .filter(|p| {
                let cv = p.current_value.unwrap_or(0.0);
                let cp = p.cur_price.unwrap_or(0.0);
                cv > 0.0 || cp > 0.0
            })
            .collect();

        if positions.is_empty() {
            return format!(
                "📊 *Argo Agent - Portfolio Report*\n\nNo active positions.\nUSDC: `${:.2}` | API Left: `${:.4}`",
                usdc_balance, api_left
            );
        }

        let total_invested: f64 = positions
            .iter()
            .map(|p| p.initial_value.unwrap_or(0.0))
            .sum();
        let total_current: f64 = positions
            .iter()
            .map(|p| p.current_value.unwrap_or(0.0))
            .sum();
        let total_pnl = total_current - total_invested;
        let pnl_pct = if total_invested > 0.0 {
            total_pnl / total_invested * 100.0
        } else {
            0.0
        };

        let mut lines = Vec::new();
        for p in &positions {
            let raw_title = p.title.as_deref().unwrap_or("Unknown");
            let mut title: String = raw_title.chars().take(90).collect();
            if raw_title.chars().count() > 90 {
                title.push_str("...");
            }
            let outcome = p.outcome.as_deref().unwrap_or("?");
            let avg = p.avg_price.unwrap_or(0.0);
            let cur = p.cur_price.unwrap_or(0.0);
            let inv = p.initial_value.unwrap_or(0.0);
            let cur_val = p.current_value.unwrap_or(0.0);
            let pos_pnl = cur_val - inv;

            let (icon, status) = if cur == 0.0 && cur_val == 0.0 {
                ("⚠️", format!("@{:.3}", avg))
            } else if pos_pnl >= 0.0 {
                let pct = if avg > 0.0 {
                    ((cur / avg) - 1.0) * 100.0
                } else {
                    0.0
                };
                ("🟢", format!("{:+.0}%", pct))
            } else {
                let pct = if avg > 0.0 {
                    ((cur / avg) - 1.0) * 100.0
                } else {
                    0.0
                };
                ("🔴", format!("{:+.0}%", pct))
            };

            lines.push(format!(
                "{} {}\n   {} | `{:.3}`→`{:.3}` | ${:.1}→${:.1} ({})",
                icon, title, outcome, avg, cur, inv, cur_val, status
            ));
        }

        format!(
            "📊 *Argo Agent - Portfolio Report*\n\n\
            💰 *Balance*\n\
            USDC: `${:.2}`\n\
            Positions: `${:.2}`\n\
            Total: `${:.2}`\n\n\
            📈 *Performance*\n\
            Invested: `${:.2}` → Current: `${:.2}`\n\
            P&L: `${:+.2}` ({:+.1}%)\n\
            Open: `{}` positions\n\n\
            📋 *Positions*\n{}",
            usdc_balance,
            total_current,
            usdc_balance + total_current,
            total_invested,
            total_current,
            total_pnl,
            pnl_pct,
            positions.len(),
            lines.join("\n")
        )
    }

    /// Check if a market has resolved by querying CLOB API for token price
    /// Returns Some((resolved, winning_price)) - winning_price is 1.0 for YES win, 0.0 for NO win
    pub async fn fetch_token_price_with_side(
        &self,
        token_id: &str,
        side: &str,
    ) -> Result<Option<f64>> {
        let side = match side {
            "sell" | "SELL" => "sell",
            _ => "buy",
        };

        let url = format!(
            "https://clob.polymarket.com/price?token_id={}&side={}",
            token_id, side
        );

        let resp = self.get_with_retry(&url).await?;
        let body = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                warn!(err = %e, "Failed to read price response body");
                return Ok(None);
            }
        };
        let parsed: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                warn!(err = %e, body = &body[..body.len().min(200)], "Failed to parse price JSON");
                return Ok(None);
            }
        };

        let price = parsed["price"]
            .as_str()
            .and_then(|p| p.parse::<f64>().ok())
            .or_else(|| parsed["price"].as_f64());

        Ok(price)
    }

    pub async fn fetch_token_price(&self, token_id: &str) -> Result<Option<f64>> {
        self.fetch_token_price_with_side(token_id, "buy").await
    }

    /// Check if a market has resolved by querying CLOB API for token price
    /// Returns Some((resolved, winning_price)) - winning_price is 1.0 for YES win, 0.0 for NO win
    pub async fn check_market_resolved(&self, token_id: &str) -> Result<Option<f64>> {
        match self.fetch_token_price(token_id).await? {
            Some(p) if p >= 0.99 => Ok(Some(1.0)), // YES won
            Some(p) if p <= 0.01 => Ok(Some(0.0)), // NO won (YES lost)
            _ => Ok(None),                         // Not resolved yet
        }
    }
}
