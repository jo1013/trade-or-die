use anyhow::Result;
use reqwest::Client;
use std::env;

pub struct Notifier {
    client: Client,
    token: String,
    chat_id: String,
}

impl Notifier {
    pub fn new() -> Option<Self> {
        let token = env::var("TELE_BOT").ok()?;
        let chat_id = env::var("TELE_CHAT_ID").ok()?;

        Some(Self {
            client: Client::new(),
            token,
            chat_id,
        })
    }

    pub async fn send_message(&self, text: &str) -> Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let _ = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": text,
                "parse_mode": "Markdown"
            }))
            .send()
            .await?;
        Ok(())
    }
}
