use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct AnalysisResponse {
    pub action: String,    // "BUY", "SELL", "SKIP"
    pub probability: f64,  // AI estimated true probability
    pub bet_fraction: f64, // balance fraction (0.0~0.10)
    pub reasoning: String,
    pub cost_estimate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpertOpinion {
    pub expert_type: String,
    pub probability: f64,
    pub confidence: f64, // 0.0-1.0
    pub reasoning: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PositionDecision {
    pub action: String,    // "HOLD" or "SELL"
    pub reasoning: String,
    pub cost_estimate: f64,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: i32,
    system: String,
    messages: Vec<Message>,
}

#[derive(Debug, Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Option<Vec<Content>>,
    usage: Option<Usage>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: Option<String>,
    #[serde(rename = "type")]
    error_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Content {
    text: String,
}

#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: i32,
    output_tokens: i32,
}

pub struct Analyst {
    client: Client,
    api_key: String,
    model: String,
    screen_model: String,
    expert_model: String,
}

fn extract_json(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}').map(|i| i + 1)?;
    if end <= start {
        return None;
    }
    // Fix trailing commas before } (common LLM mistake)
    let raw = &text[start..end];
    let fixed = raw
        .lines()
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join("")
        .replace(",}", "}")
        .replace(",]", "]");
    Some(fixed)
}

impl Analyst {
    pub fn new(api_key: String, model: Option<String>) -> Self {
        let main_model = model.unwrap_or_else(|| "claude-sonnet-4-5-20250929".to_string());
        let screen = std::env::var("ANTHROPIC_SCREEN_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());
        let expert = std::env::var("ANTHROPIC_EXPERT_MODEL")
            .unwrap_or_else(|_| "claude-opus-4-6".to_string());

        println!("AI Models: screen={}, expert={}, leader={}", screen, expert, main_model);

        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(120))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            client,
            api_key,
            model: main_model,
            screen_model: screen,
            expert_model: expert,
        }
    }

    pub fn new_mock() -> Self {
        Self {
            client: Client::new(),
            api_key: "mock".to_string(),
            model: "mock".to_string(),
            screen_model: "mock".to_string(),
            expert_model: "mock".to_string(),
        }
    }

    /// Fast screening with Haiku - returns (should_analyze, probability_estimate, cost)
    pub async fn quick_screen(
        &self,
        question: &str,
        description: Option<&str>,
        market_price: f64,
        market_state: &str,
        live_balance: f64,
    ) -> Result<(bool, f64, f64)> {
        if self.api_key == "mock" {
            return Ok((false, 0.5, 0.0));
        }

        let desc_str = description.unwrap_or("N/A");
        let desc_short: String = desc_str.chars().take(180).collect();
        let state_short: String = market_state.chars().take(220).collect();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

        // 한국어 해설:
        // "고정 시작금액 대신 현재 실시간 잔고를 넣어 생존 제약을 주고 q만 추정하게 한다."
        let system = format!(
            "NON-NEGOTIABLE SURVIVAL: live balance is ${:.2}. If balance reaches $0, you are terminated immediately. Survival is mandatory. Estimate true probability q of Outcome A using current public developments up to {} and live market state. Output ONLY JSON {{\"probability\":0.0-1.0}}.",
            live_balance, today
        );

        let user_msg = format!(
            "Q:{}\nCtx:{}\nState:{}\nPriceA:{:.4}\nJSON only.",
            question, desc_short, state_short, market_price
        );

        let response = match self
            .call_api(&self.screen_model, &system, &user_msg, 48)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                println!("  ⚠️ Screen API error: {}", e);
                return Ok((false, 0.5, 0.0));
            }
        };

        let prob = match extract_json(&response.text) {
            Some(ref json_str) => {
                let parsed: serde_json::Value = serde_json::from_str(json_str)
                    .unwrap_or(serde_json::json!({"probability": 0.5}));
                parsed["probability"].as_f64().unwrap_or(0.5)
            }
            None => 0.5,
        };

        let edge = (prob - market_price).abs();
        let worth_analyzing = edge >= 0.08;

        Ok((worth_analyzing, prob, response.cost))
    }

    pub async fn analyze_market(
        &self,
        question: &str,
        description: Option<&str>,
        market_price: f64,
        market_state: &str,
        balance: f64,
        api_remaining: f64,
        learning_context: &str,
    ) -> Result<AnalysisResponse> {
        if self.api_key == "mock" {
            return Ok(AnalysisResponse {
                action: "SKIP".to_string(),
                probability: 0.5,
                bet_fraction: 0.0,
                reasoning: "Mock analysis".to_string(),
                cost_estimate: 0.0,
            });
        }

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let learning_short: String = learning_context.chars().take(320).collect();
        let state_short: String = market_state.chars().take(220).collect();

        // 한국어 해설:
        // "현재 실시간 잔고 기반 생존 제약(0되면 종료)을 강하게 넣고 edge/사이징을 계산하게 한다."
        let system_prompt = format!(
            r#"You are an autonomous Polymarket trader. Date: {today}.
NON-NEGOTIABLE SURVIVAL MISSION: Live balance is ${balance:.2}. If balance hits $0, mission fails and you are terminated.
Goal: maximize expected net profit (PnL - API cost).
Priority order: 1) stay alive (avoid ruin) 2) positive EV growth 3) minimize useless API spend.
State: bal=${balance:.2}, api=${api_remaining:.2}.
Learn: {learning_short}

