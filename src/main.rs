mod analyst;
mod database;
mod governor;
mod notifier;
mod recharger;
mod scanner;
mod strategy;
mod trader;
mod ui;

use anyhow::Result;
use dotenv::dotenv;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, info, warn, error};

use crate::analyst::Analyst;
use crate::database::Database;
use crate::governor::Governor;
use crate::notifier::Notifier;
use crate::scanner::Scanner;
use crate::strategy::Strategy;
use crate::recharger::Recharger;
use crate::trader::Trader;

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_opt_f64(key: &str) -> Option<f64> {
    std::env::var(key).ok().and_then(|v| v.parse::<f64>().ok())
}

fn order_succeeded(resp: &crate::trader::OrderResponse) -> bool {
    let failed_status = resp
        .status
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("FAILED"))
        .unwrap_or(false);
    let has_real_error = resp
        .error
        .as_deref()
        .map(|e| !e.trim().is_empty())
        .unwrap_or(false);
    !failed_status && !has_real_error
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

fn probability_for_label(prob_a: f64, label: &str) -> Option<f64> {
    if is_yes_label(label) {
        Some(prob_a)
    } else if is_no_label(label) {
        Some(1.0 - prob_a)
    } else {
        None
    }
}

fn binary_yes_no_indices(tokens: &[crate::scanner::CleanToken]) -> Option<(usize, usize)> {
    if tokens.len() != 2 {
        return None;
    }
    let yes_idx = tokens.iter().position(|t| {
        t.outcome_label
            .as_deref()
            .map(is_yes_label)
            .unwrap_or(false)
    })?;
    let no_idx = tokens
        .iter()
        .position(|t| t.outcome_label.as_deref().map(is_no_label).unwrap_or(false))?;
    if yes_idx == no_idx {
        None
    } else {
        Some((yes_idx, no_idx))
    }
}

fn is_geopolitics_market(question: &str) -> bool {
    let q = question.to_lowercase();
    let blocked = [
        "iran", "ceasefire", "regime fall", "strike israel", "supreme leader",
        "successor to khamenei", "forces enter", "war ", "invasion",
        "nuclear weapon", "military action", "airstrike", "troops",
        "pahlavi", "nato strike",
    ];
    blocked.iter().any(|kw| q.contains(kw))
}

fn classify_market_category(question: &str) -> &'static str {
    let q = question.to_lowercase();
    if q.contains("vs.") || q.contains("vs ") || q.contains("nba") || q.contains("nfl")
        || q.contains("nhl") || q.contains("mlb") || q.contains("premier league")
        || q.contains("champions league") || q.contains("best picture")
        || q.contains("oscar") || q.contains("game ")
    {
        "Sports"
    } else if q.contains("bitcoin") || q.contains("btc") || q.contains("ethereum")
        || q.contains("eth") || q.contains("crypto") || q.contains("solana")
    {
        "Crypto"
    } else if q.contains("trump") || q.contains("biden") || q.contains("election")
        || q.contains("president") || q.contains("congress") || q.contains("governor")
    {
        "Politics"
    } else if q.contains("fed") || q.contains("interest rate") || q.contains("gdp")
        || q.contains("inflation") || q.contains("s&p") || q.contains("nasdaq")
    {
        "Finance"
    } else if q.contains("crude") || q.contains("oil") || q.contains("gold")
        || q.contains("silver")
    {
        "Commodities"
    } else {
        "Other"
    }
}

/// Dynamic edge thresholds per category.
/// Sports: outcome more predictable from stats → lower threshold (8%).
/// Crypto/Commodities: volatile, price-driven → moderate (10%).
/// Politics/Finance: narrative-driven, harder to price → higher (12%).
/// Other: default (10%).
/// Returns (screen_min_edge, trade_min_edge).
/// screen_min_edge: used during Haiku screening (lower = more markets pass to analysis).
/// trade_min_edge: used post-analysis for the final trade decision (higher = stricter).
fn category_edge_thresholds(category: &str) -> (f64, f64) {
    match category {
        "Sports"      => (0.06, 0.08),
        "Crypto"      => (0.08, 0.10),
        "Politics"    => (0.10, 0.12),
        "Finance"     => (0.10, 0.12),
        "Commodities" => (0.08, 0.10),
        _             => (0.08, 0.10), // "Other"
    }
}

fn build_market_state(market: &crate::scanner::CleanMarket) -> String {
    let end = market.end_date.as_deref().unwrap_or("NA");
    format!(
        "v24={:.0};vol={:.0};liq={:.0};chg24={:+.2}%;end={}",
        market.volume_24hr,
        market.volume_total,
        market.liquidity,
        market.one_day_price_change * 100.0,
        end
    )
}

