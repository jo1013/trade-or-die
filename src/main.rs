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
use std::time::Duration;
use tokio::time::sleep;

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
    let strategy = Strategy::new(0.08, max_bet_frac); // min_edge 8%, max_bet 10%
    let trade_cycle_secs = env_u64("TRADE_CYCLE_SECONDS", 600).max(30);
    let position_check_secs = env_u64("POSITION_CHECK_SECONDS", 60).max(10);
    let report_interval_secs = env_u64("REPORT_INTERVAL_SECONDS", 14400).max(300);
    let low_balance_wait_secs = env_u64("LOW_BALANCE_WAIT_SECONDS", trade_cycle_secs).max(30);
    let max_screens_cfg = env_u64("MAX_SCREENS_PER_CYCLE", 500).clamp(500, 1000) as usize;
    let max_analyses_cfg = env_u64("MAX_ANALYSES_PER_CYCLE", 50)
        .max(1)
        .min(max_screens_cfg as u64) as usize;
    let max_position_rechecks = env_u64("MAX_POSITION_RECHECKS", 8).max(1) as usize;
    let min_trade_usdc = env_f64("MIN_TRADE_USDC", 1.0).clamp(0.5, 100.0);
    let min_balance_for_any_trade = min_trade_usdc;

    println!(
        "Config: cycle={}s, pos_check={}s, report={}s, screens={}, analyses={}, rechecks={}, min_trade=${:.2}, min_bal_for_trade=${:.2}",
        trade_cycle_secs,
        position_check_secs,
        report_interval_secs,
        max_screens_cfg,
        max_analyses_cfg,
        max_position_rechecks,
        min_trade_usdc,
        min_balance_for_any_trade,
    );

    // Restore cumulative API costs from DB
    let prev_api_costs = db.get_total_api_cost();
    governor.api_costs = prev_api_costs;
    println!("Restored API costs from DB: ${:.4}", prev_api_costs);
    let realized_profit = db.get_realized_profit();
    governor.set_realized_profit(realized_profit);
    println!(
        "Restored realized trade PnL from DB: {:+.2}",
        realized_profit
    );

    // API seed budget: env override takes priority over DB (for manual resets)
    // API_CREDIT_SEED means "actual remaining credit right now".
    // Since remaining = seed - db_costs + profit, we must compensate for restored DB costs.
    if let Some(env_seed) = env_opt_f64("API_CREDIT_SEED") {
        let actual_remaining = env_seed.max(0.0);
        let compensated = actual_remaining + prev_api_costs - realized_profit.max(0.0);
        governor.initial_api_credit = compensated.max(0.0);
        let _ = db.set_runtime_f64("api_credit_seed", governor.initial_api_credit);
        println!(
            "API credit: remaining=${:.4} (seed=${:.4}, db_costs=${:.4})",
            actual_remaining, governor.initial_api_credit, prev_api_costs
        );
    } else if let Some(seed) = db.get_runtime_f64("api_credit_seed") {
        governor.initial_api_credit = seed.max(0.0);
        println!(
            "Restored API seed credit from DB: ${:.4}",
            governor.initial_api_credit
        );
    } else if let Some(db_left_legacy) = db.get_runtime_f64("api_credit_left") {
        let migrated_seed =
            (db_left_legacy.max(0.0) + governor.api_costs - realized_profit.max(0.0)).max(0.0);
        governor.initial_api_credit = migrated_seed;
        let _ = db.set_runtime_f64("api_credit_seed", migrated_seed);
        println!(
            "Migrated legacy API budget from left=${:.4} -> seed=${:.4}",
            db_left_legacy, migrated_seed
        );
    } else if let Some(env_left) = env_opt_f64("API_CREDIT_LEFT") {
        governor.initial_api_credit = env_left.max(0.0);
        let _ = db.set_runtime_f64("api_credit_seed", governor.initial_api_credit);
        println!(
            "Initialized API seed credit from env API_CREDIT_LEFT: ${:.4}",
            governor.initial_api_credit
        );
    } else {
        governor.initial_api_credit = governor.initial_api_credit.max(0.0);
        let _ = db.set_runtime_f64("api_credit_seed", governor.initial_api_credit);
        println!(
            "Initialized API seed credit from INITIAL_API_CREDIT: ${:.4}",
            governor.initial_api_credit
        );
    }

    println!("Fetching initial USDC balance for proxy {:?}...", proxy_address);
    let initial_balance = governor.fetch_real_balance().await.unwrap_or(0.0);
    governor.initial_balance = initial_balance;
    println!("Initial Balance: ${:.2}", initial_balance);

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
            println!("⚠️ POLYMARKET API KEYS NOT FOUND. TRADING MODE: SIMULATION ONLY.");
            None
        }
    };

    let mut recharger = match Recharger::new(proxy_address) {
        Ok(r) => {
            println!(
                "⚡ Recharger enabled: threshold=${:.2}, amount=${:.2}, cooldown={}s",
                r.recharge_threshold, r.recharge_amount, r.cooldown_secs
            );
            Some(r)
        }
        Err(e) => {
            println!("⚠️ Recharger init failed: {}. Auto-recharge disabled.", e);
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
                            Err(e) => println!("⚠️ Import failed for '{}': {}", pos.title.as_deref().unwrap_or("?"), e),
                        }
                    }
                }
            }
        }
        if imported > 0 {
            println!("📦 Imported {} existing positions into DB", imported);
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

    let mut terminal = ratatui::init();
    let mut last_report = std::time::Instant::now();
    let mut last_trade_cycle = std::time::Instant::now() - Duration::from_secs(trade_cycle_secs);
    let mut last_low_balance_notice =
        std::time::Instant::now() - Duration::from_secs(low_balance_wait_secs);

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
            println!("⚠️ Balance fetch failed: {}", e);
        }
        let _ = db.log_balance(governor.current_balance);
        governor.set_realized_profit(db.get_realized_profit());
        println!(
            "DEBUG: Balance: ${:.2} | Realized: {:+.2} | API Left: ${:.4}",
            governor.current_balance,
            governor.realized_profit,
            governor.remaining_api_credit()
        );
        let _ = db.set_runtime_f64("api_credit_left", governor.remaining_api_credit());
        let _ = db.set_runtime_f64("realized_profit", governor.realized_profit);
        ui::draw_ui(&mut terminal, &governor.survival_stats())?;

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
                println!(
                    "⚡ API credit low (${:.4}), starting auto-recharge pipeline...",
                    governor.remaining_api_credit()
                );
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
                        println!("⚠️ Auto-recharge pipeline failed: {}", e);
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

        // API credit exhausted -> DON'T die, wait for auto-recharge
        // Real credit check happens via API call failures (CREDIT_EXHAUSTED)
        if governor.remaining_api_credit() <= 0.0 {
            let _ = db.set_runtime_f64("api_credit_left", 0.0);
            println!("⚠️ Internal API credit counter depleted. Waiting for auto-reload...");
            // Don't break - wait and check again. Anthropic auto-reload may have kicked in.
            sleep(Duration::from_secs(300)).await;
            // Reset counter optimistically after auto-reload
            governor.initial_api_credit += 15.0;
            let _ = db.set_runtime_f64("api_credit_seed", governor.initial_api_credit);
            if let Some(ref n) = notifier {
                let _ = n
                    .send_message("⚡ API credit counter reset (+$15). Checking if auto-reload worked...")
                    .await;
            }
            continue;
        }

        // Check pending positions: resolve finished markets + AI-driven HOLD/SELL
        let pending_trades = db.get_pending_trades();
        let total_trades_count = db.debug_trade_counts();
        println!("DEBUG: {} pending trades (total in DB: {})", pending_trades.len(), total_trades_count);
        if !pending_trades.is_empty() {
            println!(
                "Checking {} pending positions...",
                pending_trades.len()
            );
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
                        println!("{}", msg);
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

                // AI-driven position management: ask AI whether to HOLD or SELL
                if rechecked >= max_position_rechecks || governor.remaining_api_credit() <= 0.0 {
                    continue;
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

                        println!(
                            "  [Position] {} | {} | PnL: {:+.1}% | AI: {} | {}",
                            short_q, held_label, pnl_pct * 100.0, decision.action, decision.reasoning
                        );

                        if decision.action == "SELL" {
                            let token_qty = trade.size / trade.price;
                            let sell_fill_price = (current_sell_price * 0.95).max(0.01);
                            let exit_notional = token_qty * sell_fill_price;

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
                                    println!("  ⚠️ AI sell failed on `{}`: {:?}", short_q, exit_order);
                                }
                                ok
                            } else {
                                false
                            };

                            if close_success {
                                let _ = db.resolve_trade(trade.id, "AI_SELL", exit_notional);
                                let msg = format!(
                                    "🤖 *AI Position Closed*\n`{}` {} @ `{:.3}` → `{:.3}`\nPnL: `{:+.1}%` | Cash: `${:.2}`\n사유: {}",
                                    short_q, held_label, trade.price, current_sell_price,
                                    pnl_pct * 100.0, exit_notional, decision.reasoning
                                );
                                println!("{}", msg);
                                if let Some(ref n) = notifier {
                                    let _ = n.send_message(&msg).await;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if err_str.contains("CREDIT_EXHAUSTED") {
                            println!("💳 API credit exhausted during position review!");
                            break;
                        }
                        println!("  ⚠️ Position review failed for `{}`: {}", short_q, e);
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
                println!(
                    "⏸️ Balance ${:.2} < ${:.2}. Cannot place legal <= {:.1}% bet with min trade ${:.2}. New entries paused; position risk checks continue every {}s.",
                    governor.current_balance,
                    min_balance_for_any_trade,
                    strategy.max_bet_fraction * 100.0,
                    min_trade_usdc,
                    position_check_secs
                );
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
                println!("LEARNING: {}", learning);

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

                    let (outcome_a_idx, outcome_b_idx) = match binary_yes_no_indices(&market.tokens)
                    {
                        Some(v) => v,
                        None => {
                            println!("  -> Non-binary or unlabeled market, skipping");
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

                    // Stage 1: Quick screen with Haiku (cheap)
                    screened_count += 1;
                    let (worth_it, _screen_prob, screen_cost) = analyst
                        .quick_screen(
                            &market.question,
                            market.description.as_deref(),
                            price,
                            &market_state,
                            governor.current_balance,
                        )
                        .await
                        .unwrap_or((false, 0.5, 0.0));
                    governor.track_api_cost(screen_cost);
                    let _ = db.log_api_cost(screen_cost, "screen");
                    let _ = db.set_runtime_f64("api_credit_left", governor.remaining_api_credit());
                    if governor.remaining_api_credit() <= 0.0 {
                        stop_for_api = true;
                        break;
                    }

                    if !worth_it {
                        println!(
                            "[screen {}/{}] SKIP {}",
                            screened_count, max_screens, market.question
                        );
                        continue;
                    }

                    // Stage 2: Full analysis with Sonnet (for promising markets)
                    if governor.remaining_api_credit() <= 0.0 {
                        stop_for_api = true;
                        break;
                    }
                    analyzed_count += 1;
                    println!(
                        "[{}/{}] >> {}",
                        analyzed_count, max_analyses, market.question
                    );

                    match analyst
                        .analyze_market(
                            &market.question,
                            market.description.as_deref(),
                            price,
                            &market_state,
                            governor.current_balance,
                            governor.remaining_api_credit(),
                            &learning,
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

                            // Derive direction from MATH, not AI text (AI often gets direction wrong)
                            let edge = (analysis.probability - price).abs();
                            let math_action = if edge < 0.08 {
                                "SKIP"
                            } else if analysis.probability > price {
                                "BUY"
                            } else {
                                "SELL"
                            };

                            let kelly_bet =
                                strategy.calculate_kelly_bet(price, analysis.probability);
                            let final_bet_fraction = if kelly_bet > 0.0 && edge >= 0.08 {
                                // Use max of AI suggestion and half-Kelly
                                analysis.bet_fraction.max(kelly_bet * 0.5).min(strategy.max_bet_fraction)
                            } else if analysis.action != "SKIP" && analysis.bet_fraction > 0.0 {
                                // AI wants to trade but Kelly disagrees - use AI at reduced size
                                (analysis.bet_fraction * 0.5).min(0.05)
                            } else {
                                0.0
                            };

                            let final_action = math_action;

                            println!("  -> {} (AI said {}) | AI: {:.1}% | Price: {:.2} | Edge: {:.1}% | Bet: {:.1}% (Kelly: {:.1}%) | Cost: ${:.4}",
                            final_action, analysis.action, analysis.probability * 100.0, price,
                            edge * 100.0, final_bet_fraction * 100.0, kelly_bet * 100.0, analysis.cost_estimate);

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
                                    println!("  -> No Outcome B token available, skipping");
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
                                    println!("  -> Missing token price for {}", decision_label);
                                    continue;
                                }
                            };

                            if final_bet_fraction > strategy.max_bet_fraction {
                                println!(
                                    "  ⚠️ Bet fraction {:.2}% exceeds max {:.2}% (skip)",
                                    final_bet_fraction * 100.0,
                                    strategy.max_bet_fraction * 100.0
                                );
                                continue;
                            }

                            let bet_amount_usdc = governor.current_balance * final_bet_fraction;
                            if bet_amount_usdc < min_trade_usdc {
                                println!(
                                    "  -> Bet ${:.2} below min trade ${:.2} under {:.1}% cap, skipping",
                                    bet_amount_usdc,
                                    min_trade_usdc,
                                    strategy.max_bet_fraction * 100.0
                                );
                                continue;
                            }

                            if bet_amount_usdc > governor.current_balance {
                                println!(
                                    "  ⚠️ Insufficient balance for ${:.2} bet (have ${:.2})",
                                    bet_amount_usdc, governor.current_balance
                                );
                                continue;
                            }

                            if db.has_pending_trade_for_token(&token.token_id) {
                                println!(
                                "  -> Already holding pending position on token {}, skipping duplicate entry",
                                token.token_id
                            );
                                continue;
                            }

                            let side = "BUY";
                            let outcome_label = match token.outcome_label.as_deref() {
                                Some(label) if is_yes_label(label) || is_no_label(label) => label,
                                _ => {
                                    println!(
                                        "  -> Unknown outcome label, skipping {}",
                                        decision_label
                                    );
                                    continue;
                                }
                            };
                            let entry_prob_for_token =
                                match probability_for_label(analysis.probability, outcome_label) {
                                    Some(v) => v,
                                    None => {
                                        println!(
                                            "  -> Could not map probability for label {}, skipping",
                                            outcome_label
                                        );
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
                                            let _ = n.send_message(&format!(
                                            "❌ *Order Rejected!*\nMarket: `{}`\nType: `{}`\nError: `{}`",
                                            market.question, decision_label, err
                                        )).await;
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
                                println!("💳 API credit exhausted! Triggering auto-recharge...");
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
                                                println!("⚠️ Auto-recharge failed: {}", re);
                                            }
                                        }
                                    }
                                }
                                break; // break out of market loop, will retry next cycle
                            }
                            println!("  ❌ Analysis Error: {:?}", e);
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
                    println!("⚠️ API credit depleted during cycle. Will wait for auto-reload.");
                }

                println!(
                    "--- Cycle done: screened={}, analyzed={}, traded={} ---",
                    screened_count, analyzed_count, traded_count
                );
                if let Some(ref n) = notifier {
                    let _ = n
                        .send_message(&format!(
                            "🔁 *Cycle Done*\nScreened: `{}` | Analyzed: `{}` | Traded: `{}`\nBalance: `${:.2}` | API Left: `${:.4}`",
                            screened_count,
                            analyzed_count,
                            traded_count,
                            governor.current_balance,
                            governor.remaining_api_credit()
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