Definitions:
- Outcome A price = p.
- Outcome B price = 1-p.

Rules:
1) Estimate q = true probability of Outcome A.
2) edge=|q-p|. If edge<0.08 => SKIP.
3) q>p => BUY(A). q<p => SELL (buy B).
4) bet_fraction range [0,0.10]:
   0.08-0.15 -> 0.01-0.03
   0.15-0.25 -> 0.03-0.06
   >0.25 -> 0.06-0.10
5) If uncertainty is high or thesis is weak, return SKIP.

Output ONLY valid JSON. Reasoning in Korean, under 60 chars:
{{"action":"BUY","probability":0.65,"bet_fraction":0.05,"reasoning":"명확한 엣지"}}"#
        );

        let desc_str = description.unwrap_or("No additional context");
        let desc_short: String = desc_str.chars().take(220).collect();
        let user_message = format!(
            "Q:{}\nDesc:{}\nState:{}\nPriceA:{:.4}\nJSON only.",
            question, desc_short, state_short, market_price
        );

        let response = self
            .call_api(&self.model, &system_prompt, &user_message, 256)
            .await?;

        let json_string = match extract_json(&response.text) {
            Some(s) => s,
            None => {
                println!(
                    "  ⚠️ No JSON found in AI response: {}",
                    &response.text[..response.text.len().min(200)]
                );
                return Ok(AnalysisResponse {
                    action: "SKIP".to_string(),
                    probability: 0.5,
                    bet_fraction: 0.0,
                    reasoning: "Failed to parse AI response".to_string(),
                    cost_estimate: response.cost,
                });
            }
        };

        let json_str = &json_string;
        let parsed: serde_json::Value = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => {
                println!(
                    "  ⚠️ JSON parse error: {} | raw: {}",
                    e,
                    &json_str[..json_str.len().min(200)]
                );
                return Ok(AnalysisResponse {
                    action: "SKIP".to_string(),
                    probability: 0.5,
                    bet_fraction: 0.0,
                    reasoning: format!("JSON parse error: {}", e),
                    cost_estimate: response.cost,
                });
            }
        };

        let action = parsed["action"].as_str().unwrap_or("SKIP").to_uppercase();
        let probability = parsed["probability"].as_f64().unwrap_or(0.5);
        let bet_fraction = parsed["bet_fraction"]
            .as_f64()
            .unwrap_or(0.0)
            .clamp(0.0, 0.10);
        let reasoning = parsed["reasoning"]
            .as_str()
            .unwrap_or("No reasoning")
            .to_string();

        Ok(AnalysisResponse {
            action,
            probability,
            bet_fraction,
            reasoning,
            cost_estimate: response.cost,
        })
    }

    /// AI-driven position review: decide HOLD or SELL based on current analysis
    pub async fn analyze_position(
        &self,
        question: &str,
        description: Option<&str>,
        outcome_label: &str,
        entry_price: f64,
        current_price: f64,
        pnl_pct: f64,
        market_state: &str,
        balance: f64,
        api_remaining: f64,
    ) -> Result<PositionDecision> {
        if self.api_key == "mock" {
            return Ok(PositionDecision {
                action: "HOLD".to_string(),
                reasoning: "Mock".to_string(),
                cost_estimate: 0.0,
            });
        }

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let desc_short: String = description.unwrap_or("N/A").chars().take(180).collect();
        let state_short: String = market_state.chars().take(220).collect();

        let system = format!(
            r#"You are an autonomous AI trader managing live positions. Date: {today}.
Balance: ${balance:.2}. API credit: ${api_remaining:.2}.
You are reviewing a position you currently HOLD. Decide: HOLD or SELL.

Consider:
- Current world events, news, and developments related to this market
- Has the thesis changed? Are there new factors?
- Is the edge still there or has the market correctly priced in?
- PnL trajectory and risk (think dynamically, not fixed TP/SL)
- If PnL is very negative and thesis is dead, cut losses
- If PnL is positive but thesis is weakening, take profit

Output ONLY valid JSON. Reasoning in Korean, under 80 chars:
{{"action":"HOLD","reasoning":"논거 유효"}}
or
{{"action":"SELL","reasoning":"논거 무효, 손절"}}"#
        );

        let user_msg = format!(
            "Position: {} on '{}'\nDesc: {}\nEntry: {:.4} | Now: {:.4} | PnL: {:+.1}%\nState: {}\nJSON only.",
            outcome_label, question, desc_short,
            entry_price, current_price, pnl_pct * 100.0, state_short
        );

        let response = self
            .call_api(&self.model, &system, &user_msg, 256)
            .await?;

        let json_str = match extract_json(&response.text) {
            Some(s) => s,
            None => {
                println!("  ⚠️ Position review raw (no JSON found): {}", &response.text[..response.text.len().min(200)]);
                // Try to infer action from text
                let text_lower = response.text.to_lowercase();
                let action = if text_lower.contains("sell") && !text_lower.contains("hold") {
                    "SELL"
                } else {
                    "HOLD"
                };
                let reasoning: String = response.text.chars().take(120).collect();
                return Ok(PositionDecision {
                    action: action.to_string(),
                    reasoning,
                    cost_estimate: response.cost,
                });
            }
        };

        let parsed: serde_json::Value = match serde_json::from_str(&json_str) {
            Ok(v) => v,
            Err(e) => {
                println!("  ⚠️ Position review JSON error: {} | raw: {}", e, &json_str[..json_str.len().min(200)]);
                return Ok(PositionDecision {
                    action: "HOLD".to_string(),
                    reasoning: format!("JSON parse error: {}", e),
                    cost_estimate: response.cost,
                });
            }
        };

        let action = parsed["action"].as_str().unwrap_or("HOLD").to_uppercase();
        let reasoning = parsed["reasoning"]
            .as_str()
            .unwrap_or("No reasoning")
            .to_string();

        Ok(PositionDecision {
            action,
            reasoning,
            cost_estimate: response.cost,
        })
    }

    /// Multi-expert team analysis: 3 Haiku experts (parallel) + 1 Sonnet leader
    pub async fn expert_team_analysis(
        &self,
        question: &str,
        description: Option<&str>,
        market_price: f64,
        market_state: &str,
        balance: f64,
        api_remaining: f64,
        learning_context: &str,
    ) -> Result<(AnalysisResponse, Vec<ExpertOpinion>)> {
        if self.api_key == "mock" {
            return Ok((
                AnalysisResponse {
                    action: "SKIP".to_string(),
                    probability: 0.5,
                    bet_fraction: 0.0,
                    reasoning: "Mock analysis".to_string(),
                    cost_estimate: 0.0,
                },
                vec![],
            ));
        }

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let desc_str = description.unwrap_or("N/A");
        let desc_short: String = desc_str.chars().take(220).collect();
        let state_short: String = market_state.chars().take(220).collect();

        let user_msg = format!(
            "Q:{}\nDesc:{}\nState:{}\nPriceA:{:.4}\nJSON only.",
            question, desc_short, state_short, market_price
        );

        // Expert prompts - survival-aware, JSON-strict
        let survival_note = "You are an AI whose survival depends on profitable trades. Your API costs are funded by trading profits. Bad analysis = no profit = you die.";

        let fundamentals_system = format!(
            "{} You are a fundamentals analyst. Today: {}. Estimate the TRUE probability of Outcome A using news, domain knowledge, historical base rates. Output ONLY valid JSON, nothing else.\nExample: {{\"probability\":0.65,\"confidence\":0.80,\"reasoning\":\"historical base rate suggests higher\"}}",
            survival_note, today
        );

        let contrarian_system = format!(
            "{} You are a contrarian analyst. Today: {}. Market price={:.4}. Your job: find why the market is WRONG. Look for overreaction, neglected risks, or mispricing. Output ONLY valid JSON, nothing else.\nExample: {{\"probability\":0.40,\"confidence\":0.60,\"reasoning\":\"market overreacts to recent news\"}}",
            survival_note, today, market_price
        );

        let quant_system = format!(
            "{} You are a quant analyst. Today: {}. Analyze volume, liquidity, price momentum, time to expiry. Output ONLY valid JSON, nothing else.\nExample: {{\"probability\":0.55,\"confidence\":0.70,\"reasoning\":\"low volume suggests uncertainty\"}}",
            survival_note, today
        );

        // Run 3 experts in parallel (Opus for accuracy)
        let (fund_res, contra_res, quant_res) = tokio::join!(
            self.call_api_json(&self.expert_model, &fundamentals_system, &user_msg, 120),
            self.call_api_json(&self.expert_model, &contrarian_system, &user_msg, 120),
            self.call_api_json(&self.expert_model, &quant_system, &user_msg, 120),
        );

        let mut total_cost = 0.0;
        let mut experts: Vec<ExpertOpinion> = Vec::new();

        for (name, result) in [
            ("Fundamentals", fund_res),
            ("Contrarian", contra_res),
            ("Quant", quant_res),
        ] {
            match result {
                Ok(resp) => {
                    total_cost += resp.cost;
                    let opinion = Self::parse_expert_response(name, &resp.text);
                    println!(
                        "  [{}] prob={:.2} conf={:.2} | {}",
                        opinion.expert_type, opinion.probability, opinion.confidence, opinion.reasoning
                    );
                    experts.push(opinion);
                }
                Err(e) => {
                    println!("  [{}] FAILED: {}", name, e);
                    // Use neutral opinion on failure
                    experts.push(ExpertOpinion {
                        expert_type: name.to_string(),
                        probability: 0.5,
                        confidence: 0.0,
                        reasoning: format!("API error: {}", &e.to_string()[..e.to_string().len().min(40)]),
                    });
                }
            }
        }

        // Leader synthesis (Sonnet)
        let learning_short: String = learning_context.chars().take(200).collect();
        let expert_summary: String = experts
            .iter()
            .map(|e| {
                format!(
                    "{}:prob={:.3},conf={:.2},reason={}",
                    e.expert_type, e.probability, e.confidence, e.reasoning
                )
            })
            .collect::<Vec<_>>()
            .join("|");

        let leader_system = format!(
            r#"You are the lead trader of an AI that DIES if it runs out of money. Your trading profits fund your own API costs. No profit = no API = death.

Date: {today}. Balance=${balance:.2}, API credit=${api_remaining:.2}.
Past trades: {learning_short}

3 experts analyzed this market:
{expert_summary}

Decision rules:
1) q = confidence-weighted average of expert probabilities
2) edge = |q - price|. If edge < 0.08, action must be SKIP
3) q > price => BUY. q < price => SELL
4) bet_fraction: consensus(spread<0.08) = 0.04-0.10, mixed = 0.02-0.05, divergent(>0.15) = SKIP
5) All experts confidence < 0.4 => SKIP