fn build_market_state_for_position(
    outcome_label: &str,
    entry_price: f64,
    current_price: f64,
    pnl_pct: f64,
    entry_probability: Option<f64>,
    entry_edge: Option<f64>,
) -> String {
    let entry_prob = entry_probability
        .map(|v| format!("{:.4}", v))
        .unwrap_or_else(|| "NA".to_string());
    let entry_edge = entry_edge
        .map(|v| format!("{:+.4}", v))
        .unwrap_or_else(|| "NA".to_string());
    format!(
        "held={};entry={:.4};now={:.4};pnl={:+.2}%;entry_q={};entry_edge={}",
        outcome_label,
        entry_price,
        current_price,
        pnl_pct * 100.0,
        entry_prob,
        entry_edge,
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    // Initialize structured logging (tracing)
    // Control verbosity via RUST_LOG env: e.g. RUST_LOG=info (default), RUST_LOG=debug
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    // 0. Setup
    let db = Database::new().expect("Failed to initialize DuckDB");
    let wallet_address: ethers::types::Address = std::env::var("WALLET_ADDRESS")
        .expect("WALLET_ADDRESS must be set")
        .parse()?;
    let proxy_address: ethers::types::Address = std::env::var("POLYMARKET_PROXY_ADDRESS")
        .expect("POLYMARKET_PROXY_ADDRESS must be set")
        .parse()?;

    // 1. Initialize Modules
    let mut governor = Governor::new(0.0, wallet_address, proxy_address);
    let notifier = Notifier::new();
    let max_bet_frac = env_f64("MAX_BET_FRACTION", 0.10).clamp(0.02, 0.25);
    let kelly_frac = env_f64("KELLY_FRACTION", 0.5).clamp(0.1, 1.0);
    let strategy = Strategy::new(0.10, max_bet_frac).with_kelly_fraction(kelly_frac);
    let trade_cycle_secs = env_u64("TRADE_CYCLE_SECONDS", 1800).max(30);
    let position_check_secs = env_u64("POSITION_CHECK_SECONDS", 60).max(10);
    let report_interval_secs = env_u64("REPORT_INTERVAL_SECONDS", 14400).max(300);
    let low_balance_wait_secs = env_u64("LOW_BALANCE_WAIT_SECONDS", trade_cycle_secs).max(30);
    let max_screens_cfg = env_u64("MAX_SCREENS_PER_CYCLE", 30).clamp(10, 1000) as usize;
    let max_analyses_cfg = env_u64("MAX_ANALYSES_PER_CYCLE", 3)
        .max(1)
        .min(max_screens_cfg as u64) as usize;
    let max_position_rechecks = env_u64("MAX_POSITION_RECHECKS", 3).max(1) as usize;
    let min_trade_usdc = env_f64("MIN_TRADE_USDC", 1.0).clamp(0.5, 100.0);
    let min_balance_for_any_trade = min_trade_usdc;

    info!(
        cycle_secs = trade_cycle_secs,
        pos_check_secs = position_check_secs,
        report_secs = report_interval_secs,
        screens = max_screens_cfg,
        analyses = max_analyses_cfg,
        rechecks = max_position_rechecks,
        min_trade = format_args!("${:.2}", min_trade_usdc),
        min_bal = format_args!("${:.2}", min_balance_for_any_trade),
        kelly = format_args!("{:.0}%", kelly_frac * 100.0),
        max_bet = format_args!("{:.0}%", max_bet_frac * 100.0),
        "Config loaded"
    );

    // Restore cumulative API costs from DB
    let prev_api_costs = db.get_total_api_cost();
    governor.api_costs = prev_api_costs;
    info!(api_costs = format_args!("${:.4}", prev_api_costs), "Restored API costs from DB");
    let realized_profit = db.get_realized_profit();
    governor.set_realized_profit(realized_profit);
    info!(realized_pnl = format_args!("{:+.2}", realized_profit), "Restored realized trade PnL from DB");

    // API seed budget: env override takes priority over DB (for manual resets)
    // API_CREDIT_SEED means "actual remaining credit right now".
    // Since remaining = seed - db_costs + profit, we must compensate for restored DB costs.
    if let Some(env_seed) = env_opt_f64("API_CREDIT_SEED") {
        let actual_remaining = env_seed.max(0.0);
        let compensated = actual_remaining + prev_api_costs - realized_profit.max(0.0);
        governor.initial_api_credit = compensated.max(0.0);
        let _ = db.set_runtime_f64("api_credit_seed", governor.initial_api_credit);
        info!(remaining = format_args!("${:.4}", actual_remaining), seed = format_args!("${:.4}", governor.initial_api_credit), db_costs = format_args!("${:.4}", prev_api_costs), "API credit initialized from env");
    } else if let Some(seed) = db.get_runtime_f64("api_credit_seed") {
        governor.initial_api_credit = seed.max(0.0);
        info!(seed = format_args!("${:.4}", governor.initial_api_credit), "Restored API seed credit from DB");
    } else if let Some(db_left_legacy) = db.get_runtime_f64("api_credit_left") {
        let migrated_seed =
            (db_left_legacy.max(0.0) + governor.api_costs - realized_profit.max(0.0)).max(0.0);
        governor.initial_api_credit = migrated_seed;
        let _ = db.set_runtime_f64("api_credit_seed", migrated_seed);
        info!(legacy_left = format_args!("${:.4}", db_left_legacy), seed = format_args!("${:.4}", migrated_seed), "Migrated legacy API budget");
    } else if let Some(env_left) = env_opt_f64("API_CREDIT_LEFT") {
        governor.initial_api_credit = env_left.max(0.0);
        let _ = db.set_runtime_f64("api_credit_seed", governor.initial_api_credit);
        info!(seed = format_args!("${:.4}", governor.initial_api_credit), "Initialized API seed credit from env API_CREDIT_LEFT");
    } else {
        governor.initial_api_credit = governor.initial_api_credit.max(0.0);
        let _ = db.set_runtime_f64("api_credit_seed", governor.initial_api_credit);
        info!(seed = format_args!("${:.4}", governor.initial_api_credit), "Initialized API seed credit from INITIAL_API_CREDIT");
    }

    info!(proxy = ?proxy_address, "Fetching initial USDC balance");
    let initial_balance = governor.fetch_real_balance().await.unwrap_or(0.0);
    governor.initial_balance = initial_balance;
    info!(balance = format_args!("${:.2}", initial_balance), "Initial balance");

    let scanner = Scanner::new();

    // Send startup message (portfolio report sent after import)
    if let Some(ref n) = notifier {
        let _ = n
            .send_message(&format!(
                "🚀 *Argo Agent Started*\nProxy: `{:?}`\nBalance: `${:.2}`\nAPI Left: `${:.4}`",
                proxy_address,
                initial_balance,
                governor.remaining_api_credit(),
            ))
            .await;
    }
    let analyst = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) => {
            let model = std::env::var("ANTHROPIC_MODEL").ok();
            Analyst::new(key, model)
        }
        Err(_) => Analyst::new_mock(),
    };

    let trader = match Trader::new() {
        Ok(t) => Some(t),
        Err(_) => {
            warn!("POLYMARKET API KEYS NOT FOUND. TRADING MODE: SIMULATION ONLY.");
            None
        }
    };

    let mut recharger = match Recharger::new(proxy_address) {
        Ok(r) => {
            info!(threshold = format_args!("${:.2}", r.recharge_threshold), amount = format_args!("${:.2}", r.recharge_amount), cooldown = r.cooldown_secs, "Recharger enabled");
            Some(r)
        }
        Err(e) => {
            warn!(err = %e, "Recharger init failed. Auto-recharge disabled.");
            None
        }
    };

    // Import existing Polymarket positions into DB (so bot can manage them)
    if let Ok(positions) = scanner.fetch_positions(proxy_address).await {
        let mut imported = 0;
        for pos in &positions {
            if let Some(ref asset) = pos.asset {
                // Skip closed/redeemable/dead positions
                if pos.closed.unwrap_or(false) || pos.redeemable.unwrap_or(false) {
                    continue;
                }
                // Only import if not already tracked
                if !db.has_pending_trade_for_token(asset) {
                    let avg = pos.avg_price.unwrap_or(0.0);
                    let inv = pos.initial_value.unwrap_or(0.0);
                    if avg > 0.0 && inv > 0.0 {
                        match db.log_trade(
                            pos.title.as_deref().unwrap_or("Imported position"),
                            None,
                            asset,
                            "BUY",
                            avg,
                            inv,
                            pos.negative_risk.unwrap_or(false),
                            pos.outcome.as_deref(),
                            None,
                            None,
                            Some("imported_from_polymarket"),
                            "SUCCESS",
                        ) {
                            Ok(_) => imported += 1,
                            Err(e) => warn!(title = pos.title.as_deref().unwrap_or("?"), err = %e, "Import failed"),
                        }
                    }
                }
            }
        }
        if imported > 0 {
            info!(count = imported, "Imported existing positions into DB");
            if let Some(ref n) = notifier {
                let _ = n
                    .send_message(&format!("📦 {} 기존 포지션 DB에 임포트 완료", imported))
                    .await;
            }
        }
    }

    // Send portfolio report as final startup message (most visible in Telegram)
    if let Some(ref n) = notifier {
        let report = scanner
            .build_portfolio_report(
                proxy_address,
                initial_balance,
                governor.remaining_api_credit(),
            )
            .await;
        let _ = n.send_message(&report).await;
    }

    // Screening cache: (screen_prob, last_screened_time) keyed by market question
    // Markets screened within 1 hour are skipped to save Haiku API costs
    let mut screen_cache: HashMap<String, (f64, Instant)> = HashMap::new();
    let screen_cache_ttl_secs: u64 = env_u64("SCREEN_CACHE_TTL_SECS", 3600);

    // Price-change detection: skip position reviews when price hasn't moved
    let mut last_review: HashMap<String, (f64, std::time::Instant)> = HashMap::new();
    // Price history for MA cross strategy (fallback when API exhausted)
    let mut price_history: HashMap<String, Vec<f64>> = HashMap::new();
    let ma_short_period: usize = env_u64("MA_SHORT_PERIOD", 5).max(2) as usize;  // ~5 checks
    let ma_long_period: usize = env_u64("MA_LONG_PERIOD", 20).max(5) as usize;   // ~20 checks
    let ma_max_history: usize = ma_long_period + 5; // keep a little extra

    let mut terminal = ratatui::init();
    let mut last_report = std::time::Instant::now();
    let mut last_trade_cycle = std::time::Instant::now() - Duration::from_secs(trade_cycle_secs);
    let mut last_low_balance_notice =
        std::time::Instant::now() - Duration::from_secs(low_balance_wait_secs);
    let mut api_low_warned = false;

    // 2. Main Loop
    loop {
        if crossterm::event::poll(Duration::from_millis(500))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.code == crossterm::event::KeyCode::Char('q') {
                    break;
                }
            }
        }

        if let Err(e) = governor.fetch_real_balance().await {
            warn!(err = %e, "Balance fetch failed");
        }
        let _ = db.log_balance(governor.current_balance);
        governor.set_realized_profit(db.get_realized_profit());
        debug!(balance = format_args!("${:.2}", governor.current_balance), realized = format_args!("{:+.2}", governor.realized_profit), api_left = format_args!("${:.4}", governor.remaining_api_credit()), "Status");
        let _ = db.set_runtime_f64("api_credit_left", governor.remaining_api_credit());
        let _ = db.set_runtime_f64("realized_profit", governor.realized_profit);
        ui::draw_ui(&mut terminal, &governor.survival_stats())?;

        // API credit low warning (once at $1)
        if governor.remaining_api_credit() <= 1.0 && governor.remaining_api_credit() > 0.0 && !api_low_warned {
            api_low_warned = true;
            if let Some(ref n) = notifier {
                let pending = db.get_pending_trades();
                let _ = n
                    .send_message(&format!(
                        "🔋 *API 크레딧 부족 경고*\nAPI Left: `${:.4}`\nBalance: `${:.2}` | 포지션: `{}개`\n\nAI 리뷰 곧 중단됩니다. 포지션 자동정산은 계속 작동.",
                        governor.remaining_api_credit(), governor.current_balance, pending.len()
                    ))
                    .await;
            }
        }
        if governor.remaining_api_credit() > 1.0 {
            api_low_warned = false; // reset if credit gets refilled
        }

        // Low balance: don't die, keep managing positions (they may resolve profitably)
        // Just skip new trades, but continue checking existing positions

        if last_report.elapsed().as_secs() > report_interval_secs {
            if let Some(ref n) = notifier {
                let report = scanner
                    .build_portfolio_report(
                        governor.proxy_address,
                        governor.current_balance,
                        governor.remaining_api_credit(),
                    )
                    .await;
                let _ = n.send_message(&report).await;
            }
            // Periodic backup alongside the report interval
            db.backup();
            last_report = std::time::Instant::now();
        }

        // Auto-recharge: when API credit < $5, send $10 to RedotPay
        if let Some(ref mut r) = recharger {
            if r.needs_recharge(governor.remaining_api_credit(), governor.current_balance) {
                info!(api_left = format_args!("${:.4}", governor.remaining_api_credit()), "API credit low, starting auto-recharge pipeline");
                match r.full_recharge_pipeline().await {
                    Ok((tx_hash, amount_sent)) => {
                        if let Some(ref n) = notifier {
                            let _ = n
                                .send_message(&format!(
                                    "⚡ *API Recharge - RedotPay 입금 완료*\n\
                                    API Credit Left: `${:.4}`\n\
                                    RedotPay 입금: `${:.2}` USDC\n\
                                    TX: `{}`\n\n\
                                    👉 *Action Required:*\n\
                                    1. RedotPay 카드로 anthropic.com API 크레딧 충전\n\
                                    2. .env에서 API_CREDIT_LEFT 업데이트\n\
                                    3. 봇이 자동으로 재개됩니다",
                                    governor.remaining_api_credit(),
                                    amount_sent,
                                    tx_hash
                                ))
                                .await;
                        }
                    }
                    Err(e) => {
                        warn!(err = %e, "Auto-recharge pipeline failed");
                        if let Some(ref n) = notifier {
                            let _ = n
                                .send_message(&format!(
                                    "⚠️ *Auto-recharge failed:* `{}`",
                                    e
                                ))
                                .await;
                        }
                    }
                }
            }
        }

        // API credit exhausted -> keep running for MA/TP/SL fallback on positions
        // Only skip new AI analyses; position management continues with price-based logic
        if governor.remaining_api_credit() <= 0.0 {
            let _ = db.set_runtime_f64("api_credit_left", 0.0);
            if !api_low_warned {
                // Send one-time notification (reuse api_low_warned flag for exhaustion too)
                api_low_warned = true;
                warn!("API credit depleted. MA/TP/SL fallback active for position management.");
                if let Some(ref n) = notifier {
                    let pending = db.get_pending_trades();
                    let _ = n
                        .send_message(&format!(
                            "🪫 *API 크레딧 소진*\nBalance: `${:.2}`\n포지션: `{}개` 보유중\n\n⚠️ AI 분석 중단. MA/TP/SL 자동매매 작동중.\n포지션 자동정산도 계속 작동.",
                            governor.current_balance, pending.len()
                        ))
                        .await;
                }
            }
            // DON'T continue — fall through to position check with MA/TP/SL fallback
        }

        // Check pending positions: resolve finished markets + AI-driven HOLD/SELL
        let pending_trades = db.get_pending_trades();
        let total_trades_count = db.debug_trade_counts();
        debug!(pending = pending_trades.len(), total = %total_trades_count, "Trade counts");
        let mut pos_sold = 0usize;
        let mut pos_hold = 0usize;
        let mut pos_summary: Vec<String> = Vec::new();
        if !pending_trades.is_empty() {
            info!(count = pending_trades.len(), "Checking pending positions");
            let mut positions_by_asset: HashMap<String, crate::scanner::Position> = HashMap::new();
            if let Ok(positions) = scanner.fetch_positions(governor.proxy_address).await {
                for p in positions {
                    if let Some(asset) = p.asset.clone() {
                        positions_by_asset.insert(asset, p);
                    }
                }
            }

            let mut rechecked = 0usize;
            for trade in &pending_trades {
                let short_q: String = trade.market_question.chars().take(35).collect();
                let position = positions_by_asset.get(&trade.token_id);

                // Resolve only when position is redeemable (actual resolved state from data-api).
                if let Some(pos) = position {
                    if pos.redeemable.unwrap_or(false) {
                        let payout = pos.current_value.unwrap_or(0.0).max(0.0);
                        let outcome = if payout > 0.0 { "WIN" } else { "LOSS" };
                        let _ = db.resolve_trade(trade.id, outcome, payout);
                        let msg = if outcome == "WIN" {
                            format!(
                                "🎉 *Trade Resolved: WIN!*\n`{}` {} @ `{:.3}`\nInvested: `${:.2}` → Payout: `${:.2}`",
                                short_q, trade.side, trade.price, trade.size, payout
                            )
                        } else {
                            format!(
                                "💸 *Trade Resolved: LOSS*\n`{}` {} @ `{:.3}`\nLost: `${:.2}`",
                                short_q, trade.side, trade.price, trade.size
                            )
                        };
                        info!(outcome, market = %short_q, "Trade resolved");
                        if let Some(ref n) = notifier {
                            let _ = n.send_message(&msg).await;
                        }
                        continue;
                    }
                }

                // Only manage BUY positions with valid entry price
                if trade.side != "BUY" || trade.price <= 0.0 {
                    continue;
                }

                // Get current sell price
                let current_sell_price = match scanner
                    .fetch_token_price_with_side(&trade.token_id, "sell")
                    .await
                {
                    Ok(Some(p)) if (0.0..1.0).contains(&p) => p,
                    _ => position.and_then(|p| p.cur_price).unwrap_or(0.0),
                };
                if !(0.0..1.0).contains(&current_sell_price) {
                    continue;
                }

                let pnl_pct = (current_sell_price / trade.price) - 1.0;
                let held_label = trade
                    .outcome_label
                    .as_deref()
                    .or_else(|| position.and_then(|p| p.outcome.as_deref()))
                    .unwrap_or("UNKNOWN");

                // === 가격 히스토리 기록 (MA 계산용) ===
                let hist = price_history.entry(trade.token_id.clone()).or_insert_with(Vec::new);
                hist.push(current_sell_price);
                if hist.len() > ma_max_history {
                    hist.drain(0..hist.len() - ma_max_history);
                }

                // === Fallback: API 없을 때 MA 크로스 + TP/SL 자동 매매 ===
                let auto_tp_pct = env_f64("AUTO_TP_PCT", 0.15); // +15% 익절
                let auto_sl_pct = env_f64("AUTO_SL_PCT", -0.10); // -10% 손절
                if governor.remaining_api_credit() <= 0.0 {
                    // 1) 하드 TP/SL — 무조건 실행
                    let trigger = if pnl_pct >= auto_tp_pct {
                        Some(("TP", format!("자동 익절 {:+.1}% >= {:.0}%", pnl_pct * 100.0, auto_tp_pct * 100.0)))
                    } else if pnl_pct <= auto_sl_pct {
                        Some(("SL", format!("자동 손절 {:+.1}% <= {:.0}%", pnl_pct * 100.0, auto_sl_pct * 100.0)))
                    } else {
                        None
                    };

                    // 2) MA 크로스 판단 — TP/SL 아닌 영역에서 추세 반전 감지
                    let trigger = trigger.or_else(|| {
                        let h = price_history.get(&trade.token_id)?;
                        if h.len() < ma_long_period {
                            return None; // 데이터 부족
                        }
                        let short_ma: f64 = h[h.len()-ma_short_period..].iter().sum::<f64>() / ma_short_period as f64;
                        let long_ma: f64 = h[h.len()-ma_long_period..].iter().sum::<f64>() / ma_long_period as f64;

                        // 데드크로스: 단기 MA가 장기 MA 아래로 — 하락 추세
                        if short_ma < long_ma * 0.995 {
                            // 수익 중이면 추세 꺾임 → 익절, 손실 중이면 하락 확인 → 손절
                            let tag = if pnl_pct >= 0.0 { "MA_TP" } else { "MA_SL" };
                            Some((tag, format!(
                                "MA 데드크로스 단기{:.4} < 장기{:.4} | PnL {:+.1}%",
                                short_ma, long_ma, pnl_pct * 100.0
                            )))
                        } else {
                            None
                        }
                    });

                    if let Some((tp_sl, reason)) = trigger {
                        // Use actual token size from position API, not DB estimate
                        let real_token_qty = position.and_then(|p| p.size).unwrap_or(0.0);
                        let token_qty = if real_token_qty > 0.0 {
                            real_token_qty
                        } else {
                            trade.size / trade.price // fallback to DB estimate
                        };
                        // Pass notional as token_qty * price so trader can derive correct maker/taker amounts
                        let exit_notional = token_qty * current_sell_price;
                        info!(trigger = tp_sl, market = %short_q, tokens = format_args!("{:.2}", token_qty), real_tokens = format_args!("{:.2}", real_token_qty), price = format_args!("{:.4}", current_sell_price), notional = format_args!("${:.4}", exit_notional), "Auto exit");

                        let close_success = if let Some(t) = &trader {
                            let exit_order = t
                                .place_market_order(
                                    &trade.token_id,
                                    "SELL",
                                    current_sell_price,
                                    exit_notional,
                                    trade.neg_risk,
                                )
                                .await;
                            let ok = matches!(&exit_order, Ok(resp) if order_succeeded(resp));
                            if !ok {
                                warn!(trigger = tp_sl, market = %short_q, err = ?exit_order, "Auto sell failed");
                            }
                            ok
                        } else {
                            false
                        };

                        if close_success {
                            last_review.remove(&trade.token_id);
                            price_history.remove(&trade.token_id);
                            let resolve_tag = format!("AUTO_{}", tp_sl);
                            let _ = db.resolve_trade(trade.id, &resolve_tag, exit_notional);
                            let profit = exit_notional - trade.size;
                            governor.realized_profit += profit;
                            pos_sold += 1;
                            pos_summary.push(format!("⚡{} {} {:+.1}%", tp_sl, short_q, pnl_pct * 100.0));
                            let msg = format!(
                                "⚡ *Auto {} (API 없음)*\n`{}` {} @ `{:.3}` → `{:.3}`\nInvested: `${:.2}` → Return: `${:.2}` | Profit: `{:+.2}`\nPnL: `{:+.1}%`\n사유: {}",
                                tp_sl, short_q, held_label, trade.price, current_sell_price,
                                trade.size, exit_notional, profit,
                                pnl_pct * 100.0, reason
                            );
                            info!(trigger = tp_sl, market = %short_q, pnl = format_args!("{:+.1}%", pnl_pct * 100.0), profit = format_args!("${:+.2}", profit), "Auto position closed");
                            if let Some(ref n) = notifier {
                                let _ = n.send_message(&msg).await;
                            }
                        }
                    } else {
                        // MA 상태 표시
                        let h = price_history.get(&trade.token_id);
                        let ma_info = if let Some(h) = h {
                            if h.len() >= ma_long_period {
                                let short_ma: f64 = h[h.len()-ma_short_period..].iter().sum::<f64>() / ma_short_period as f64;
                                let long_ma: f64 = h[h.len()-ma_long_period..].iter().sum::<f64>() / ma_long_period as f64;
                                format!("MA({})={:.4} vs MA({})={:.4}", ma_short_period, short_ma, ma_long_period, long_ma)
                            } else {
                                format!("MA 수집중 {}/{}", h.len(), ma_long_period)
                            }
                        } else {
                            "MA 없음".to_string()
                        };
                        debug!(market = %short_q, entry = format_args!("{:.4}", trade.price), now = format_args!("{:.4}", current_sell_price), pnl = format_args!("{:+.1}%", pnl_pct * 100.0), ma = %ma_info, "No API fallback hold");
                    }
                    continue;
                }

                // AI-driven position management: ask AI whether to HOLD or SELL
                if rechecked >= max_position_rechecks {
                    continue;
                }

                // Skip review if price hasn't moved significantly since last review
                if let Some((last_price, last_time)) = last_review.get(&trade.token_id) {
                    let price_change = ((current_sell_price - last_price) / last_price).abs();
                    if price_change < 0.05 && last_time.elapsed().as_secs() < 1800 {
                        debug!(market = %short_q, change = format_args!("{:+.2}%", price_change * 100.0), age_secs = last_time.elapsed().as_secs(), "Skip position review: no significant move");
                        continue;
                    }
                }

                let market_state = build_market_state_for_position(
                    held_label,
                    trade.price,
                    current_sell_price,
                    pnl_pct,
                    trade.entry_probability,
                    trade.entry_edge,
                );

                match analyst
                    .analyze_position(
                        &trade.market_question,
                        trade.market_description.as_deref(),
                        held_label,
                        trade.price,
                        current_sell_price,
                        pnl_pct,
                        &market_state,
                        governor.current_balance,
                        governor.remaining_api_credit(),
                    )
                    .await
                {
                    Ok(decision) => {
                        rechecked += 1;
                        governor.track_api_cost(decision.cost_estimate);
                        let _ = db.log_api_cost(decision.cost_estimate, "position_review");
                        let _ = db.set_runtime_f64("api_credit_left", governor.remaining_api_credit());

                        // Record this review for price-change detection
                        last_review.insert(trade.token_id.clone(), (current_sell_price, std::time::Instant::now()));

                        info!(market = %short_q, label = held_label, pnl = format_args!("{:+.1}%", pnl_pct * 100.0), action = %decision.action, reason = %decision.reasoning, "Position review");

                        if decision.action == "HOLD" {
                            pos_hold += 1;
                            pos_summary.push(format!("📌HOLD {} {:+.1}%", short_q, pnl_pct * 100.0));
                        }

                        if decision.action == "SELL" {
                            let real_token_qty = position.and_then(|p| p.size).unwrap_or(0.0);
                            let token_qty = if real_token_qty > 0.0 {
                                real_token_qty
                            } else {
                                trade.size / trade.price
                            };
                            let exit_notional = token_qty * current_sell_price;

                            let close_success = if let Some(t) = &trader {
                                let exit_order = t
                                    .place_market_order(
                                        &trade.token_id,
                                        "SELL",
                                        current_sell_price,
                                        exit_notional,
                                        trade.neg_risk,
                                    )
                                    .await;
                                let ok = matches!(&exit_order, Ok(resp) if order_succeeded(resp));
                                if !ok {
                                    warn!(market = %short_q, err = ?exit_order, "AI sell failed");
                                }
                                ok
                            } else {
                                false
                            };

                            if close_success {
                                last_review.remove(&trade.token_id);
                                let _ = db.resolve_trade(trade.id, "AI_SELL", exit_notional);
                                let profit = exit_notional - trade.size;
                                pos_sold += 1;
                                pos_summary.push(format!("🤖SELL {} {:+.1}%", short_q, pnl_pct * 100.0));
                                let msg = format!(
                                    "🤖 *AI Position Closed*\n`{}` {} @ `{:.3}` → `{:.3}`\nInvested: `${:.2}` → Return: `${:.2}` | Profit: `{:+.2}`\nPnL: `{:+.1}%`\n사유: {}",
                                    short_q, held_label, trade.price, current_sell_price,
                                    trade.size, exit_notional, profit,
                                    pnl_pct * 100.0, decision.reasoning
                                );
                                info!(market = %short_q, pnl = format_args!("{:+.1}%", pnl_pct * 100.0), profit = format_args!("${:+.2}", profit), "AI position closed");
                                if let Some(ref n) = notifier {
                                    let _ = n.send_message(&msg).await;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if err_str.contains("CREDIT_EXHAUSTED") {
                            warn!("API credit exhausted during position review! Switching to MA/TP/SL fallback.");
                            governor.force_api_credit_zero();
                            let _ = db.set_runtime_f64("api_credit_left", 0.0);
                            break;
                        }
                        warn!(market = %short_q, err = %e, "Position review failed");
                    }
                }
            }
        }

        governor.set_realized_profit(db.get_realized_profit());
        let _ = db.set_runtime_f64("realized_profit", governor.realized_profit);

        let trade_cycle_due = last_trade_cycle.elapsed().as_secs() >= trade_cycle_secs;

        // Keep risk checks running frequently even on low balance.
        // Only throttle low-balance notifications and skip new-entry cycle.
        if governor.current_balance < min_balance_for_any_trade {
            if last_low_balance_notice.elapsed().as_secs() >= low_balance_wait_secs {
                warn!(balance = format_args!("${:.2}", governor.current_balance), required = format_args!("${:.2}", min_balance_for_any_trade), "Low balance: new entries paused, position checks continue");
                if let Some(ref n) = notifier {
                    let _ = n
                        .send_message(&format!(
                            "⏸️ *Cycle Paused (Low Balance)*\nBalance: `${:.2}`\nRequired: `${:.2}` (to keep <= `{:.1}%` risk with `${:.2}` min trade)\nAPI Left: `${:.4}`\nRisk check: `{} sec`\nStatus ping: `{} min`",
                            governor.current_balance,
                            min_balance_for_any_trade,
                            strategy.max_bet_fraction * 100.0,
                            min_trade_usdc,
                            governor.remaining_api_credit(),
                            position_check_secs,
                            low_balance_wait_secs / 60
                        ))
                        .await;
                }
                last_low_balance_notice = std::time::Instant::now();
            }
        } else if trade_cycle_due {
            last_trade_cycle = std::time::Instant::now();
            if let Ok(markets) = scanner.get_active_markets().await {
                let learning = db.get_learning_summary();
                debug!(summary = %learning, "Learning context");

                let mut screened_count = 0;
                let mut analyzed_count = 0;
                let max_screens = max_screens_cfg;
                let max_analyses = max_analyses_cfg;
                let mut traded_count = 0;
                let mut stop_for_api = false;

                for market in markets.iter() {
                    if screened_count >= max_screens || analyzed_count >= max_analyses {
                        break;
                    }

                    if governor.remaining_api_credit() <= 0.0 {
                        stop_for_api = true;
                        break;
                    }

                    // P2-5: Block geopolitics/war markets (AI has no edge here)
                    if is_geopolitics_market(&market.question) {
                        continue;
                    }

                    let (outcome_a_idx, outcome_b_idx) = match binary_yes_no_indices(&market.tokens)
                    {
                        Some(v) => v,
                        None => {
                            debug!("Non-binary or unlabeled market, skipping");
                            continue;
                        }
                    };

                    if outcome_a_idx >= market.tokens.len() || outcome_b_idx >= market.tokens.len()
                    {
                        continue;
                    }

                    let price = match market.tokens.get(outcome_a_idx).and_then(|t| t.price) {
                        Some(p) => p,
                        None => continue,
                    };
                    let market_state = build_market_state(market);

                    // Dynamic edge threshold based on market category
                    let category = classify_market_category(&market.question);
                    let (screen_min_edge, trade_min_edge) = category_edge_thresholds(category);

                    // Stage 1: Quick screen with Haiku (cheap) — with cache
                    let (worth_it, screen_cost) = if let Some((cached_prob, cached_time)) = screen_cache.get(&market.question) {
                        if cached_time.elapsed().as_secs() < screen_cache_ttl_secs {
                            // Cache hit: check if previous screening found edge (category-aware)
                            let cached_edge = (cached_prob - price).abs();
                            if cached_edge < screen_min_edge {
                                // No edge last time → skip entirely (no API cost)
                                debug!(screen = screened_count, max = max_screens, market = %market.question, cat = category, min_edge = format_args!("{:.0}%", screen_min_edge * 100.0), "Screen skip: cached, no edge");
                                continue;
                            }
                            // Had edge before → pass through to analysis (no API cost)
                            (true, 0.0)
                        } else {
                            // Cache expired → normal Haiku screening
                            screened_count += 1;
                            let (w, sp, sc) = analyst
                                .quick_screen(
                                    &market.question,
                                    market.description.as_deref(),
                                    price,
                                    &market_state,
                                    governor.current_balance,
                                    screen_min_edge,
                                )
                                .await
                                .unwrap_or((false, 0.5, 0.0));
                            screen_cache.insert(market.question.clone(), (sp, Instant::now()));
                            governor.track_api_cost(sc);
                            let _ = db.log_api_cost(sc, "screen");
                            let _ = db.set_runtime_f64("api_credit_left", governor.remaining_api_credit());
                            (w, sc)
                        }
                    } else {
                        // Cache miss → normal Haiku screening
                        screened_count += 1;
                        let (w, sp, sc) = analyst
                            .quick_screen(
                                &market.question,
                                market.description.as_deref(),
                                price,
                                &market_state,
                                governor.current_balance,
                                screen_min_edge,
                            )
                            .await
                            .unwrap_or((false, 0.5, 0.0));
                        screen_cache.insert(market.question.clone(), (sp, Instant::now()));
                        governor.track_api_cost(sc);
                        let _ = db.log_api_cost(sc, "screen");
                        let _ = db.set_runtime_f64("api_credit_left", governor.remaining_api_credit());
                        (w, sc)
                    };
                    if screen_cost > 0.0 && governor.remaining_api_credit() <= 0.0 {
                        stop_for_api = true;
                        break;
                    }

                    if !worth_it {
                        debug!(screen = screened_count, max = max_screens, market = %market.question, "Screen skip: no edge");
                        continue;
                    }

                    // Stage 2: Full analysis with Sonnet (for promising markets)
                    // Skip if already analyzed within 24 hours
                    if db.has_recent_analysis(&market.question, 86400) {
                        debug!(analyzed = analyzed_count, max = max_analyses, market = %market.question, "Skip: analyzed within 24h");
                        continue;
                    }
                    if governor.remaining_api_credit() <= 0.0 {
                        stop_for_api = true;
                        break;
                    }
                    analyzed_count += 1;
                    info!(n = analyzed_count, max = max_analyses, market = %market.question, "Analyzing market");

                    match analyst
                        .analyze_market(
                            &market.question,
                            market.description.as_deref(),
                            price,
                            &market_state,
                            governor.current_balance,
                            governor.remaining_api_credit(),
                            &learning,
                            trade_min_edge,
                        )
                        .await
                    {
                        Ok(analysis) => {
                            governor.track_api_cost(analysis.cost_estimate);
                            let _ = db.log_api_cost(analysis.cost_estimate, "opus_analysis");
                            let _ = db.set_runtime_f64(
                                "api_credit_left",
                                governor.remaining_api_credit(),
                            );
                            let _ = db.log_analysis(
                                &market.question,
                                analysis.probability,
                                &analysis.reasoning,
                                analysis.cost_estimate,
                            );
                            if governor.remaining_api_credit() <= 0.0 {
                                stop_for_api = true;
                                break;
                            }

                            // P2-7: Overconfidence shrink — pull AI prob toward market price
                            let shrink_factor = 0.6;
                            let adjusted_prob = price + (analysis.probability - price) * shrink_factor;
                            let edge = (adjusted_prob - price).abs();
                            // Dynamic edge threshold: category-specific trade_min_edge
                            let math_action = if edge < trade_min_edge {
                                "SKIP"
                            } else if adjusted_prob > price {
                                "BUY"
                            } else {
                                "SELL"
                            };

                            let kelly_bet =
                                strategy.calculate_kelly_bet_with_edge(price, adjusted_prob, trade_min_edge);
                            let final_bet_fraction = if kelly_bet > 0.0 && edge >= trade_min_edge {
                                kelly_bet // Already scaled by kelly_fraction (default half-Kelly)
                            } else {
                                0.0 // Kelly says no → skip
                            };

                            let final_action = math_action;

                            info!(action = final_action, ai_said = %analysis.action, ai_prob = format_args!("{:.1}%", analysis.probability * 100.0), adj_prob = format_args!("{:.1}%", adjusted_prob * 100.0), price = format_args!("{:.2}", price), edge = format_args!("{:.1}%", edge * 100.0), min_edge = format_args!("{:.0}%", trade_min_edge * 100.0), cat = category, bet = format_args!("{:.1}%", final_bet_fraction * 100.0), kelly = format_args!("{:.1}%", kelly_bet * 100.0), cost = format_args!("${:.4}", analysis.cost_estimate), "Analysis result");

                            if final_action == "SKIP" || final_bet_fraction <= 0.0 {
                                continue;
                            }

                            // final_action=BUY => buy Outcome A, SELL => buy Outcome B.
                            let (token_idx, decision_label) = if final_action == "BUY" {
                                let label = market.tokens[outcome_a_idx]
                                    .outcome_label
                                    .as_deref()
                                    .unwrap_or("YES");
                                (outcome_a_idx, format!("BUY_{}", label.to_ascii_uppercase()))
                            } else {
                                if market.tokens.len() < 2
                                    || outcome_b_idx >= market.tokens.len()
                                    || outcome_b_idx == outcome_a_idx
                                {
                                    debug!("No Outcome B token available, skipping");
                                    continue;
                                }
                                let label = market.tokens[outcome_b_idx]
                                    .outcome_label
                                    .as_deref()
                                    .unwrap_or("NO");
                                (outcome_b_idx, format!("BUY_{}", label.to_ascii_uppercase()))
                            };

                            let token = &market.tokens[token_idx];
                            let order_price = match token.price {
                                Some(p) if (0.0..1.0).contains(&p) => p,
                                _ => {
                                    debug!(label = %decision_label, "Missing token price, skipping");
                                    continue;
                                }
                            };

                            if final_bet_fraction > strategy.max_bet_fraction {
                                warn!(bet = format_args!("{:.2}%", final_bet_fraction * 100.0), max = format_args!("{:.2}%", strategy.max_bet_fraction * 100.0), "Bet fraction exceeds max, skipping");
                                continue;
                            }

                            let bet_amount_usdc = governor.current_balance * final_bet_fraction;
                            if bet_amount_usdc < min_trade_usdc {
                                debug!(bet = format_args!("${:.2}", bet_amount_usdc), min = format_args!("${:.2}", min_trade_usdc), "Bet below min trade, skipping");
                                continue;
                            }

                            if bet_amount_usdc > governor.current_balance {
                                warn!(bet = format_args!("${:.2}", bet_amount_usdc), balance = format_args!("${:.2}", governor.current_balance), "Insufficient balance");
                                continue;
                            }

                            if db.has_pending_trade_for_token(&token.token_id) {
                                debug!(token = %token.token_id, "Already holding pending position, skipping");
                                continue;
                            }

                            // Block duplicate entry on same market question (any side)
                            if db.has_pending_trade_for_question(&market.question) {
                                debug!(market = %market.question, "Already holding position on this market, skipping");
                                continue;
                            }

                            // P2-6: Category position limit (max 2 per category)
                            {
                                let cat = classify_market_category(&market.question);
                                let pending = db.get_pending_trades();
                                let cat_count = pending.iter()
                                    .filter(|t| classify_market_category(&t.market_question) == cat)
                                    .count();
                                if cat_count >= 2 {
                                    debug!(category = cat, count = cat_count, "Category position limit reached, skipping");
                                    continue;
                                }
                            }

                            let side = "BUY";
                            let outcome_label = match token.outcome_label.as_deref() {
                                Some(label) if is_yes_label(label) || is_no_label(label) => label,
                                _ => {
                                    debug!(label = %decision_label, "Unknown outcome label, skipping");
                                    continue;
                                }
                            };
                            let entry_prob_for_token =
                                match probability_for_label(analysis.probability, outcome_label) {
                                    Some(v) => v,
                                    None => {
                                        debug!(label = %outcome_label, "Could not map probability, skipping");
                                        continue;
                                    }
                                };
                            let entry_edge = entry_prob_for_token - order_price;

                            if let Some(ref n) = notifier {
                                let _ = n.send_message(&format!(
                                "🎯 *AI Decision: {}*\nMarket: `{}`\nAI: `{:.1}%` vs Price: `{:.2}`\nBet: `${:.2}` ({:.1}%)\nKelly: {:.1}%\nEntry Edge: `{:+.1}%`\n사유: {}",
                                decision_label, market.question, analysis.probability * 100.0, price,
                                bet_amount_usdc, final_bet_fraction * 100.0, kelly_bet * 100.0, entry_edge * 100.0, analysis.reasoning
                            )).await;
                            }

                            if let Some(t) = &trader {
                                let order_res = t
                                    .place_market_order(
                                        &token.token_id,
                                        side,
                                        order_price,
                                        bet_amount_usdc,
                                        market.neg_risk,
                                    )
                                    .await;

                                let status = match &order_res {
                                    Ok(resp) if order_succeeded(resp) => "SUCCESS",
                                    _ => "FAILED",
                                };
                                let _ = db.log_trade(
                                    &market.question,
                                    market.description.as_deref(),
                                    &token.token_id,
                                    side,
                                    order_price,
                                    bet_amount_usdc,
                                    market.neg_risk,
                                    Some(outcome_label),
                                    Some(entry_prob_for_token),
                                    Some(entry_edge),
                                    Some(analysis.reasoning.as_str()),
                                    status,
                                );
                                if status == "SUCCESS" {
                                    traded_count += 1;
                                }

                                if let Some(ref n) = notifier {
                                    match &order_res {
                                        Ok(resp) if order_succeeded(resp) => {
                                            let _ = n.send_message(&format!(
                                            "✅ *Order Filled!*\n{} `{}` @ `{:.4}`\nAmount: `${:.2}` | ID: `{}`",
                                            decision_label, market.question, order_price, bet_amount_usdc,
                                            resp.order_id.as_deref().unwrap_or("N/A")
                                        )).await;
                                        }
                                        Ok(resp) => {
                                            let err = resp.error.as_deref().unwrap_or(
                                                resp.status
                                                    .as_deref()
                                                    .unwrap_or("Unknown exchange error"),
                                            );
                                            let is_fok = err.contains("fully filled") || err.contains("FOK");
                                            let is_balance = err.contains("not enough balance");
                                            if is_fok {
                                                let _ = n.send_message(&format!(
                                                "⏭️ *Skip (유동성 부족)*\n`{}` | `{}`",
                                                market.question, decision_label
                                            )).await;
                                            } else if is_balance {
                                                let _ = n.send_message(&format!(
                                                "⏭️ *Skip (잔고 부족)*\n`{}` | `{}`",
                                                market.question, decision_label
                                            )).await;
                                            } else {
                                                let _ = n.send_message(&format!(
                                                "❌ *Order Rejected!*\nMarket: `{}`\nType: `{}`\nError: `{}`",
                                                market.question, decision_label, err
                                            )).await;
                                            }
                                        }
                                        Err(e) => {
                                            let _ = n
                                                .send_message(&format!(
                                                    "❌ *Order Failed!*\nMarket: `{}`\nError: `{}`",
                                                    market.question, e
                                                ))
                                                .await;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let err_str = e.to_string();
                            // Detect credit exhaustion → trigger auto recharge
                            if err_str.contains("CREDIT_EXHAUSTED") {
                                warn!("API credit exhausted! Switching to fallback + triggering auto-recharge");
                                governor.force_api_credit_zero();
                                let _ = db.set_runtime_f64("api_credit_left", 0.0);
                                if let Some(ref mut r) = recharger {
                                    if r.needs_recharge(0.0, governor.current_balance) {
                                        match r.full_recharge_pipeline().await {
                                            Ok((tx_hash, amount)) => {
                                                if let Some(ref n) = notifier {
                                                    let _ = n.send_message(&format!(
                                                        "⚡ *API 크레딧 소진 → 자동 충전!*\nRedotPay 입금: `${:.2}`\nTX: `{}`\n\n5분 후 재시도합니다...",
                                                        amount, tx_hash
                                                    )).await;
                                                }
                                                // Wait for Anthropic auto-reload
                                                sleep(Duration::from_secs(300)).await;
                                                governor.initial_api_credit += 15.0;
                                                let _ = db.set_runtime_f64("api_credit_seed", governor.initial_api_credit);
                                            }
                                            Err(re) => {
                                                warn!(err = %re, "Auto-recharge failed");
                                            }
                                        }
                                    }
                                }
                                break; // break out of market loop, will retry next cycle
                            }
                            error!(err = ?e, "Analysis error");
                            if let Some(ref n) = notifier {
                                let _ = n
                                    .send_message(&format!("❌ *Analysis Failed:* `{}`", e))
                                    .await;
                            }
                        }
                    }
                }

                if stop_for_api {
                    let _ = db.set_runtime_f64("api_credit_left", 0.0);
                    warn!("API credit depleted during cycle. Will wait for auto-reload.");
                }

                info!(screened = screened_count, analyzed = analyzed_count, traded = traded_count, "Cycle done");
                if let Some(ref n) = notifier {
                    let pos_info = if pos_sold > 0 || pos_hold > 0 {
                        format!(
                            "\n📋 포지션: SELL `{}` | HOLD `{}` / `{}`개",
                            pos_sold, pos_hold, pending_trades.len()
                        )
                    } else {
                        String::new()
                    };
                    let pos_detail = if !pos_summary.is_empty() {
                        format!("\n{}", pos_summary.iter().map(|s| format!("`{}`", s)).collect::<Vec<_>>().join("\n"))
                    } else {
                        String::new()
                    };
                    let _ = n
                        .send_message(&format!(
                            "🔁 *Cycle Done*\nScreened: `{}` | Analyzed: `{}` | Traded: `{}`\nBalance: `${:.2}` | API Left: `${:.4}`{}{}",
                            screened_count,
                            analyzed_count,
                            traded_count,
                            governor.current_balance,
                            governor.remaining_api_credit(),
                            pos_info,
                            pos_detail
                        ))
                        .await;
                }
            }
        }

        db.checkpoint();
        sleep(Duration::from_secs(position_check_secs)).await;
    }

    ratatui::restore();
    Ok(())
}
