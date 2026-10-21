# Rust Bot Rebuild — first implementation

Rust implementation of the admission/anti-bot core. Original Python Shield24 v0.1.0
remains archived in Notion as a functional reference.

## Scope of this PR

- Long polling via Telegram Bot API. Keep only one running bot instance.
- New-member restriction before CAPTCHA delivery.
- Join-request CAPTCHA in the temporary private chat, if Telegram supplies `user_chat_id`.
- Persisted SQLite settings, challenge nonce and deadline, audit, offset and poison-update records.
- CAPTCHA callback checked against the correct Telegram user and the active challenge nonce.
- Deadline expiration and explicit manual state for uncertain API outcomes.
- Admin commands: `/help`, `/status`, `/guard`, `/links`, `/lockdown`, `/timeout`, `/attempts`.
- URL deletion if the administrator enables it; off by default.
- Rust CI, Docker and Compose.

## Run locally

Use Rust stable (`cargo`) and SQLite-compatible local storage.

```bash
export BOT_TOKEN='your-token-from-BotFather'
export DATABASE_PATH='./data/rebuild.sqlite3'
cargo run --release
```

Do **not** commit real tokens, database files or personally identifying dumps.

## Validation status

- Compilation and GitHub CI: must pass before merging.
- Integration with a real Telegram test group: **not done**.
- Full Python Shield24 25-test behavioral parity: **not yet achieved**.
- This is a new codebase, not a verified production deployment.

## Deferred / incomplete

- Crash recovery for uncertain Telegram actions requires a human review;
  do not retry destructive actions blindly.
- Per-user pre-existing permission restoration is not implemented.
- `/report`, review queue, flood rules, join-request Guardian mode,
  fine-grained policy, book search and quizzes are later iterations.
- The first PR does not reproduce the entire 72-function backlog.

## Permissions

For member admission: Telegram bot requires administrator permission to restrict
members and delete CAPTCHA messages. For join requests: approve/decline rights.
For the link filter: delete messages. Privacy-mode settings must allow receiving
the update kinds configured for the chat.

Do not deploy this build as protection for an active community until a separate
test supergroup exercises success, denial, timeout, restart and API failures.