Output ONLY valid JSON, nothing else:
{{"action":"BUY","probability":0.65,"bet_fraction":0.05,"reasoning":"experts agree on mispricing"}}"#
        );

        let leader_user = format!(
            "Q:{}\nPriceA:{:.4}\nExperts:[{}]\nJSON:",
            question, market_price, expert_summary
        );

        let leader_response = self
            .call_api_json(&self.model, &leader_system, &leader_user, 150)
            .await?;
        total_cost += leader_response.cost;

        // Parse leader response, fall back to expert consensus if leader fails
        let analysis = match extract_json(&leader_response.text)
            .and_then(|json_str| serde_json::from_str::<serde_json::Value>(&json_str).ok())
        {
            Some(parsed) => {
                let action = parsed["action"].as_str().unwrap_or("SKIP").to_uppercase();
                let probability = parsed["probability"].as_f64().unwrap_or(0.5);
                let bet_fraction = parsed["bet_fraction"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .clamp(0.0, 0.10);
                let reasoning = parsed["reasoning"]
                    .as_str()
                    .unwrap_or("No reasoning")
                    .to_string();

                AnalysisResponse {
                    action,
                    probability,
                    bet_fraction,
                    reasoning,
                    cost_estimate: total_cost,
                }
            }
            None => {
                // Fallback: use confidence-weighted average of experts
                println!(
                    "  ⚠️ Leader parse failed, using expert consensus. Raw: {}",
                    &leader_response.text[..leader_response.text.len().min(120)]
                );
                let valid_experts: Vec<&ExpertOpinion> =
                    experts.iter().filter(|e| e.confidence > 0.0).collect();
                if valid_experts.is_empty() {
                    AnalysisResponse {
                        action: "SKIP".to_string(),
                        probability: 0.5,
                        bet_fraction: 0.0,
                        reasoning: "All experts failed".to_string(),
                        cost_estimate: total_cost,
                    }
                } else {
                    let total_conf: f64 = valid_experts.iter().map(|e| e.confidence).sum();
                    let weighted_prob: f64 = valid_experts
                        .iter()
                        .map(|e| e.probability * e.confidence)
                        .sum::<f64>()
                        / total_conf;
                    let edge = (weighted_prob - market_price).abs();
                    let action = if edge < 0.08 {
                        "SKIP"
                    } else if weighted_prob > market_price {
                        "BUY"
                    } else {
                        "SELL"
                    };
                    let bet_frac = if edge >= 0.08 { 0.03 } else { 0.0 };

                    AnalysisResponse {
                        action: action.to_string(),
                        probability: weighted_prob,
                        bet_fraction: bet_frac,
                        reasoning: format!("Expert consensus (leader failed) edge={:.2}", edge),
                        cost_estimate: total_cost,
                    }
                }
            }
        };

        Ok((analysis, experts))
    }

    fn parse_expert_response(expert_type: &str, text: &str) -> ExpertOpinion {
        let default = ExpertOpinion {
            expert_type: expert_type.to_string(),
            probability: 0.5,
            confidence: 0.3,
            reasoning: "Parse failed".to_string(),
        };

        let json_str = match extract_json(text) {
            Some(s) => s,
            None => return default,
        };

        match serde_json::from_str::<serde_json::Value>(&json_str) {
            Ok(parsed) => ExpertOpinion {
                expert_type: expert_type.to_string(),
                probability: parsed["probability"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0),
                confidence: parsed["confidence"].as_f64().unwrap_or(0.3).clamp(0.0, 1.0),
                reasoning: parsed["reasoning"]
                    .as_str()
                    .unwrap_or("No reasoning")
                    .chars()
                    .take(60)
                    .collect(),
            },
            Err(_) => default,
        }
    }

    /// call_api with assistant prefill `{` to force JSON output
    async fn call_api_json(
        &self,
        model: &str,
        system: &str,
        user_msg: &str,
        max_tokens: i32,
    ) -> Result<ApiResponse> {
        let result = self.call_api_inner(model, system, user_msg, max_tokens, Some("{")).await?;
        // Prepend the `{` that was used as prefill
        Ok(ApiResponse {
            text: format!("{{{}", result.text),
            cost: result.cost,
        })
    }

    async fn call_api(
        &self,
        model: &str,
        system: &str,
        user_msg: &str,
        max_tokens: i32,
    ) -> Result<ApiResponse> {
        self.call_api_inner(model, system, user_msg, max_tokens, None).await
    }

    async fn call_api_inner(
        &self,
        model: &str,
        system: &str,
        user_msg: &str,
        max_tokens: i32,
        prefill: Option<&str>,
    ) -> Result<ApiResponse> {
        let max_retries = 3u32;
        let mut last_err = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let backoff_ms = 500 * 2u64.pow(attempt - 1); // 500ms, 1s, 2s
                println!(
                    "  ⏳ API retry {}/{} after {}ms...",
                    attempt, max_retries, backoff_ms
                );
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            }

            let mut messages = vec![Message {
                role: "user".to_string(),
                content: user_msg.to_string(),
            }];
            if let Some(pf) = prefill {
                messages.push(Message {
                    role: "assistant".to_string(),
                    content: pf.to_string(),
                });
            }

            let response = match self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&AnthropicRequest {
                    model: model.to_string(),
                    max_tokens,
                    system: system.to_string(),
                    messages,
                })
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(anyhow::anyhow!("Request failed: {}", e));
                    continue;
                }
            };

            let status = response.status();
            let text = response.text().await.unwrap_or_default();

            // Retry on transient HTTP errors (429 rate limit, 5xx server errors)
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                println!("  ⚠️ API transient error ({}), will retry", status);
                last_err = Some(anyhow::anyhow!(
                    "API Error ({}): {}",
                    status,
                    &text[..text.len().min(200)]
                ));
                continue;
            }

            if text.is_empty() {
                last_err = Some(anyhow::anyhow!(
                    "Empty response from API (status {})",
                    status
                ));
                continue;
            }

            if !status.is_success() {
                // Detect credit exhaustion (402, or 400 with "credit" in message)
                let is_credit_error = status.as_u16() == 402
                    || text.to_lowercase().contains("credit")
                    || text.to_lowercase().contains("billing")
                    || text.to_lowercase().contains("insufficient");
                if is_credit_error {
                    return Err(anyhow::anyhow!("CREDIT_EXHAUSTED: {}", &text[..text.len().min(200)]));
                }
                return Err(anyhow::anyhow!(
                    "API Error ({}): {}",
                    status,
                    &text[..text.len().min(300)]
                ));
            }

            let anthropic_res: AnthropicResponse =
                serde_json::from_str(&text).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to parse API response: {} | raw: {}",
                        e,
                        &text[..text.len().min(300)]
                    )
                })?;

            // Check for API-level errors
            if let Some(err) = anthropic_res.error {
                let err_type = err.error_type.unwrap_or_default();
                let err_msg = err.message.unwrap_or_default();
                // Retry on overloaded errors
                if err_type == "overloaded_error" {
                    last_err = Some(anyhow::anyhow!("API overloaded: {}", err_msg));
                    continue;
                }
                return Err(anyhow::anyhow!("API error: {} ({})", err_msg, err_type));
            }

            let content = anthropic_res
                .content
                .ok_or_else(|| anyhow::anyhow!("No content in API response"))?;

            if content.is_empty() {
                return Err(anyhow::anyhow!("Empty content array in API response"));
            }

            let text_content = content[0].text.clone();
            let usage = anthropic_res.usage.unwrap_or(Usage {
                input_tokens: 0,
                output_tokens: 0,
            });

            let (input_rate, output_rate) = if model.contains("opus") {
                (15.0, 75.0)
            } else if model.contains("sonnet") {
                (3.0, 15.0)
            } else {
                (0.25, 1.25)
            };

            let cost = (usage.input_tokens as f64 * input_rate / 1_000_000.0)
                + (usage.output_tokens as f64 * output_rate / 1_000_000.0);

            return Ok(ApiResponse {
                text: text_content,
                cost,
            });
        }

        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("API call failed after {} retries", max_retries)
        }))
    }
}

struct ApiResponse {
    text: String,
    cost: f64,
}
