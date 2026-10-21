use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct Telegram {
    client: Client,
    endpoint: String,
}

/// Read-only requests can be retried when the server outcome is uncertain.
/// Mutating Telegram calls are NOT safe to replay after a transport failure:
/// the first request may already have changed a user's permissions or sent a message.
fn safe_to_retry(method: &str) -> bool {
    matches!(
        method,
        "getMe" | "getUpdates" | "getChat" | "getChatMember" | "getWebhookInfo"
    )
}

impl Telegram {
    pub fn new(token: &str) -> Result<Self> {
        let client = Client::builder().timeout(Duration::from_secs(40)).build()?;
        Ok(Self {
            client,
            endpoint: format!("https://api.telegram.org/bot{token}/"),
        })
    }

    #[cfg(test)]
    pub fn mock(endpoint: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(4))
                .build()
                .expect("test client"),
            endpoint,
        }
    }

    pub async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let max_attempts = if safe_to_retry(method) { 4 } else { 1 };
        for attempt in 0..max_attempts {
            let outcome = self
                .client
                .post(format!("{}{}", self.endpoint, method))
                .json(&body)
                .send()
                .await;
            match outcome {
                Ok(response) => {
                    // Never log request URLs: they contain the BotFather token.
                    let data: Value = response
                        .json()
                        .await
                        .map_err(|_| anyhow::anyhow!("Telegram {method}: invalid JSON response"))?;
                    if data["ok"] == true {
                        return Ok(data["result"].clone());
                    }
                    let status = data["error_code"].as_i64().unwrap_or(500);
                    let description = data["description"].as_str().unwrap_or("unknown API error");
                    let permanent = (400..500).contains(&status) && status != 429;
                    if permanent || attempt + 1 == max_attempts {
                        anyhow::bail!(
                            "Telegram {method} returned {status}: {description};                             outcome of mutating requests must be verified"
                        );
                    }
                    let wait = data["parameters"]["retry_after"]
                        .as_u64()
                        .unwrap_or(1_u64 << attempt)
                        .min(30);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                Err(error) => {
                    let kind = if error.is_timeout() {
                        "timeout"
                    } else if error.is_connect() {
                        "connection error"
                    } else {
                        "transport error"
                    };
                    if attempt + 1 == max_attempts {
                        anyhow::bail!(
                            "Telegram {method}: {kind}; outcome of mutating requests is unknown"
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(1_u64 << attempt)).await;
                }
            }
        }
        unreachable!("bounded retry loop always returns")
    }

    pub async fn send(&self, chat: i64, text: &str) -> Result<i64> {
        let response = self
            .call("sendMessage", json!({"chat_id": chat, "text": text}))
            .await?;
        response["message_id"]
            .as_i64()
            .context("sendMessage result missing message_id")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    #[test]
    fn retry_classification_is_explicit() {
        for name in [
            "getUpdates",
            "getChat",
            "getChatMember",
            "getMe",
            "getWebhookInfo",
        ] {
            assert!(safe_to_retry(name), "{name}");
        }
        for name in [
            "sendMessage",
            "restrictChatMember",
            "banChatMember",
            "approveChatJoinRequest",
            "declineChatJoinRequest",
            "deleteMessage",
            "answerCallbackQuery",
            "deleteWebhook",
        ] {
            assert!(!safe_to_retry(name), "{name}");
        }
    }

    #[tokio::test]
    async fn mutating_call_is_not_retried_after_server_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botSECRET/banChatMember"))
            .respond_with(ResponseTemplate::new(502).set_body_json(json!({
                "ok": false, "error_code": 502, "description": "bad gateway"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = Telegram::mock(format!("{}/botSECRET/", server.uri()));
        let error = client
            .call("banChatMember", json!({"chat_id": -1, "user_id": 7}))
            .await
            .expect_err("must report ambiguous action instead of retrying");
        assert!(error.to_string().contains("banChatMember"));
        assert!(!error.to_string().contains("SECRET"));
        server.verify().await;
    }

    #[tokio::test]
    async fn read_only_request_may_retry_a_temporary_error() -> Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let server = MockServer::start().await;
        let attempts = std::sync::Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(move |_: &wiremock::Request| {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(503).set_body_json(json!({
                        "ok": false, "error_code": 503, "description": "unavailable"
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "ok": true, "result": {"status": "member"}
                    }))
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let client = Telegram::mock(format!("{}/botTEST/", server.uri()));
        let response = client
            .call("getChatMember", json!({"chat_id": -1, "user_id": 7}))
            .await?;
        assert_eq!(response["status"], "member");
        server.verify().await;
        Ok(())
    }
}
