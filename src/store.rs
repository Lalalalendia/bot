use anyhow::{Context, Result};
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use std::fs::{File, OpenOptions};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Clone, Debug)]
pub struct Challenge {
    pub chat_id: i64,
    pub user_id: i64,
    pub nonce: String,
    pub flow: String,
    pub status: String,
    pub delivery_chat_id: i64,
    pub answer: i64,
    pub options: Vec<i64>,
    pub attempts: i64,
    pub expires_at: i64,
    pub message_id: Option<i64>,
    pub action: Option<String>,
    pub updated_at: i64,
}

pub struct Store {
    conn: Connection,
    // The operating-system lock stays alive for as long as the bot runs.
    _instance_lock: Option<File>,
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        if path != ":memory:" {
            if let Some(parent) = Path::new(path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent).context("create DB directory")?;
            }
        }
        let instance_lock = if path == ":memory:" {
            None
        } else {
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(format!("{path}.instance.lock"))
                .context("open bot instance lock")?;
            file.try_lock_exclusive()
                .context("another bot instance already owns this SQLite database")?;
            Some(file)
        };
        let conn = Connection::open(path).context("open SQLite")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS state(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS settings(
               chat_id INTEGER PRIMARY KEY, enabled INTEGER NOT NULL DEFAULT 1,
               timeout_s INTEGER NOT NULL DEFAULT 120, attempts INTEGER NOT NULL DEFAULT 1,
               links INTEGER NOT NULL DEFAULT 0, lockdown INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS challenges(
               chat_id INTEGER NOT NULL, user_id INTEGER NOT NULL, nonce TEXT NOT NULL,
               flow TEXT NOT NULL, status TEXT NOT NULL, delivery_chat_id INTEGER NOT NULL,
               answer INTEGER NOT NULL, options TEXT NOT NULL,
               attempts INTEGER NOT NULL DEFAULT 0, expires_at INTEGER NOT NULL,
               message_id INTEGER, action TEXT, updated_at INTEGER NOT NULL,
               PRIMARY KEY(chat_id, user_id)
             );
             CREATE TABLE IF NOT EXISTS audit(
               id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL,
               chat_id INTEGER NOT NULL, user_id INTEGER NOT NULL,
               event TEXT NOT NULL, detail TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS deadletters(
               update_id INTEGER PRIMARY KEY, at INTEGER NOT NULL, reason TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS processed_callbacks(
               callback_id TEXT PRIMARY KEY, at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_challenges_sweep_expiry
                 ON challenges(status, expires_at);
             CREATE INDEX IF NOT EXISTS idx_challenges_sweep_unsent
                 ON challenges(status, message_id);",
        )?;
        // Upgrade existing groups to the single-attempt policy.
        conn.execute("UPDATE settings SET attempts=1 WHERE attempts<>1", [])?;
        Ok(Self {
            conn,
            _instance_lock: instance_lock,
        })
    }

    pub fn offset(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT value FROM state WHERE key='offset'", [], |r| {
                r.get(0)
            })
            .optional()?
            .unwrap_or(0))
    }

    pub fn offset_save(&self, offset: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO state(key,value) VALUES('offset',?1)
            ON CONFLICT(key) DO UPDATE SET value=max(value,excluded.value)",
            [offset],
        )?;
        Ok(())
    }

    pub fn deadletter(&self, id: i64, reason: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO deadletters(update_id,at,reason) VALUES(?1,?2,?3)",
            params![id, now(), reason],
        )?;
        Ok(())
    }

    pub fn settings(&self, chat: i64) -> Result<Settings> {
        // Most calls read an existing group: avoid attempting an INSERT for
        // every regular message. Keep the original durable defaults and the
        // first-access INSERT for groups that do not exist yet.
        let read = || -> Result<Option<Settings>> {
            self.conn
                .query_row(
                    "SELECT enabled,timeout_s,attempts,links,lockdown FROM settings WHERE chat_id=?1",
                    [chat],
                    |r| {
                        Ok(Settings {
                            enabled: r.get::<_, i64>(0)? != 0,
                            timeout: r.get(1)?,
                            max_attempts: r.get(2)?,
                            links: r.get::<_, i64>(3)? != 0,
                            lockdown: r.get::<_, i64>(4)? != 0,
                        })
                    },
                )
                .optional()
                .map_err(Into::into)
        };
        if let Some(settings) = read()? {
            return Ok(settings);
        }
        self.conn
            .execute("INSERT OR IGNORE INTO settings(chat_id) VALUES(?1)", [chat])?;
        read()?.context("settings were not persisted after creating the group")
    }

    pub fn setting(&self, chat: i64, key: &str, value: i64) -> Result<()> {
        let column = match key {
            "enabled" => "enabled",
            "timeout_s" => "timeout_s",
            "links" => "links",
            "lockdown" => "lockdown",
            _ => anyhow::bail!("invalid settings field"),
        };
        self.settings(chat)?;
        self.conn.execute(
            &format!("UPDATE settings SET {column}=?1 WHERE chat_id=?2"),
            params![value, chat],
        )?;
        Ok(())
    }

    pub fn get(&self, chat: i64, user: i64) -> Result<Option<Challenge>> {
        let mut stmt = self.conn.prepare(
            "SELECT chat_id,user_id,nonce,flow,status,delivery_chat_id,answer,
             options,attempts,expires_at,message_id,action,updated_at
             FROM challenges WHERE chat_id=?1 AND user_id=?2",
        )?;
        let result = stmt
            .query_row(params![chat, user], |row| {
                let encoded: String = row.get(7)?;
                let options = serde_json::from_str(&encoded).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })?;
                Ok(Challenge {
                    chat_id: row.get(0)?,
                    user_id: row.get(1)?,
                    nonce: row.get(2)?,
                    flow: row.get(3)?,
                    status: row.get(4)?,
                    delivery_chat_id: row.get(5)?,
                    answer: row.get(6)?,
                    options,
                    attempts: row.get(8)?,
                    expires_at: row.get(9)?,
                    message_id: row.get(10)?,
                    action: row.get(11)?,
                    updated_at: row.get(12)?,
                })
            })
            .optional()?;
        Ok(result)
    }

    pub fn create(&self, c: &Challenge) -> Result<()> {
        self.conn.execute(
            "INSERT INTO challenges(chat_id,user_id,nonce,flow,status,delivery_chat_id,answer,options,attempts,expires_at,message_id,action,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(chat_id,user_id) DO UPDATE SET nonce=excluded.nonce,flow=excluded.flow,
               status=excluded.status,delivery_chat_id=excluded.delivery_chat_id,
               answer=excluded.answer,options=excluded.options,attempts=excluded.attempts,
               expires_at=excluded.expires_at,message_id=excluded.message_id,
               action=excluded.action,updated_at=excluded.updated_at",
            params![c.chat_id,c.user_id,c.nonce,c.flow,c.status,c.delivery_chat_id,c.answer,
                serde_json::to_string(&c.options)?,c.attempts,c.expires_at,c.message_id,c.action,now()]
        )?;
        self.audit(c.chat_id, c.user_id, "challenge", &c.flow)?;
        Ok(())
    }

    pub fn mark_sent(&self, c: &Challenge, message_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE challenges SET message_id=?1,updated_at=?2
            WHERE chat_id=?3 AND user_id=?4 AND nonce=?5 AND status='pending'",
            params![message_id, now(), c.chat_id, c.user_id, c.nonce],
        )?;
        Ok(())
    }

    /// One Telegram callback may be delivered repeatedly after a restart
    /// or timeout. Record its ID in the SAME transaction as the penalty.
    /// Returns (remaining, limit_reached, newly_processed).
    pub fn wrong_once(
        &self,
        c: &Challenge,
        max: i64,
        callback_id: &str,
    ) -> Result<(i64, bool, bool)> {
        if callback_id.is_empty() {
            anyhow::bail!("callback_id must not be empty");
        }
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<(i64, bool, bool)> {
            let inserted = self.conn.execute(
                "INSERT OR IGNORE INTO processed_callbacks(callback_id,at)
                 VALUES(?1,?2)",
                params![callback_id, now()],
            )?;
            if inserted == 0 {
                return Ok((0, false, false));
            }
            let updated = self.conn.execute(
                "UPDATE challenges SET attempts=attempts+1,updated_at=?1
                 WHERE chat_id=?2 AND user_id=?3 AND nonce=?4 AND status='pending'",
                params![now(), c.chat_id, c.user_id, c.nonce],
            )?;
            if updated == 0 {
                return Ok((0, false, false));
            }
            let fresh = self
                .get(c.chat_id, c.user_id)?
                .context("challenge vanished after penalty")?;
            let left = (max - fresh.attempts).max(0);
            if left == 0 {
                let action = if fresh.flow == "member" {
                    "kick"
                } else {
                    "reject"
                };
                // Reaching the attempt limit must be atomic with the penalty:
                // a crash here must not permit a later correct callback.
                let n = self.conn.execute(
                    "UPDATE challenges SET status='acting',action=?1,updated_at=?2
                     WHERE chat_id=?3 AND user_id=?4 AND nonce=?5 AND status='pending'",
                    params![action, now(), fresh.chat_id, fresh.user_id, fresh.nonce],
                )?;
                anyhow::ensure!(n == 1, "could not reserve rejection action");
            }
            Ok((left, left == 0, true))
        })();
        match result {
            Ok(value) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Persist the intended external action before making a Telegram API request.
    pub fn claim(&self, c: &Challenge, action: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE challenges SET status='acting',action=?1,updated_at=?2
            WHERE chat_id=?3 AND user_id=?4 AND nonce=?5 AND status='pending'",
            params![action, now(), c.chat_id, c.user_id, c.nonce],
        )?;
        Ok(n == 1)
    }

    pub fn finish(&self, c: &Challenge, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE challenges SET status=?1,action=NULL,updated_at=?2
            WHERE chat_id=?3 AND user_id=?4 AND nonce=?5 AND status='acting'",
            params![status, now(), c.chat_id, c.user_id, c.nonce],
        )?;
        self.audit(c.chat_id, c.user_id, status, "")?;
        Ok(())
    }

    /// After Telegram confirms admission, give this member ten minutes
    /// to write their first message. Reuse the existing SQLite challenge row.
    pub fn start_first_message_window(&self, c: &Challenge) -> Result<()> {
        let admitted_at = now();
        let updated = self.conn.execute(
            "UPDATE challenges SET status='awaiting_message',action=NULL,
             updated_at=?1,expires_at=?2 WHERE chat_id=?3 AND user_id=?4
             AND nonce=?5 AND status='acting'",
            params![
                admitted_at,
                admitted_at + 600,
                c.chat_id,
                c.user_id,
                c.nonce
            ],
        )?;
        anyhow::ensure!(
            updated == 1,
            "admission was not awaiting Telegram confirmation"
        );
        self.audit(
            c.chat_id,
            c.user_id,
            "awaiting_message",
            "first message within 600 seconds",
        )?;
        Ok(())
    }

    /// The Telegram message time must fall inside the admission window.
    /// Edits, commands, empty messages and messages by bots are filtered by the caller.
    pub fn record_first_message(
        &self,
        chat: i64,
        user: i64,
        message_id: i64,
        sent_at: i64,
    ) -> Result<bool> {
        if message_id <= 0 || sent_at <= 0 {
            return Ok(false);
        }
        let updated = self.conn.execute(
            "UPDATE challenges SET status='passed',updated_at=?1
             WHERE chat_id=?2 AND user_id=?3 AND status='awaiting_message'
               AND updated_at<=?4 AND expires_at>=?4",
            params![now(), chat, user, sent_at],
        )?;
        if updated == 1 {
            self.audit(
                chat,
                user,
                "first_message",
                &format!("message_id={message_id}"),
            )?;
        }
        Ok(updated == 1)
    }

    /// Atomically reserve the timeout action: a late message cannot undo it.
    pub fn claim_first_message_timeout(&self, c: &Challenge) -> Result<bool> {
        let updated = self.conn.execute(
            "UPDATE challenges SET status='acting',action='kick',updated_at=?1
             WHERE chat_id=?2 AND user_id=?3 AND nonce=?4
               AND status='awaiting_message' AND expires_at<=?1",
            params![now(), c.chat_id, c.user_id, c.nonce],
        )?;
        Ok(updated == 1)
    }

    /// Close the window without sanction if the member has left or become admin.
    pub fn close_first_message_window(
        &self,
        c: &Challenge,
        status: &str,
        reason: &str,
    ) -> Result<bool> {
        anyhow::ensure!(
            matches!(status, "passed" | "failed"),
            "invalid first-message result"
        );
        let updated = self.conn.execute(
            "UPDATE challenges SET status=?1,updated_at=?2
             WHERE chat_id=?3 AND user_id=?4 AND nonce=?5 AND status='awaiting_message'",
            params![status, now(), c.chat_id, c.user_id, c.nonce],
        )?;
        if updated == 1 {
            self.audit(c.chat_id, c.user_id, status, reason)?;
        }
        Ok(updated == 1)
    }

    /// Telegram confirmed membership outside our own join-request approval.
    /// This can happen when a moderator approves the request in Telegram itself.
    /// Do not decline an already-admitted member when the old CAPTCHA times out.
    /// Close a pending challenge that no longer needs any Telegram API action.
    /// Used only after independently checking current group membership.
    pub fn close_without_action(&self, c: &Challenge, status: &str, detail: &str) -> Result<bool> {
        anyhow::ensure!(
            matches!(status, "passed" | "failed"),
            "invalid terminal challenge status",
        );
        let changed = self.conn.execute(
            "UPDATE challenges SET status=?1,action=NULL,updated_at=?2
             WHERE chat_id=?3 AND user_id=?4 AND nonce=?5 AND status='pending'",
            params![status, now(), c.chat_id, c.user_id, c.nonce],
        )?;
        if changed == 1 {
            self.audit(c.chat_id, c.user_id, status, detail)?;
        }
        Ok(changed == 1)
    }

    pub fn resolve_external_approval(&self, c: &Challenge) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE challenges SET status='passed',action=NULL,updated_at=?1
             WHERE chat_id=?2 AND user_id=?3 AND nonce=?4
               AND flow='request' AND status IN ('pending','manual')",
            params![now(), c.chat_id, c.user_id, c.nonce],
        )?;
        if changed == 1 {
            self.audit(c.chat_id, c.user_id, "external_approval", "member joined")?;
        }
        Ok(changed == 1)
    }

    pub fn manual(&self, c: &Challenge, reason: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE challenges SET status='manual',updated_at=?1
            WHERE chat_id=?2 AND user_id=?3 AND nonce=?4
              AND status IN ('pending','acting','awaiting_message')",
            params![now(), c.chat_id, c.user_id, c.nonce],
        )?;
        self.audit(c.chat_id, c.user_id, "manual", reason)?;
        Ok(())
    }

    pub fn audit(&self, chat: i64, user: i64, event: &str, detail: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO audit(at,chat_id,user_id,event,detail) VALUES(?1,?2,?3,?4,?5)",
            params![now(), chat, user, event, detail],
        )?;
        Ok(())
    }

    /// Manual overrides may only replace a pending/manual admission decision.
    /// Never overwrite an in-flight external action: its outcome may be unknown.
    pub fn manual_override(&self, c: &Challenge, action: &str) -> Result<bool> {
        if !matches!(
            (c.flow.as_str(), action),
            ("member", "pass" | "kick") | ("request", "pass" | "reject")
        ) {
            anyhow::bail!("invalid manual action for admission flow");
        }
        let affected = self.conn.execute(
            "UPDATE challenges SET status='acting',action=?1,updated_at=?2
             WHERE chat_id=?3 AND user_id=?4 AND nonce=?5
               AND status IN ('pending','manual')",
            params![action, now(), c.chat_id, c.user_id, c.nonce],
        )?;
        Ok(affected == 1)
    }

    pub fn list_open(&self, chat: i64, limit: usize) -> Result<Vec<Challenge>> {
        let mut stmt = self.conn.prepare(
            "SELECT user_id FROM challenges WHERE chat_id=?1
             AND status IN ('pending','acting','manual')
             ORDER BY updated_at DESC LIMIT ?2",
        )?;
        let ids = stmt
            .query_map(params![chat, limit as i64], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|user| self.get(chat, user)?.context("open challenge disappeared"))
            .collect()
    }

    pub fn event_counts(&self, chat: i64) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT event,count(*) FROM audit WHERE chat_id=?1 GROUP BY event")?;
        let counts = stmt
            .query_map([chat], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(counts)
    }

    /// Materialize only work the maintenance loop can actually act upon.
    /// Future CAPTCHA/probation rows stay indexed in SQLite rather than being
    /// decoded, allocated, and looked up after every getUpdates response.
    /// Unconfirmed deliveries must still be surfaced immediately after restart.
    pub fn sweep_candidates(&self, as_of: i64) -> Result<Vec<Challenge>> {
        let mut stmt = self.conn.prepare(
            "SELECT chat_id,user_id FROM challenges
             WHERE status='acting'
                OR (status='pending' AND message_id IS NULL)
                OR (status='pending' AND expires_at<=?1)
                OR (status='awaiting_message' AND expires_at<=?1)",
        )?;
        let keys = stmt
            .query_map([as_of], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        keys.into_iter()
            .map(|(chat, user)| self.get(chat, user)?.context("challenge disappeared"))
            .collect()
    }
}

