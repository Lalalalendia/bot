use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct Telegram {
    client: Client,
    endpoint: String,
}

impl Telegram {
    pub fn new(token: &str) -> Result<Self> {
        let client = Client::builder().timeout(Duration::from_secs(40)).build()?;
        Ok(Self {client,endpoint: format!("https://api.telegram.org/bot{token}/")})
    }

    pub async fn call(&self, method: &str, body: Value) -> Result<Value> {
        for attempt in 0..4 {
            let outcome = self.client.post(format!("{}{}",self.endpoint,method))
                .json(&body).send().await;
            match outcome {
                Ok(response) => {
                    let response: Value = response.json().await.context("invalid Telegram JSON")?;
                    if response["ok"] == true { return Ok(response["result"].clone()); }
                    let status=response["error_code"].as_i64().unwrap_or(500);
                    let msg=response["description"].as_str().unwrap_or("unknown Telegram error");
                    if (400..500).contains(&status) && status != 429 {
                        anyhow::bail!("Telegram permanent {status}: {msg}");
                    }
                    if attempt==3 {anyhow::bail!("Telegram error {status}: {msg}");}
                    let wait=response["parameters"]["retry_after"].as_u64()
                        .unwrap_or(1u64 << attempt).min(30);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                Err(e) => {
                    if attempt==3 {anyhow::bail!("Telegram transport failure: {}", e.status().map(|s|s.to_string()).unwrap_or_else(|| "network error".into()));}
                    tokio::time::sleep(Duration::from_secs(1u64 << attempt)).await;
                }
            }
        }
        unreachable!()
    }

    pub async fn send(&self, chat: i64, text: &str) -> Result<i64> {
        let v=self.call("sendMessage",json!({"chat_id":chat,"text":text})).await?;
        v["message_id"].as_i64().context("sendMessage missing message_id")
    }
}
