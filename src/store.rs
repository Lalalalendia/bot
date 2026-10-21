use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::{path::Path, time::{SystemTime, UNIX_EPOCH}};

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
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
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        if path != ":memory:" {
            if let Some(parent) = Path::new(path).parent() {
                std::fs::create_dir_all(parent).context("create DB directory")?;
            }
        }
        let conn = Connection::open(path).context("open SQLite")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS state(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS settings(
               chat_id INTEGER PRIMARY KEY, enabled INTEGER NOT NULL DEFAULT 1,
               timeout_s INTEGER NOT NULL DEFAULT 90, attempts INTEGER NOT NULL DEFAULT 2,
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
             );"
        )?;
        Ok(Self { conn })
    }

    pub fn offset(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT value FROM state WHERE key='offset'", [], |r| r.get(0))
            .optional()?.unwrap_or(0))
    }

    pub fn offset_save(&self, offset: i64) -> Result<()> {
        self.conn.execute("INSERT INTO state(key,value) VALUES('offset',?1)
            ON CONFLICT(key) DO UPDATE SET value=max(value,excluded.value)", [offset])?;
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
        self.conn.execute("INSERT OR IGNORE INTO settings(chat_id) VALUES(?1)", [chat])?;
        self.conn.query_row("SELECT enabled,timeout_s,attempts,links,lockdown FROM settings WHERE chat_id=?1",
            [chat], |r| Ok(Settings {
                enabled: r.get::<_,i64>(0)? != 0, timeout: r.get(1)?,
                max_attempts: r.get(2)?, links: r.get::<_,i64>(3)? != 0,
                lockdown: r.get::<_,i64>(4)? != 0,
            })).map_err(Into::into)
    }

    pub fn setting(&self, chat: i64, key: &str, value: i64) -> Result<()> {
        let column = match key {
            "enabled" => "enabled", "timeout_s" => "timeout_s",
            "attempts" => "attempts", "links" => "links", "lockdown" => "lockdown",
            _ => anyhow::bail!("invalid settings field"),
        };
        self.settings(chat)?;
        self.conn.execute(&format!("UPDATE settings SET {column}=?1 WHERE chat_id=?2"),
            params![value, chat])?;
        Ok(())
    }

    pub fn get(&self, chat: i64, user: i64) -> Result<Option<Challenge>> {
        let mut stmt = self.conn.prepare(
            "SELECT chat_id,user_id,nonce,flow,status,delivery_chat_id,answer,
             options,attempts,expires_at,message_id,action,updated_at
             FROM challenges WHERE chat_id=?1 AND user_id=?2",
        )?;
        let result = stmt.query_row(params![chat, user], |row| {
            let encoded: String = row.get(7)?;
            let options = serde_json::from_str(&encoded).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    7, rusqlite::types::Type::Text, Box::new(err),
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
        }).optional()?;
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
        self.audit(c.chat_id,c.user_id,"challenge",&c.flow)?;
        Ok(())
    }

    pub fn mark_sent(&self, c: &Challenge, message_id: i64) -> Result<()> {
        self.conn.execute("UPDATE challenges SET message_id=?1,updated_at=?2
            WHERE chat_id=?3 AND user_id=?4 AND nonce=?5 AND status='pending'",
            params![message_id,now(),c.chat_id,c.user_id,c.nonce])?;
        Ok(())
    }

    pub fn wrong(&self, c: &Challenge, max: i64) -> Result<(i64,bool)> {
        let updated=self.conn.execute("UPDATE challenges SET attempts=attempts+1,updated_at=?1
            WHERE chat_id=?2 AND user_id=?3 AND nonce=?4 AND status='pending'",
            params![now(),c.chat_id,c.user_id,c.nonce])?;
        if updated==0 {
            return Ok((0,false));
        }
        let Some(fresh) = self.get(c.chat_id,c.user_id)? else { return Ok((0,false)); };
        let left = (max-fresh.attempts).max(0);
        Ok((left,left==0))
    }

    /// Persist the intended external action before making a Telegram API request.
    pub fn claim(&self, c: &Challenge, action: &str) -> Result<bool> {
        let n = self.conn.execute("UPDATE challenges SET status='acting',action=?1,updated_at=?2
            WHERE chat_id=?3 AND user_id=?4 AND nonce=?5 AND status='pending'",
            params![action,now(),c.chat_id,c.user_id,c.nonce])?;
        Ok(n == 1)
    }

    pub fn finish(&self, c: &Challenge, status: &str) -> Result<()> {
        self.conn.execute("UPDATE challenges SET status=?1,action=NULL,updated_at=?2
            WHERE chat_id=?3 AND user_id=?4 AND nonce=?5 AND status='acting'",
            params![status,now(),c.chat_id,c.user_id,c.nonce])?;
        self.audit(c.chat_id,c.user_id,status,"")?;
        Ok(())
    }

    pub fn manual(&self, c: &Challenge, reason: &str) -> Result<()> {
        self.conn.execute("UPDATE challenges SET status='manual',updated_at=?1
            WHERE chat_id=?2 AND user_id=?3 AND nonce=?4 AND status IN ('pending','acting')",
            params![now(),c.chat_id,c.user_id,c.nonce])?;
        self.audit(c.chat_id,c.user_id,"manual",reason)?;
        Ok(())
    }

    pub fn audit(&self, chat: i64, user: i64, event: &str, detail: &str) -> Result<()> {
        self.conn.execute("INSERT INTO audit(at,chat_id,user_id,event,detail) VALUES(?1,?2,?3,?4,?5)",
            params![now(),chat,user,event,detail])?;
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
        let ids = stmt.query_map(params![chat, limit as i64], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter().map(|user| {
            self.get(chat, user)?.context("open challenge disappeared")
        }).collect()
    }

    pub fn event_counts(&self, chat: i64) -> Result<Vec<(String, i64)>> {
        let mut stmt=self.conn.prepare(
            "SELECT event,count(*) FROM audit WHERE chat_id=?1 GROUP BY event"
        )?;
        let counts=stmt.query_map([chat],|r| Ok((r.get(0)?,r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(counts)
    }

    pub fn open_challenges(&self) -> Result<Vec<Challenge>> {
        let mut stmt=self.conn.prepare("SELECT chat_id,user_id FROM challenges WHERE status IN ('pending','acting','manual')")?;
        let keys=stmt.query_map([],|r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        keys.into_iter().map(|(chat,user)| self.get(chat,user)?.context("challenge disappeared")).collect()
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
    fn durable_offsets_and_settings() -> Result<()> {
        let s=Store::open(":memory:")?;
        s.offset_save(9)?;
        s.offset_save(3)?;
        assert_eq!(s.offset()?,9);
        assert_eq!(s.settings(-5)?.timeout,90);
        assert!(s.setting(-5,"timeout_s",120).is_ok());
        assert_eq!(s.settings(-5)?.timeout,120);
        assert!(s.setting(-5,"foo",120).is_err());
        Ok(())
    }

    #[test]
    fn nonce_prevents_stale_claim() -> Result<()> {
        let s=Store::open(":memory:")?;
        let mut c=Challenge {chat_id:-10,user_id:44,nonce:"first".into(),flow:"member".into(),
            status:"pending".into(),delivery_chat_id:-10,answer:4,options:vec![4,9],
            attempts:0,expires_at:now()+100,message_id:None,action:None,updated_at:now()};
        s.create(&c)?;
        c.nonce="second".into();
        s.create(&c)?;
        let mut stale=c.clone();
        stale.nonce="first".into();
        assert!(!s.claim(&stale,"pass")?);
        assert!(s.claim(&c,"pass")?);
        assert!(!s.claim(&c,"pass")?);
        s.finish(&c,"passed")?;
        assert_eq!(s.get(-10,44)?.unwrap().status,"passed");
        Ok(())
    }
    #[test]
    fn manual_override_survives_process_restart() -> Result<()> {
        let file=std::env::temp_dir().join(format!(
            "rebuild-admission-{}-{}.sqlite3",std::process::id(),rand::random::<u64>()
        ));
        let location=file.to_str().context("invalid path")?;
        let initial=Store::open(location)?;
        let c=Challenge {
            chat_id:-101,user_id:72,nonce:"restart-safe".into(),
            flow:"member".into(),status:"pending".into(),
            delivery_chat_id:-101,answer:7,options:vec![5,7,9],
            attempts:0,expires_at:now()+60,message_id:None,
            action:None,updated_at:now(),
        };
        initial.create(&c)?;
        initial.manual(&c,"Telegram API rejected restriction")?;
        initial.offset_save(77)?;
        drop(initial);

        let after=Store::open(location)?;
        assert_eq!(after.offset()?,77);
        let review=after.get(-101,72)?.context("missing admission state")?;
        assert_eq!(review.status,"manual");
        assert!(after.manual_override(&review,"reject").is_err());
        assert!(after.manual_override(&review,"pass")?);
        assert!(!after.manual_override(&review,"pass")?);
        let acting=after.get(-101,72)?.context("missing acting state")?;
        assert_eq!(acting.action.as_deref(),Some("pass"));
        after.finish(&acting,"passed")?;
        let final_state=after.get(-101,72)?.context("missing passed state")?;
        assert_eq!(final_state.status,"passed");
        assert!(!after.manual_override(&final_state,"kick")?);
        assert_eq!(after.event_counts(-101)?.iter().map(|(_,n)|n).sum::<i64>(),3);
        drop(after);
        std::fs::remove_file(&file)?;
        Ok(())
    }

    #[test]
    fn wrong_answer_requires_live_matching_nonce() -> Result<()> {
        let store=Store::open(":memory:")?;
        let c=Challenge {
            chat_id:-5,user_id:51,nonce:"active".into(),
            flow:"request".into(),status:"pending".into(),
            delivery_chat_id:51,answer:8,options:vec![6,8,10],
            attempts:0,expires_at:now()+60,message_id:None,
            action:None,updated_at:now(),
        };
        store.create(&c)?;
        let mut wrong=c.clone();
        wrong.nonce="obsolete".into();
        assert_eq!(store.wrong(&wrong,2)?,(0,false));
        assert_eq!(store.wrong(&c,2)?,(1,false));
        assert_eq!(store.wrong(&c,2)?,(0,true));
        assert!(store.claim(&c,"reject")?);
        assert_eq!(store.wrong(&c,2)?,(0,false));
        assert_eq!(store.get(-5,51)?.unwrap().attempts,2);
        Ok(())
    }

}