#[derive(Debug)]
pub struct Settings {
    pub enabled: bool,
    pub timeout: i64,
    pub max_attempts: i64,
    pub links: bool,
    pub lockdown: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_loads_only_actionable_rows_even_with_many_future_challenges() -> Result<()> {
        let store = Store::open(":memory:")?;
        let current = now();
        for user_id in 1..=1_000_i64 {
            store.create(&Challenge {
                chat_id: -10,
                user_id,
                nonce: format!("sweep-{user_id}"),
                flow: "member".into(),
                status: if user_id % 2 == 0 {
                    "awaiting_message"
                } else {
                    "pending"
                }
                .into(),
                delivery_chat_id: -10,
                answer: 7,
                options: vec![3, 5, 7],
                attempts: 0,
                expires_at: current + 600,
                message_id: Some(user_id),
                action: None,
                updated_at: current,
            })?;
        }
        assert_eq!(store.sweep_candidates(current)?.len(), 0);

        // Unsent pending challenge is actionable, regardless of its deadline.
        store
            .conn
            .execute("UPDATE challenges SET message_id=NULL WHERE user_id=1", [])?;
        // Pending and probation whose timers expired must still be returned.
        store.conn.execute(
            "UPDATE challenges SET expires_at=?1 WHERE user_id IN (2,3)",
            [current - 1],
        )?;
        // In-flight API action needs manual reconciliation, even if not expired.
        store
            .conn
            .execute("UPDATE challenges SET status='acting' WHERE user_id=4", [])?;
        // Manual-review entries are not auto-actionable.
        store
            .conn
            .execute("UPDATE challenges SET status='manual' WHERE user_id=5", [])?;
        let candidates = store.sweep_candidates(current)?;
        let mut ids: Vec<i64> = candidates.iter().map(|c| c.user_id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        // A future-dated cutoff cannot pick up new rows from the current instant.
        assert_eq!(
            store
                .sweep_candidates(current - 1)?
                .iter()
                .map(|c| c.user_id)
                .filter(|id| *id == 6)
                .count(),
            0
        );
        Ok(())
    }

    /// Perf experiment, never used as a timing gate on shared CI runners.
    /// The control path is the old Store::open_challenges implementation.
    #[test]
    #[ignore = "run in dedicated performance CI"]
    fn bench_due_sweep_against_legacy() -> Result<()> {
        use std::time::Instant;

        fn legacy_scan(store: &Store) -> Result<Vec<Challenge>> {
            let mut stmt = store.conn.prepare(
                "SELECT chat_id,user_id FROM challenges
                 WHERE status IN ('pending','acting','manual','awaiting_message')",
            )?;
            let keys = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            keys.into_iter()
                .map(|(chat, user)| store.get(chat, user)?.context("challenge disappeared"))
                .collect()
        }

        fn is_due(c: &Challenge, at: i64) -> bool {
            c.status == "acting"
                || (c.status == "pending" && (c.message_id.is_none() || c.expires_at <= at))
                || (c.status == "awaiting_message" && c.expires_at <= at)
        }

        fn run_case(rows: i64, repeats: usize) -> Result<()> {
            let store = Store::open(":memory:")?;
            let at = now();
            for user_id in 1..=rows {
                let status = if user_id % 211 == 0 {
                    "manual"
                } else if user_id % 101 == 0 {
                    "acting"
                } else if user_id % 2 == 0 {
                    "awaiting_message"
                } else {
                    "pending"
                };
                store.create(&Challenge {
                    chat_id: -123,
                    user_id,
                    nonce: format!("bench-{user_id}"),
                    flow: "member".into(),
                    status: status.into(),
                    delivery_chat_id: -123,
                    answer: 5,
                    options: vec![2, 5, 7],
                    attempts: 0,
                    expires_at: if user_id % 103 == 0 { at - 1 } else { at + 600 },
                    message_id: if user_id % 31 == 0 {
                        None
                    } else {
                        Some(user_id)
                    },
                    action: None,
                    updated_at: at,
                })?;
            }

            let old = legacy_scan(&store)?;
            let due = store.sweep_candidates(at)?;
            let mut expected: Vec<i64> = old
                .iter()
                .filter(|c| is_due(c, at))
                .map(|c| c.user_id)
                .collect();
            let mut actual: Vec<i64> = due.iter().map(|c| c.user_id).collect();
            expected.sort_unstable();
            actual.sort_unstable();
            assert_eq!(actual, expected, "must preserve every sanction candidate");

            let mut old_ns = Vec::with_capacity(repeats);
            let mut new_ns = Vec::with_capacity(repeats);
            for iteration in 0..repeats {
                if iteration % 2 == 0 {
                    let start = Instant::now();
                    let result = legacy_scan(&store)?;
                    std::hint::black_box(result.len());
                    old_ns.push(start.elapsed().as_nanos());

                    let start = Instant::now();
                    let result = store.sweep_candidates(at)?;
                    std::hint::black_box(result.len());
                    new_ns.push(start.elapsed().as_nanos());
                } else {
                    let start = Instant::now();
                    let result = store.sweep_candidates(at)?;
                    std::hint::black_box(result.len());
                    new_ns.push(start.elapsed().as_nanos());

                    let start = Instant::now();
                    let result = legacy_scan(&store)?;
                    std::hint::black_box(result.len());
                    old_ns.push(start.elapsed().as_nanos());
                }
            }

            old_ns.sort_unstable();
            new_ns.sort_unstable();
            let p50 = repeats / 2;
            let p95 = ((repeats * 95) / 100).min(repeats - 1);
            println!(
                "BOR_SWEEP_BENCH {}",
                serde_json::json!({
                    "rows_waiting": rows,
                    "rows_materialized_legacy": old.len(),
                    "rows_materialized_due": due.len(),
                    "repeats": repeats,
                    "legacy_p50_us": old_ns[p50] as f64 / 1000.0,
                    "legacy_p95_us": old_ns[p95] as f64 / 1000.0,
                    "new_p50_us": new_ns[p50] as f64 / 1000.0,
                    "new_p95_us": new_ns[p95] as f64 / 1000.0
                })
            );
            Ok(())
        }

        run_case(1_000, 20)?;
        run_case(10_000, 20)?;
        Ok(())
    }

    #[test]
    fn settings_fastpath_preserves_defaults_and_admin_changes() -> Result<()> {
        let s = Store::open(":memory:")?;
        let first = s.settings(-808)?;
        assert!(first.enabled);
        assert_eq!(first.timeout, 120);
        assert_eq!(first.max_attempts, 1);
        assert!(!first.links);
        assert!(!first.lockdown);
        let count: i64 = s.conn.query_row(
            "SELECT COUNT(*) FROM settings WHERE chat_id=?1",
            [-808],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1);

        s.setting(-808, "timeout_s", 150)?;
        s.setting(-808, "links", 1)?;
        for _ in 0..100 {
            let current = s.settings(-808)?;
            assert_eq!(current.timeout, 150);
            assert!(current.links);
        }
        let count: i64 = s.conn.query_row(
            "SELECT COUNT(*) FROM settings WHERE chat_id=?1",
            [-808],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1);
        assert!(s.settings(-909)?.enabled);
        Ok(())
    }

    /// Comparison on the same warmed SQLite connection and same group.
    #[test]
    #[ignore = "run only in the isolated settings performance workflow"]
    fn bench_settings_read_fastpath() -> Result<()> {
        use std::time::Instant;
        let store = Store::open(":memory:")?;
        let chat = -414;
        let original = store.settings(chat)?;
        let iterations: usize = 5_000;
        let repeats: usize = 24;
        let mut old_samples = Vec::with_capacity(repeats);
        let mut new_samples = Vec::with_capacity(repeats);

        fn old_settings(store: &Store, chat: i64) -> Result<Settings> {
            store
                .conn
                .execute("INSERT OR IGNORE INTO settings(chat_id) VALUES(?1)", [chat])?;
            store.conn
                .query_row(
                    "SELECT enabled,timeout_s,attempts,links,lockdown FROM settings WHERE chat_id=?1",
                    [chat],
                    |r| {
                        Ok(Settings {
                            enabled: r.get::<_, i64>(0)? != 0,
                            timeout: r.get(1)?,
                            max_attempts: r.get(2)?,
                            links: r.get::<_, i64>(3)? != 0,
                            lockdown: r.get::<_, i64>(4)? != 0,
                        })
                    },
                )
                .map_err(Into::into)
        }

        for run in 0..repeats {
            let bench_old = || -> Result<u128> {
                let start = Instant::now();
                for _ in 0..iterations {
                    let x = old_settings(&store, chat)?;
                    assert_eq!(x.timeout, original.timeout);
                    std::hint::black_box(x.enabled);
                }
                Ok(start.elapsed().as_micros())
            };
            let bench_new = || -> Result<u128> {
                let start = Instant::now();
                for _ in 0..iterations {
                    let x = store.settings(chat)?;
                    assert_eq!(x.timeout, original.timeout);
                    std::hint::black_box(x.enabled);
                }
                Ok(start.elapsed().as_micros())
            };
            if run % 2 == 0 {
                old_samples.push(bench_old()?);
                new_samples.push(bench_new()?);
            } else {
                new_samples.push(bench_new()?);
                old_samples.push(bench_old()?);
            }
        }
        old_samples.sort_unstable();
        new_samples.sort_unstable();
        println!(
            "BOR_SETTINGS_BENCH {}",
            serde_json::json!({
                "repeats": repeats,
                "reads_per_sample": iterations,
                "old_p50_us": old_samples[repeats / 2],
                "new_p50_us": new_samples[repeats / 2],
                "old_p95_us": old_samples[(repeats * 95 / 100).min(repeats - 1)],
                "new_p95_us": new_samples[(repeats * 95 / 100).min(repeats - 1)]
            })
        );
        Ok(())
    }

    #[test]
    fn one_attempt_and_message_window() -> Result<()> {
        let s = Store::open(":memory:")?;
        let c = Challenge {
            chat_id: -100,
            user_id: 31,
            nonce: "first-message".into(),
            flow: "member".into(),
            status: "pending".into(),
            delivery_chat_id: -100,
            answer: 5,
            options: vec![3, 5, 7],
            attempts: 0,
            expires_at: now() + 120,
            message_id: Some(7),
            action: None,
            updated_at: now(),
        };
        s.create(&c)?;
        assert!(s.claim(&c, "pass")?);
        s.start_first_message_window(&c)?;
        let waiting = s.get(-100, 31)?.context("challenge missing")?;
        assert_eq!(waiting.status, "awaiting_message");
        assert_eq!(waiting.expires_at - waiting.updated_at, 600);
        assert!(!s.record_first_message(-100, 31, 12, waiting.updated_at - 1)?);
        assert!(!s.record_first_message(-100, 31, 12, waiting.expires_at + 1)?);
        assert!(s.record_first_message(-100, 31, 12, waiting.updated_at + 1)?);
        assert!(!s.record_first_message(-100, 31, 12, waiting.updated_at + 2)?);
        assert_eq!(s.get(-100, 31)?.unwrap().status, "passed");
        assert!(!s.claim_first_message_timeout(&waiting)?);
        Ok(())
    }

    #[test]
    fn expired_message_window_reserves_one_kick() -> Result<()> {
        let s = Store::open(":memory:")?;
        let c = Challenge {
            chat_id: -300,
            user_id: 88,
            nonce: "expired-message".into(),
            flow: "request".into(),
            status: "pending".into(),
            delivery_chat_id: 88,
            answer: 12,
            options: vec![11, 12],
            attempts: 0,
            expires_at: now() + 120,
            message_id: Some(5),
            action: None,
            updated_at: now(),
        };
        s.create(&c)?;
        assert!(s.claim(&c, "pass")?);
        s.start_first_message_window(&c)?;
        s.conn.execute(
            "UPDATE challenges SET expires_at=?1 WHERE chat_id=?2 AND user_id=?3",
            params![now() - 1, c.chat_id, c.user_id],
        )?;
        let waiting = s.get(-300, 88)?.context("challenge missing")?;
        assert!(s.claim_first_message_timeout(&waiting)?);
        assert!(!s.claim_first_message_timeout(&waiting)?);
        assert!(!s.record_first_message(-300, 88, 10, now() - 1)?);
        s.finish(&waiting, "failed")?;
        assert_eq!(s.get(-300, 88)?.unwrap().status, "failed");
        Ok(())
    }

    #[test]
    fn durable_offsets_and_settings() -> Result<()> {
        let s = Store::open(":memory:")?;
        s.offset_save(9)?;
        s.offset_save(3)?;
        assert_eq!(s.offset()?, 9);
        assert_eq!(s.settings(-5)?.timeout, 120);
        assert_eq!(s.settings(-5)?.max_attempts, 1);
        assert!(s.setting(-5, "attempts", 3).is_err());
        assert!(s.setting(-5, "timeout_s", 120).is_ok());
        assert_eq!(s.settings(-5)?.timeout, 120);
        assert!(s.setting(-5, "foo", 120).is_err());
        Ok(())
    }

    #[test]
    fn nonce_prevents_stale_claim() -> Result<()> {
        let s = Store::open(":memory:")?;
        let mut c = Challenge {
            chat_id: -10,
            user_id: 44,
            nonce: "first".into(),
            flow: "member".into(),
            status: "pending".into(),
            delivery_chat_id: -10,
            answer: 4,
            options: vec![4, 9],
            attempts: 0,
            expires_at: now() + 100,
            message_id: None,
            action: None,
            updated_at: now(),
        };
        s.create(&c)?;
        c.nonce = "second".into();
        s.create(&c)?;
        let mut stale = c.clone();
        stale.nonce = "first".into();
        assert!(!s.claim(&stale, "pass")?);
        assert!(s.claim(&c, "pass")?);
        assert!(!s.claim(&c, "pass")?);
        s.finish(&c, "passed")?;
        assert_eq!(s.get(-10, 44)?.unwrap().status, "passed");
        Ok(())
    }
    #[test]
    fn manual_override_survives_process_restart() -> Result<()> {
        let file = std::env::temp_dir().join(format!(
            "rebuild-admission-{}-{}.sqlite3",
            std::process::id(),
            rand::random::<u64>()
        ));
        let location = file.to_str().context("invalid path")?;
        let initial = Store::open(location)?;
        assert!(
            Store::open(location).is_err(),
            "a second instance must not use the same live database"
        );
        let c = Challenge {
            chat_id: -101,
            user_id: 72,
            nonce: "restart-safe".into(),
            flow: "member".into(),
            status: "pending".into(),
            delivery_chat_id: -101,
            answer: 7,
            options: vec![5, 7, 9],
            attempts: 0,
            expires_at: now() + 60,
            message_id: None,
            action: None,
            updated_at: now(),
        };
        initial.create(&c)?;
        initial.manual(&c, "Telegram API rejected restriction")?;
        initial.offset_save(77)?;
        drop(initial);

        let after = Store::open(location)?;
        assert_eq!(after.offset()?, 77);
        let review = after.get(-101, 72)?.context("missing admission state")?;
        assert_eq!(review.status, "manual");
        assert!(after.manual_override(&review, "reject").is_err());
        assert!(after.manual_override(&review, "pass")?);
        assert!(!after.manual_override(&review, "pass")?);
        let acting = after.get(-101, 72)?.context("missing acting state")?;
        assert_eq!(acting.action.as_deref(), Some("pass"));
        after.finish(&acting, "passed")?;
        let final_state = after.get(-101, 72)?.context("missing passed state")?;
        assert_eq!(final_state.status, "passed");
        assert!(!after.manual_override(&final_state, "kick")?);
        assert_eq!(
            after
                .event_counts(-101)?
                .iter()
                .map(|(_, n)| n)
                .sum::<i64>(),
            3
        );
        drop(after);
        std::fs::remove_file(&file)?;
        Ok(())
    }

    #[test]
    fn external_approval_cancels_request_timeout_and_cannot_override_acting() -> Result<()> {
        let store = Store::open(":memory:")?;
        let c = Challenge {
            chat_id: -101,
            user_id: 17,
            nonce: "request".into(),
            flow: "request".into(),
            status: "pending".into(),
            delivery_chat_id: 17,
            answer: 12,
            options: vec![9, 12],
            attempts: 0,
            expires_at: now() - 3,
            message_id: Some(200),
            action: None,
            updated_at: now() - 30,
        };
        store.create(&c)?;
        assert!(store.resolve_external_approval(&c)?);
        assert!(!store.resolve_external_approval(&c)?);
        let done = store.get(-101, 17)?.context("request not found")?;
        assert_eq!(done.status, "passed");
        assert!(!store.claim(&done, "reject")?);

        let mut another = c.clone();
        another.nonce = "second".to_owned();
        another.user_id = 18;
        store.create(&another)?;
        assert!(store.claim(&another, "reject")?);
        assert!(!store.resolve_external_approval(&another)?);
        assert_eq!(store.get(-101, 18)?.unwrap().status, "acting");
        Ok(())
    }

    #[test]
    fn duplicate_callback_after_database_reopen_does_not_count_twice() -> Result<()> {
        let file = std::env::temp_dir().join(format!(
            "rebuild-callback-{}-{}.sqlite3",
            std::process::id(),
            rand::random::<u64>()
        ));
        let path = file.to_str().context("invalid temporary path")?;
        let first = Store::open(path)?;
        let c = Challenge {
            chat_id: -42,
            user_id: 8,
            nonce: "session".into(),
            flow: "member".into(),
            status: "pending".into(),
            delivery_chat_id: -42,
            answer: 10,
            options: vec![5, 10, 11],
            attempts: 0,
            expires_at: now() + 60,
            message_id: Some(3),
            action: None,
            updated_at: now(),
        };
        first.create(&c)?;
        assert_eq!(
            first.wrong_once(&c, 2, "callback-unique")?,
            (1, false, true)
        );
        drop(first);

        let reopened = Store::open(path)?;
        let state = reopened.get(-42, 8)?.context("lost challenge")?;
        assert_eq!(state.attempts, 1);
        assert_eq!(
            reopened.wrong_once(&state, 2, "callback-unique")?,
            (0, false, false)
        );
        assert_eq!(reopened.get(-42, 8)?.unwrap().attempts, 1);
        drop(reopened);
        std::fs::remove_file(&file)?;
        Ok(())
    }

    #[test]
    fn terminal_noop_requires_pending_nonce() -> Result<()> {
        let db = Store::open(":memory:")?;
        let c = Challenge {
            chat_id: -9,
            user_id: 8,
            nonce: "nonce".into(),
            flow: "member".into(),
            status: "pending".into(),
            delivery_chat_id: -9,
            answer: 7,
            options: vec![7],
            attempts: 0,
            expires_at: now() - 1,
            message_id: Some(200),
            action: None,
            updated_at: now(),
        };
        db.create(&c)?;
        let mut stale = c.clone();
        stale.nonce = "old".into();
        assert!(!db.close_without_action(&stale, "passed", "external admin")?);
        assert!(db.close_without_action(&c, "passed", "external admin")?);
        assert_eq!(db.get(-9, 8)?.unwrap().status, "passed");
        assert!(!db.close_without_action(&c, "failed", "old timeout")?);
        assert!(db.close_without_action(&c, "manual", "bad").is_err());
        Ok(())
    }

    #[test]
    fn wrong_answer_requires_live_matching_nonce() -> Result<()> {
        let store = Store::open(":memory:")?;
        let c = Challenge {
            chat_id: -5,
            user_id: 51,
            nonce: "active".into(),
            flow: "request".into(),
            status: "pending".into(),
            delivery_chat_id: 51,
            answer: 8,
            options: vec![6, 8, 10],
            attempts: 0,
            expires_at: now() + 60,
            message_id: None,
            action: None,
            updated_at: now(),
        };
        store.create(&c)?;
        let mut wrong = c.clone();
        wrong.nonce = "obsolete".into();
        assert_eq!(store.wrong_once(&wrong, 2, "cb-stale")?, (0, false, false));
        assert_eq!(store.wrong_once(&c, 2, "cb-1")?, (1, false, true));
        assert_eq!(store.wrong_once(&c, 2, "cb-1")?, (0, false, false));
        assert_eq!(store.wrong_once(&c, 2, "cb-2")?, (0, true, true));
        assert_eq!(store.get(-5, 51)?.unwrap().status, "acting");
        assert_eq!(
            store.get(-5, 51)?.unwrap().action.as_deref(),
            Some("reject")
        );
        assert!(!store.claim(&c, "pass")?);
        assert_eq!(store.wrong_once(&c, 2, "cb-3")?, (0, false, false));
        assert_eq!(store.get(-5, 51)?.unwrap().attempts, 2);
        Ok(())
    }
}
