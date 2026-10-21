mod bot;
mod model;
mod preflight;
mod store;
mod telegram;

use anyhow::{Context, Result};
use bot::Bot;
use store::Store;
use telegram::Telegram;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rebuild_bot=info,warn".into()),
        )
        .init();
    let token = std::env::var("BOT_TOKEN").context("BOT_TOKEN is required")?;
    if token.trim().is_empty() {
        anyhow::bail!("BOT_TOKEN must not be empty");
    }
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
