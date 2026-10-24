mod bot;
mod model;
mod preflight;
mod store;
mod telegram;

use anyhow::{Context, Result};
use bot::Bot;
use std::path::PathBuf;
use store::Store;
use telegram::Telegram;

/// A systemd LoadCredential is authoritative when CREDENTIALS_DIRECTORY is set.
/// Do not fall back to an environment token if the configured credential is missing.
fn load_bot_token(env_token: Option<String>, credentials_dir: Option<PathBuf>) -> Result<String> {
    let raw = match credentials_dir {
        Some(directory) => std::fs::read_to_string(directory.join("bot_token"))
            .context("read systemd bot_token credential")?,
        None => env_token.context("BOT_TOKEN or systemd bot_token credential is required")?,
    };
    let token = raw.trim();
    anyhow::ensure!(!token.is_empty(), "bot token must not be empty");
    Ok(token.to_owned())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rebuild_bot=info,warn".into()),
        )
        .init();
    let token = load_bot_token(
        std::env::var("BOT_TOKEN").ok(),
        std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from),
    )?;
    // One-shot read-only preflight: neither starts polling nor opens SQLite.
    // Use a separate test supergroup; never paste the token into chat or Git.
    let mut args = std::env::args().skip(1);
    let command = args.next();
    anyhow::ensure!(
        args.next().is_none(),
        "only one command argument is allowed"
    );
    let tg = Telegram::new(&token)?;
    match command.as_deref() {
        Some("--preflight") => {
            let chat_id = std::env::var("TEST_CHAT_ID")
                .context("TEST_CHAT_ID is required for --preflight")?
                .parse::<i64>()
                .context("TEST_CHAT_ID must be a numeric Telegram chat ID")?;
            let report = preflight::check(&tg, chat_id).await?;
            println!("Telegram preflight PASS");
            println!("Bot: @{} (ID {})", report.username, report.bot_id);
            println!(
                "Test supergroup: {} (ID {})",
                report.chat_title, report.chat_id
            );
            println!("Webhook: absent; moderation rights: present; group defaults: checked");
            return Ok(());
        }
        None => {}
        Some(_) => anyhow::bail!("unknown argument; use --preflight or no arguments"),
    }
    let database =
        std::env::var("DATABASE_PATH").unwrap_or_else(|_| "./data/rebuild.sqlite3".into());
    let bot = Bot::new(Store::open(&database)?, tg);
    bot.run().await
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn local_env_token_remains_supported() -> Result<()> {
        assert_eq!(
            load_bot_token(Some("local:test".into()), None)?,
            "local:test"
        );
        assert!(load_bot_token(None, None).is_err());
        Ok(())
    }

    #[test]
    fn missing_systemd_credential_never_falls_back_to_env() {
        let missing =
            std::env::temp_dir().join(format!("bor-no-credential-{}", rand::random::<u64>()));
        let err = load_bot_token(Some("local:test".into()), Some(missing)).unwrap_err();
        assert!(err
            .to_string()
            .contains("read systemd bot_token credential"));
    }

    #[test]
    fn systemd_credential_takes_priority_and_trims_newline() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("bor-test-credential-{}", rand::random::<u64>()));
        std::fs::create_dir(&directory)?;
        std::fs::write(directory.join("bot_token"), b"12345:EXAMPLE\n")?;
        let actual = load_bot_token(Some("must-not-be-used".into()), Some(directory.clone()))?;
        std::fs::remove_dir_all(directory)?;
        assert_eq!(actual, "12345:EXAMPLE");
        Ok(())
    }
}
