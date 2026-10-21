//! Read-only Telegram deployment checks. No polling, moderation or database writes.
use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::telegram::Telegram;

#[derive(Debug)]
pub struct Report {
    pub bot_id: i64,
    pub username: String,
    pub chat_id: i64,
    pub chat_title: String,
}

fn validate_group(chat: &Value) -> Result<()> {
    anyhow::ensure!(
        chat["type"] == "supergroup",
        "TEST_CHAT_ID must identify a Telegram supergroup"
    );
    let permissions = chat["permissions"]
        .as_object()
        .context("getChat did not expose the group's default permissions")?;
    anyhow::ensure!(
        permissions.get("can_send_messages") == Some(&Value::Bool(true)),
        "the test supergroup must allow members to send messages by default"
    );
    // The bot restores group defaults after CAPTCHA, so absent or malformed
    // permission data cannot be treated as permissive.
    anyhow::ensure!(
        permissions.values().all(Value::is_boolean),
        "Telegram returned invalid group permission values"
    );
    Ok(())
}

fn validate_administrator(member: &Value) -> Result<()> {
    anyhow::ensure!(
        member["status"] == "administrator",
        "bot must be a group administrator (getChatMember.status)"
    );
    for (key, purpose) in [
        (
            "can_restrict_members",
            "restricting and releasing new members",
        ),
        (
            "can_delete_messages",
            "cleaning CAPTCHA and moderated messages",
        ),
        ("can_invite_users", "approving and declining join requests"),
    ] {
        anyhow::ensure!(
            member[key] == true,
            "bot is missing {key}, required for {purpose}"
        );
    }
    Ok(())
}

/// Verify a live test-supergroup using only read-only Telegram methods.
/// A successful result is *not* evidence that a new user completed CAPTCHA.
pub async fn check(tg: &Telegram, chat_id: i64) -> Result<Report> {
    let me = tg.call("getMe", json!({})).await.context("getMe")?;
    let bot_id = me["id"].as_i64().context("getMe did not return bot ID")?;
    let username = me["username"]
        .as_str()
        .context("getMe did not return a bot username")?
        .to_owned();

    let webhook = tg
        .call("getWebhookInfo", json!({}))
        .await
        .context("getWebhookInfo")?;
    anyhow::ensure!(
        webhook["url"].as_str().is_none_or(str::is_empty),
        "webhook is configured; this long-polling bot will not start until the deployment mode is changed explicitly"
    );

    let chat = tg
        .call("getChat", json!({"chat_id":chat_id}))
        .await
        .context("getChat")?;
    validate_group(&chat)?;

    let member = tg
        .call("getChatMember", json!({"chat_id":chat_id,"user_id":bot_id}))
        .await
        .context("getChatMember(bot)")?;
    validate_administrator(&member)?;

    Ok(Report {
        bot_id,
        username,
        chat_id,
        chat_title: chat["title"].as_str().unwrap_or("(untitled)").to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    async fn wire(server: &MockServer, name: &str, response: Value) {
        Mock::given(method("POST"))
            .and(path(format!("/botTEST/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": response
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn live_api_preflight_succeeds_for_ready_test_group() -> Result<()> {
        let server = MockServer::start().await;
        wire(
            &server,
            "getMe",
            json!({"id":111,"username":"RebuildTestBot"}),
        )
        .await;
        wire(&server, "getWebhookInfo", json!({"url":""})).await;
        wire(
            &server,
            "getChat",
            json!({
                "id":-100200,"title":"Test group","type":"supergroup",
                "permissions":{"can_send_messages":true,"can_send_photos":false}
            }),
        )
        .await;
        wire(
            &server,
            "getChatMember",
            json!({
                "status":"administrator",
                "can_restrict_members":true,
                "can_delete_messages":true,
                "can_invite_users":true
            }),
        )
        .await;
        let tg = Telegram::mock(format!("{}/botTEST/", server.uri()));
        let report = check(&tg, -100200).await?;
        assert_eq!(report.bot_id, 111);
        assert_eq!(report.username, "RebuildTestBot");
        assert_eq!(report.chat_id, -100200);
        assert_eq!(report.chat_title, "Test group");
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn preflight_refuses_existing_webhook_without_mutating_telegram() -> Result<()> {
        let server = MockServer::start().await;
        wire(
            &server,
            "getMe",
            json!({"id":111,"username":"RebuildTestBot"}),
        )
        .await;
        wire(
            &server,
            "getWebhookInfo",
            json!({"url":"https://example.org/hook"}),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/deleteWebhook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let tg = Telegram::mock(format!("{}/botTEST/", server.uri()));
        assert!(check(&tg, -100200)
            .await
            .unwrap_err()
            .to_string()
            .contains("webhook"));
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn missing_join_permission_fails_closed() -> Result<()> {
        let server = MockServer::start().await;
        wire(
            &server,
            "getMe",
            json!({"id":111,"username":"RebuildTestBot"}),
        )
        .await;
        wire(&server, "getWebhookInfo", json!({"url":""})).await;
        wire(
            &server,
            "getChat",
            json!({
                "id":-100200,"type":"supergroup",
                "permissions":{"can_send_messages":true}
            }),
        )
        .await;
        wire(
            &server,
            "getChatMember",
            json!({
                "status":"administrator",
                "can_restrict_members":true,
                "can_delete_messages":true,
                "can_invite_users":false
            }),
        )
        .await;
        let tg = Telegram::mock(format!("{}/botTEST/", server.uri()));
        let err = check(&tg, -100200).await.unwrap_err();
        assert!(err.to_string().contains("can_invite_users"));
        server.verify().await;
        Ok(())
    }

    #[test]
    fn private_chat_or_non_writable_group_is_rejected() {
        assert!(validate_group(&json!({
            "type":"private","permissions":{"can_send_messages":true}
        }))
        .is_err());
        assert!(validate_group(&json!({
            "type":"supergroup","permissions":{"can_send_messages":false}
        }))
        .is_err());
        assert!(validate_group(&json!({
            "type":"supergroup","permissions":{"can_send_messages":"true"}
        }))
        .is_err());
    }
}
