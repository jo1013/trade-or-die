use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct AnalysisResponse {
    pub action: String,    // "BUY", "SELL", "SKIP"
    pub probability: f64,  // AI estimated true probability
    pub bet_fraction: f64, // balance fraction (0.0~0.06)
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

        Self {
            client: Client::new(),
            api_key,
            model: main_model,
            screen_model: screen,
        }
    }

    pub fn new_mock() -> Self {
        Self {
            client: Client::new(),
            api_key: "mock".to_string(),
            model: "mock".to_string(),
            screen_model: "mock".to_string(),
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
4) bet_fraction range [0,0.06]:
   0.08-0.15 -> 0.01-0.02
   0.15-0.25 -> 0.02-0.04
   >0.25 -> 0.04-0.06
5) If uncertainty is high or thesis is weak, return SKIP.

Return JSON only:
{{
  "action": "BUY" | "SELL" | "SKIP",
  "probability": 0.0-1.0,
  "bet_fraction": 0.0-0.06,
  "reasoning": "max 12 words"
}}"#
        );

        let desc_str = description.unwrap_or("No additional context");
        let desc_short: String = desc_str.chars().take(220).collect();
        let user_message = format!(
            "Q:{}\nDesc:{}\nState:{}\nPriceA:{:.4}\nJSON only.",
            question, desc_short, state_short, market_price
        );

        let response = self
            .call_api(&self.model, &system_prompt, &user_message, 110)
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
            .clamp(0.0, 0.06);
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

    async fn call_api(
        &self,
        model: &str,
        system: &str,
        user_msg: &str,
        max_tokens: i32,
    ) -> Result<ApiResponse> {
        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&AnthropicRequest {
                model: model.to_string(),
                max_tokens,
                system: system.to_string(),
                messages: vec![Message {
                    role: "user".to_string(),
                    content: user_msg.to_string(),
                }],
            })
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if text.is_empty() {
            return Err(anyhow::anyhow!(
                "Empty response from API (status {})",
                status
            ));
        }

        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "API Error ({}): {}",
                status,
                &text[..text.len().min(300)]
            ));
        }

        let anthropic_res: AnthropicResponse = serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse API response: {} | raw: {}",
                e,
                &text[..text.len().min(300)]
            )
        })?;

        // Check for API-level errors
        if let Some(err) = anthropic_res.error {
            return Err(anyhow::anyhow!(
                "API error: {} ({})",
                err.message.unwrap_or_default(),
                err.error_type.unwrap_or_default()
            ));
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

        Ok(ApiResponse {
            text: text_content,
            cost,
        })
    }
}

struct ApiResponse {
    text: String,
    cost: f64,
}
