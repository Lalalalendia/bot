mod bot;
mod model;
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
    let database =
        std::env::var("DATABASE_PATH").unwrap_or_else(|_| "./data/rebuild.sqlite3".into());
    let bot = Bot::new(Store::open(&database)?, Telegram::new(&token)?);
    bot.run().await
}
