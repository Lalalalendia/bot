use crate::{
    model::{Callback, JoinRequest, MemberChange, Message, Update, User},
    store::{now, Challenge, Store},
    telegram::Telegram,
};
use anyhow::{Context, Result};
use rand::{seq::SliceRandom, Rng};
use serde_json::{json, Value};
use std::{collections::HashMap, time::Duration};

fn stale_event(event_at: i64, now_at: i64) -> bool {
    event_at > 0 && now_at.saturating_sub(event_at) > 600
}

fn skip_existing(previous: &Challenge, new_flow: &str, now_at: i64) -> bool {
    if matches!(
        previous.status.as_str(),
        "pending" | "acting" | "manual" | "awaiting_message"
    ) {
        return true;
    }
    // Telegram emits chat_member when a request is approved; never impose a
    // second CAPTCHA on someone who just passed the pre-join verification.
    if previous.status == "passed" {
        let elapsed = now_at.saturating_sub(previous.updated_at);
        if previous.flow == "request" && elapsed < 300 {
            return matches!(new_flow, "member" | "request");
        }
        if previous.flow == "member" && new_flow == "member" && elapsed < 12 {
            return true;
        }
    }
    false
}

/// Never elevate a member beyond the group defaults when CAPTCHA is passed.
/// Missing group permission information is a manual-review condition, not
/// permission to enable all message types.
fn default_chat_permissions(chat: &Value) -> Result<Value> {
    let source = chat["permissions"]
        .as_object()
        .context("getChat did not return default group permissions")?;
    let mut permissions = serde_json::Map::new();
    for field in [
        "can_send_messages",
        "can_send_audios",
        "can_send_documents",
        "can_send_photos",
        "can_send_videos",
        "can_send_video_notes",
        "can_send_voice_notes",
        "can_send_polls",
        "can_send_other_messages",
        "can_add_web_page_previews",
        "can_react_to_messages",
        "can_edit_tag",
        "can_change_info",
        "can_invite_users",
        "can_pin_messages",
        "can_manage_topics",
    ] {
        if let Some(value) = source.get(field) {
            if !value.is_boolean() {
                anyhow::bail!("invalid group permission type");
            }
            permissions.insert(field.to_string(), value.clone());
        }
    }
    if permissions.is_empty() {
        anyhow::bail!("group permissions are absent; manual review required");
    }
    Ok(Value::Object(permissions))
}

fn no_send_permissions() -> Value {
    let mut permissions = serde_json::Map::new();
    for name in [
        "can_send_messages",
        "can_send_audios",
        "can_send_documents",
        "can_send_photos",
        "can_send_videos",
        "can_send_video_notes",
        "can_send_voice_notes",
        "can_send_polls",
        "can_send_other_messages",
        "can_add_web_page_previews",
        "can_react_to_messages",
        "can_edit_tag",
        "can_change_info",
        "can_invite_users",
        "can_pin_messages",
        "can_manage_topics",
    ] {
        permissions.insert(name.to_string(), Value::Bool(false));
    }
    Value::Object(permissions)
}

pub struct Bot {
    store: Store,
    tg: Telegram,
    id: i64,
    username: Option<String>,
}

impl Bot {
    pub fn new(store: Store, tg: Telegram) -> Self {
        Self {
            store,
            tg,
            id: 0,
            username: None,
        }
    }

    pub async fn run(mut self) -> Result<()> {
        let me = self
            .tg
            .call("getMe", json!({}))
            .await
            .context("Bot token validation")?;
        self.id = me["id"].as_i64().context("getMe.id missing")?;
        self.username = me["username"].as_str().map(str::to_owned);
        tracing::info!(bot_id = self.id, "polling initialized");
        // Do not destroy an existing webhook just because this binary started.
        // Switching delivery modes is an explicit administrator operation.
        let webhook = self.tg.call("getWebhookInfo", json!({})).await?;
        if webhook["url"].as_str().is_some_and(|url| !url.is_empty()) {
            anyhow::bail!("Telegram webhook is configured; refuse to start long polling");
        }
        let mut failures: HashMap<i64, u8> = HashMap::new();
        let mut poll_errors = 0u32;
        loop {
            let offset = self.store.offset()?;
            match self.tg.call("getUpdates",json!({
                "offset":offset,"limit":50,"timeout":25,
                "allowed_updates":["message","edited_message","callback_query","chat_member","chat_join_request"]
            })).await {
                Ok(raw)=>{
                    poll_errors=0;
                    let updates: Vec<Value>=serde_json::from_value(raw)
                        .context("Telegram getUpdates returned a non-array")?;
                    for event in updates {
                        let id=event["update_id"].as_i64()
                            .context("Telegram update missing numeric update_id")?;
                        if id<self.store.offset()? {continue;}
                        let Some(update)=self.decode_update(event)? else {
                            // The individual malformed update is quarantined.
                            // Remaining updates in the same batch still progress.
                            continue;
                        };
                        match self.handle(update).await {
                            Ok(())=>{self.store.offset_save(id+1)?;failures.remove(&id);},
                            Err(err)=>{
                                let count=failures.entry(id).or_default();
                                *count=count.saturating_add(1);
                                tracing::error!(update_id=id,attempt=*count,error=%err,"update failed");
                                if *count>=5 {
                                    self.store.deadletter(id,&err.to_string())?;
                                    self.store.offset_save(id+1)?;
                                    failures.remove(&id);
                                } else {
                                    tokio::time::sleep(Duration::from_secs(1u64 << (*count).min(5))).await;
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(e)=>{
                    poll_errors=poll_errors.saturating_add(1);
                    tracing::warn!(error=%e,"poll error");
                    tokio::time::sleep(Duration::from_secs(2u64.pow(poll_errors.min(5)))).await;
                }
            }
            if let Err(e) = self.sweep().await {
                tracing::error!(error=%e,"maintenance");
            }
        }
    }

    fn decode_update(&self, event: Value) -> Result<Option<Update>> {
        let id = event["update_id"]
            .as_i64()
            .context("Telegram update missing update_id")?;
        match serde_json::from_value(event) {
            Ok(update) => Ok(Some(update)),
            Err(error) => {
                // Do not let a single malformed event stop the poller or
                // prevent subsequent independent updates from being processed.
                self.store
                    .deadletter(id, &format!("update decode failed: {error}"))?;
                self.store.offset_save(id + 1)?;
                tracing::warn!(update_id=id,error=%error,"invalid Telegram update quarantined");
                Ok(None)
            }
        }
    }

    async fn admin(&self, chat: i64, user: i64) -> Result<bool> {
        let result = self
            .tg
            .call("getChatMember", json!({"chat_id":chat,"user_id":user}))
            .await?;
        Ok(matches!(
            result["status"].as_str(),
            Some("administrator" | "creator")
        ))
    }

    fn manual_admission(
        &self,
        chat: i64,
        user: &User,
        flow: &str,
        delivery: i64,
        reason: &str,
    ) -> Result<()> {
        if let Some(existing) = self.store.get(chat, user.id)? {
            if skip_existing(&existing, flow, now()) {
                return Ok(());
            }
        }
        let entry = Challenge {
            chat_id: chat,
            user_id: user.id,
            nonce: format!("{:016x}", rand::random::<u64>()),
            flow: flow.into(),
            status: "manual".into(),
            delivery_chat_id: delivery,
            answer: 0,
            options: Vec::new(),
            attempts: 0,
            expires_at: now(),
            message_id: None,
            action: None,
            updated_at: now(),
        };
        self.store.create(&entry)?;
        self.store.audit(chat, user.id, "manual", reason)?;
        Ok(())
    }

    async fn challenge(&self, chat: i64, user: &User, flow: &str, delivery: i64) -> Result<()> {
        if user.is_bot || user.id == self.id {
            return Ok(());
        }
        // Reconcile externally admitted users before checking group settings.
        // Telegram can send chat_member when a moderator approved a pending
        // join request directly in the Telegram interface.
        if let Some(previous) = self.store.get(chat, user.id)? {
            if flow == "member"
                && previous.flow == "request"
                && matches!(previous.status.as_str(), "pending" | "manual")
                && self.store.resolve_external_approval(&previous)?
            {
                if let Some(message) = previous.message_id {
                    if let Err(error) = self
                        .tg
                        .call(
                            "deleteMessage",
                            json!({
                                "chat_id":previous.delivery_chat_id,
                                "message_id":message
                            }),
                        )
                        .await
                    {
                        tracing::debug!(chat,user=user.id,error=%error,
                            "could not clean up externally approved CAPTCHA");
                    }
                }
                return Ok(());
            }
            if skip_existing(&previous, flow, now()) {
                return Ok(());
            }
        }
        let settings = self.store.settings(chat)?;
        if !settings.enabled {
            return Ok(());
        }
        if flow == "member" {
            let member = match self
                .tg
                .call(
                    "getChatMember",
                    json!({
                        "chat_id":chat,"user_id":user.id
                    }),
                )
                .await
            {
                Ok(member) => member,
                Err(error) => {
                    self.manual_admission(
                        chat,
                        user,
                        flow,
                        delivery,
                        "unable to verify existing membership/administrator rights",
                    )?;
                    tracing::error!(chat,user=user.id,error=%error,
                        "new member requires administrator review");
                    return Ok(());
                }
            };
            match member["status"].as_str() {
                Some("administrator" | "creator") => return Ok(()),
                Some("restricted") => {
                    self.store
                        .audit(chat, user.id, "manual", "existing restrictions")?;
                    return Ok(());
                }
                Some("left" | "kicked") => return Ok(()),
                _ => {}
            }
        }
        if settings.lockdown {
            // Never blindly repeat an uncertain ban/decline after an API error.
            self.store.audit(chat, user.id, "lockdown_attempt", flow)?;
            let action = if flow == "request" {
                self.tg
                    .call(
                        "declineChatJoinRequest",
                        json!({
                            "chat_id":chat,"user_id":user.id
                        }),
                    )
                    .await
            } else {
                self.tg
                    .call(
                        "banChatMember",
                        json!({
                            "chat_id":chat,"user_id":user.id,"until_date":now()+60
                        }),
                    )
                    .await
            };
            match action {
                Ok(_) => self.store.audit(chat, user.id, "lockdown", flow)?,
                Err(error) => {
                    self.store
                        .audit(chat, user.id, "manual", "lockdown outcome unknown")?;
                    tracing::error!(chat,user=user.id,error=%error,
                        "manual verification of lockdown required");
                }
            }
            return Ok(());
        }
        let a = rand::thread_rng().gen_range(1..=9);
        let b = rand::thread_rng().gen_range(1..=9);
        let answer = a + b;
        let mut options: Vec<i64> = (2..=18).collect();
        options.shuffle(&mut rand::thread_rng());
        options.truncate(9);
        if !options.contains(&answer) {
            options.pop();
            options.push(answer);
        }
        options.sort_unstable();
        let nonce = format!("{:016x}", rand::random::<u64>());
        let c = Challenge {
            chat_id: chat,
            user_id: user.id,
            nonce,
            flow: flow.into(),
            status: "pending".into(),
            delivery_chat_id: delivery,
            answer,
            options,
            attempts: 0,
            expires_at: now() + settings.timeout,
            message_id: None,
            action: None,
            updated_at: now(),
        };
        self.store.create(&c)?;
        // Persist the challenge before contacting Telegram. The row makes
        // permission-denied failures visible to /pending, even after a restart.
        // Restrict before delivering a challenge so no unrestricted window
        // is created by the CAPTCHA itself.
        if flow == "member" {
            let result = self
                .tg
                .call(
                    "restrictChatMember",
                    json!({
                        "chat_id":chat,"user_id":user.id,
                        "permissions":no_send_permissions(),
                        "use_independent_chat_permissions":true
                    }),
                )
                .await;
            if let Err(e) = result {
                self.store
                    .audit(chat, user.id, "restrict_failed", &e.to_string())?;
                // If Telegram refuses to restrict, attempt removal instead of
                // silently leaving the newcomer able to send messages.
                if self.store.claim(&c, "kick")? {
                    self.perform(&c, "kick").await?;
                }
                tracing::error!(chat,user=user.id,error=%e,
                    "Could not restrict member; fail-closed removal requested");
                return Ok(());
            }
        }
        self.deliver(&c, a, b).await?;
        Ok(())
    }

    async fn deliver(&self, c: &Challenge, a: i64, b: i64) -> Result<()> {
        let keyboard: Vec<Vec<Value>> = c
            .options
            .iter()
            .map(|value| {
                vec![json!({
                    "text":value.to_string(),
                    "callback_data":format!("q:{}:{}:{}:{}",c.chat_id,c.user_id,c.nonce,value)
                })]
            })
            .collect();
        let msg = self
            .tg
            .call(
                "sendMessage",
                json!({
                    "chat_id":c.delivery_chat_id,
                    "text":format!("Проверка для {}. Сколько будет {} + {}?",c.user_id,a,b),
                    "reply_markup":{"inline_keyboard":keyboard}
                }),
            )
            .await;
        match msg {
            Ok(msg) => {
                self.store
                    .mark_sent(c, msg["message_id"].as_i64().context("missing CAPTCHA id")?)?;
            }
            Err(e) => {
                self.store
                    .manual(c, &format!("CAPTCHA delivery failed: {e}"))?;
                tracing::warn!(chat=c.chat_id,user=c.user_id,error=%e,"manual admission required");
            }
        }
        Ok(())
    }

    async fn join(&self, request: JoinRequest) -> Result<()> {
        tracing::debug!(
            event_at = request.date,
            chat_id = request.chat.id,
            "join request received"
        );
        if stale_event(request.date, now()) {
            tracing::warn!(chat = request.chat.id, "Ignoring stale join request");
            return Ok(());
        }
        if request.query_id.is_some() {
            self.manual_admission(
                request.chat.id,
                &request.user,
                "request",
                request.chat.id,
                "Guardian join request requires manual review",
            )?;
            return Ok(());
        }
        let Some(delivery) = request.user_chat_id else {
            self.manual_admission(
                request.chat.id,
                &request.user,
                "request",
                request.chat.id,
                "missing user_chat_id; unable to send CAPTCHA",
            )?;
            return Ok(());
        };
        self.challenge(request.chat.id, &request.user, "request", delivery)
            .await
    }

    async fn member(&self, change: MemberChange) -> Result<()> {
        tracing::debug!(
            event_at = change.date,
            chat_id = change.chat.id,
            "membership change received"
        );
        if stale_event(change.date, now()) {
            tracing::warn!(chat = change.chat.id, "Ignoring stale membership update");
            return Ok(());
        }
        if !change.old_chat_member.in_chat() && change.new_chat_member.in_chat() {
            self.challenge(
                change.chat.id,
                &change.new_chat_member.user,
                "member",
                change.chat.id,
            )
            .await?;
        }
        Ok(())
    }

    async fn callback(&self, c: Callback) -> Result<()> {
        let Some(data) = c.data.as_deref() else {
            return Ok(());
        };
        let fields: Vec<&str> = data.split(':').collect();
        if fields.len() != 5 || fields[0] != "q" {
            return Ok(());
        }
        let Some(chat) = fields[1].parse::<i64>().ok() else {
            return Ok(());
        };
        let Some(user) = fields[2].parse::<i64>().ok() else {
            return Ok(());
        };
        let Some(value) = fields[4].parse::<i64>().ok() else {
            return Ok(());
        };
        let mut reply = "Проверка устарела";
        if user == c.user.id {
            if let Some(state) = self.store.get(chat, user)? {
                if state.nonce == fields[3]
                    && state.status == "pending"
                    && state.expires_at > now()
                    && state.options.contains(&value)
                {
                    if value == state.answer {
                        if self.store.claim(&state, "pass")? {
                            self.perform(&state, "pass").await?;
                        }
                        // A correct answer is not proof that Telegram actually
                        // restored permissions or approved the request.
                        let current = self
                            .store
                            .get(chat, user)?
                            .context("challenge missing after acceptance")?;
                        reply = if matches!(current.status.as_str(), "passed" | "awaiting_message")
                        {
                            "Проверка пройдена. Напишите сообщение в группе за 10 минут."
                        } else {
                            "Ответ правильный; требуется проверка администратора"
                        };
                    } else {
                        let (left, reject, fresh) = self.store.wrong_once(&state, 1, &c.id)?;
                        if fresh {
                            if reject {
                                // The transaction already reserved the terminal
                                // action, so replay cannot turn failure into pass.
                                self.perform(
                                    &state,
                                    if state.flow == "member" {
                                        "kick"
                                    } else {
                                        "reject"
                                    },
                                )
                                .await?;
                            }
                            reply = if left == 0 {
                                "Проверка не пройдена"
                            } else {
                                "Неверный ответ"
                            };
                        } else {
                            reply = "Ответ уже обработан";
                        }
                    }
                }
            }
        } else {
            reply = "Это проверка другого участника";
        }
        self.tg
            .call(
                "answerCallbackQuery",
                json!({"callback_query_id":c.id,"text":reply}),
            )
            .await?;
        Ok(())
    }

    async fn perform(&self, c: &Challenge, action: &str) -> Result<()> {
        // The intended action is stored as "acting" before calling Telegram.
        // On a crash/uncertain response we do not blindly reissue the action.
        let result = match (c.flow.as_str(), action) {
            ("member", "pass") => {
                match self.tg.call("getChat", json!({"chat_id":c.chat_id})).await {
                    Ok(chat) => match default_chat_permissions(&chat) {
                        Ok(permissions) => {
                            self.tg
                                .call(
                                    "restrictChatMember",
                                    json!({
                                        "chat_id":c.chat_id,"user_id":c.user_id,
                                        "permissions":permissions,
                                        "use_independent_chat_permissions":true
                                    }),
                                )
                                .await
                        }
                        Err(err) => Err(err),
                    },
                    Err(err) => Err(err),
                }
            }
            ("member" | "request", "kick") => {
                self.tg
                    .call(
                        "banChatMember",
                        json!({
                            "chat_id":c.chat_id,"user_id":c.user_id,"until_date":now()+86400
                        }),
                    )
                    .await
            }
            ("request", "pass") => {
                self.tg
                    .call(
                        "approveChatJoinRequest",
                        json!({
                            "chat_id":c.chat_id,"user_id":c.user_id
                        }),
                    )
                    .await
            }
            ("request", "reject") => {
                self.tg
                    .call(
                        "declineChatJoinRequest",
                        json!({
                            "chat_id":c.chat_id,"user_id":c.user_id
                        }),
                    )
                    .await
            }
            _ => anyhow::bail!("invalid external action {action}"),
        };
        match result {
            Ok(_) => {
                if action == "pass" {
                    self.store.start_first_message_window(c)?;
                } else {
                    self.store.finish(c, "failed")?;
                }
                if let Some(message) = c.message_id {
                    if let Err(e) = self
                        .tg
                        .call(
                            "deleteMessage",
                            json!({
                                "chat_id":c.delivery_chat_id,"message_id":message
                            }),
                        )
                        .await
                    {
                        tracing::debug!(error=%e,"CAPTCHA cleanup failed");
                    }
                }
            }
            Err(e) => {
                self.store.manual(c, &e.to_string())?;
                tracing::error!(chat=c.chat_id,user=c.user_id,error=%e,"manual action required");
            }
        }
        Ok(())
    }

    async fn moderate_links(&self, m: &Message, sender: &User) -> Result<bool> {
        if !self.store.settings(m.chat.id)?.links
            || !m
                .entities
                .iter()
                .chain(m.caption_entities.iter())
                .any(|entity| matches!(entity.kind.as_str(), "url" | "text_link"))
            || self.admin(m.chat.id, sender.id).await?
        {
            return Ok(false);
        }
        self.store
            .audit(m.chat.id, sender.id, "delete_attempt", "link")?;
        match self
            .tg
            .call(
                "deleteMessage",
                json!({
                    "chat_id":m.chat.id,"message_id":m.message_id
                }),
            )
            .await
        {
            Ok(_) => self.store.audit(m.chat.id, sender.id, "deleted", "link")?,
            Err(error) => {
                self.store.audit(
                    m.chat.id,
                    sender.id,
                    "manual",
                    "link deletion outcome unknown",
                )?;
                tracing::error!(chat=m.chat.id,message=m.message_id,error=%error,
                    "manual verification of link deletion required");
            }
        }
        // A URL-bearing command must not mutate settings after moderation.
        Ok(true)
    }

    async fn message(&self, m: Message) -> Result<()> {
        self.message_impl(m, false).await
    }

    async fn message_impl(&self, m: Message, edited: bool) -> Result<()> {
        if stale_event(m.edit_date.unwrap_or(m.date), now()) {
            tracing::debug!(chat = m.chat.id, "Ignoring delayed message");
            return Ok(());
        }
        if !edited {
            for user in &m.new_chat_members {
                self.challenge(m.chat.id, user, "member", m.chat.id).await?;
            }
        }
        let Some(text) = m.text.as_deref().or(m.caption.as_deref()) else {
            return Ok(());
        };
        let Some(sender) = m.sender.as_ref() else {
            return Ok(());
        };
        if self.moderate_links(&m, sender).await? {
            return Ok(());
        }
        if !edited && !sender.is_bot && !text.trim().is_empty() && !text.starts_with('/') {
            self.store
                .record_first_message(m.chat.id, sender.id, m.message_id, m.date)?;
        }
        if !edited && text.starts_with('/') {
            let words: Vec<&str> = text.split_whitespace().collect();
            let command = match words[0].split_once('@') {
                Some((cmd, recipient)) => {
                    if recipient.is_empty()
                        || !self
                            .username
                            .as_deref()
                            .is_some_and(|name| name.eq_ignore_ascii_case(recipient))
                    {
                        return Ok(());
                    }
                    cmd
                }
                None => words[0],
            };
            if command == "/help" {
                self.tg.send(m.chat.id,"/help /status /guard on|off /links on|off /lockdown on|off /timeout 30..300 /pending /allow ID /deny ID /stats").await?;
                return Ok(());
            }
            if !self.admin(m.chat.id, sender.id).await? {
                return Ok(());
            }
            if command == "/status" {
                let s = self.store.settings(m.chat.id)?;
                self.tg
                    .send(
                        m.chat.id,
                        &format!(
                    "Protection: {} | timeout: {}s | attempts: {} | links: {} | lockdown: {}",
                    s.enabled,s.timeout,s.max_attempts,s.links,s.lockdown
                ),
                    )
                    .await?;
                return Ok(());
            }
            if command == "/pending" {
                let candidates = self.store.list_open(m.chat.id, 12)?;
                let summary = if candidates.is_empty() {
                    "Ожидающих проверок нет".to_string()
                } else {
                    format!(
                        "Незавершённые проверки:\n{}",
                        candidates
                            .iter()
                            .map(|c| format!("{}: {} ({})", c.user_id, c.status, c.flow))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                self.tg.send(m.chat.id, &summary).await?;
                return Ok(());
            }
            if matches!(command, "/allow" | "/deny") {
                let Some(target) = words
                    .get(1)
                    .and_then(|id| id.parse::<i64>().ok())
                    .filter(|id| *id > 0)
                else {
                    self.tg
                        .send(m.chat.id, "Формат: /allow ID или /deny ID")
                        .await?;
                    return Ok(());
                };
                let Some(pending) = self.store.get(m.chat.id, target)? else {
                    self.tg
                        .send(m.chat.id, "Ожидающей проверки для этого ID нет")
                        .await?;
                    return Ok(());
                };
                let action = match (command, pending.flow.as_str()) {
                    ("/allow", _) => "pass",
                    ("/deny", "request") => "reject",
                    ("/deny", "member") => "kick",
                    _ => anyhow::bail!("unknown flow"),
                };
                if self.store.manual_override(&pending, action)? {
                    self.store.audit(
                        m.chat.id,
                        target,
                        "admin_override",
                        &format!("{} by {}", command, sender.id),
                    )?;
                    self.perform(&pending, action).await?;
                }
                let state = self
                    .store
                    .get(m.chat.id, target)?
                    .context("manual action lost")?;
                self.tg
                    .send(
                        m.chat.id,
                        &format!(
                            "Статус {}: {}. Если manual или acting — проверьте вручную.",
                            target, state.status
                        ),
                    )
                    .await?;
                return Ok(());
            }
            if command == "/stats" {
                let counts = self.store.event_counts(m.chat.id)?;
                let summary = if counts.is_empty() {
                    "Событий пока нет".to_string()
                } else {
                    format!(
                        "События группы:\n{}",
                        counts
                            .iter()
                            .map(|(event, n)| format!("{event}: {n}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                self.tg.send(m.chat.id, &summary).await?;
                return Ok(());
            }
            let update = match (command, words.get(1).copied()) {
                ("/guard", Some("on")) => Some(("enabled", 1)),
                ("/guard", Some("off")) => Some(("enabled", 0)),
                ("/links", Some("on")) => Some(("links", 1)),
                ("/links", Some("off")) => Some(("links", 0)),
                ("/lockdown", Some("on")) => Some(("lockdown", 1)),
                ("/lockdown", Some("off")) => Some(("lockdown", 0)),
                ("/timeout", Some(v)) => v
                    .parse::<i64>()
                    .ok()
                    .filter(|n| (30..=300).contains(n))
                    .map(|n| ("timeout_s", n)),
                _ => None,
            };
            if let Some((key, value)) = update {
                self.store.setting(m.chat.id, key, value)?;
                self.store.audit(m.chat.id, sender.id, "setting", key)?;
                self.tg.send(m.chat.id, "Настройка сохранена").await?;
            }
            return Ok(());
        }
        Ok(())
    }

    async fn handle(&self, u: Update) -> Result<()> {
        tracing::trace!(update_id = u.update_id, "handling Telegram update");
        if let Some(request) = u.chat_join_request {
            return self.join(request).await;
        }
        if let Some(change) = u.chat_member {
            return self.member(change).await;
        }
        if let Some(c) = u.callback_query {
            return self.callback(c).await;
        }
        if let Some(m) = u.edited_message {
            return self.message_impl(m, true).await;
        }
        if let Some(m) = u.message {
            return self.message(m).await;
        }
        Ok(())
    }

    async fn expire(&self, c: &Challenge) -> Result<()> {
        // Reconcile current Telegram membership before punishing a user for
        // an old CAPTCHA. Moderators may have intervened while the bot was off.
        let membership = match self
            .tg
            .call(
                "getChatMember",
                json!({
                    "chat_id":c.chat_id,"user_id":c.user_id
                }),
            )
            .await
        {
            Ok(m) => m,
            Err(error) => {
                self.store
                    .manual(c, "unable to verify current member state at expiry")?;
                tracing::warn!(chat=c.chat_id,user=c.user_id,error=%error,
                    "expired admission requires manual verification");
                return Ok(());
            }
        };
        let status = membership["status"].as_str();
        let active = matches!(status, Some("member" | "administrator" | "creator"))
            || (status == Some("restricted") && membership["is_member"] == true);
        let privileged = matches!(status, Some("administrator" | "creator"));
        let gone = matches!(status, Some("left" | "kicked"));
        if c.flow == "request" && active {
            self.store.resolve_external_approval(c)?;
            return Ok(());
        }
        if c.flow == "member" && privileged {
            self.store
                .close_without_action(c, "passed", "member promoted externally")?;
            return Ok(());
        }
        if c.flow == "member" && gone {
            self.store
                .close_without_action(c, "failed", "member already left")?;
            return Ok(());
        }
        if c.flow == "request" && status == Some("kicked") {
            self.store
                .close_without_action(c, "failed", "join request already blocked")?;
            return Ok(());
        }
        if (c.flow == "request" && status != Some("left")) || (c.flow == "member" && !active) {
            self.store
                .manual(c, "unexpected Telegram membership status at expiry")?;
            return Ok(());
        }
        let action = if c.flow == "member" { "kick" } else { "reject" };
        if self.store.claim(c, action)? {
            self.perform(c, action).await?;
        }
        Ok(())
    }

    async fn expire_first_message(&self, c: &Challenge) -> Result<()> {
        let member = match self
            .tg
            .call(
                "getChatMember",
                json!({"chat_id":c.chat_id,"user_id":c.user_id}),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                self.store
                    .manual(c, "first message membership check failed")?;
                tracing::warn!(error=%error, "first-message timeout requires manual review");
                return Ok(());
            }
        };
        let status = member["status"].as_str();
        if matches!(status, Some("administrator" | "creator")) {
            self.store
                .close_first_message_window(c, "passed", "promoted to administrator")?;
        } else if matches!(status, Some("left" | "kicked")) {
            self.store
                .close_first_message_window(c, "failed", "already left group")?;
        } else if status == Some("member")
            || (status == Some("restricted")
                && member["is_member"] == true
                && member["can_send_messages"] == true)
        {
            if self.store.claim_first_message_timeout(c)? {
                self.perform(c, "kick").await?;
            }
        } else {
            // A user still unable to send messages must not be kicked for
            // failing the post-CAPTCHA write requirement.
            self.store
                .manual(c, "not confirmed as writable group member")?;
        }
        Ok(())
    }

    async fn sweep(&self) -> Result<()> {
        for c in self.store.open_challenges()? {
            if c.status == "acting" {
                self.store
                    .manual(&c, "uncertain previous API action, verify manually")?;
            } else if c.status == "awaiting_message" && c.expires_at <= now() {
                self.expire_first_message(&c).await?;
            } else if c.status == "pending" && c.expires_at <= now() {
                self.expire(&c).await?;
            } else if c.status == "pending" && c.message_id.is_none() {
                self.store
                    .manual(&c, "delivery not confirmed before restart")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_initial_restriction_denies_media() {
        let permissions = no_send_permissions();
        for field in [
            "can_send_messages",
            "can_send_photos",
            "can_send_videos",
            "can_send_audios",
            "can_send_voice_notes",
            "can_send_polls",
            "can_send_other_messages",
            "can_react_to_messages",
            "can_edit_tag",
            "can_invite_users",
        ] {
            assert_eq!(permissions[field], false, "{field} must be disabled");
        }
    }

    #[tokio::test]
    async fn one_wrong_answer_kicks_and_replay_does_not_kick_again() -> Result<()> {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        let success = ResponseTemplate::new(200).set_body_json(json!({"ok":true,"result":true}));
        Mock::given(method("POST"))
            .and(path("/botTEST/banChatMember"))
            .respond_with(success.clone())
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/answerCallbackQuery"))
            .respond_with(success)
            .expect(2)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        let c = Challenge {
            chat_id: -401,
            user_id: 43,
            nonce: "one-answer".into(),
            flow: "member".into(),
            status: "pending".into(),
            delivery_chat_id: -401,
            answer: 6,
            options: vec![6, 9],
            attempts: 0,
            expires_at: now() + 120,
            message_id: None,
            action: None,
            updated_at: now(),
        };
        bot.store.create(&c)?;
        let callback = Callback {
            id: "one-click".into(),
            user: User {
                id: c.user_id,
                is_bot: false,
            },
            data: Some(format!("q:{}:{}:{}:9", c.chat_id, c.user_id, c.nonce)),
        };
        bot.callback(callback.clone()).await?;
        assert_eq!(
            bot.store.get(c.chat_id, c.user_id)?.unwrap().status,
            "failed"
        );
        bot.callback(callback).await?;
        assert_eq!(
            bot.store.get(c.chat_id, c.user_id)?.unwrap().status,
            "failed"
        );
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn first_real_message_completes_admission() -> Result<()> {
        use crate::model::Chat;
        let store = Store::open(":memory:")?;
        let c = Challenge {
            chat_id: -701,
            user_id: 72,
            nonce: "write-once".into(),
            flow: "member".into(),
            status: "pending".into(),
            delivery_chat_id: -701,
            answer: 4,
            options: vec![4, 8],
            attempts: 0,
            expires_at: now() + 120,
            message_id: None,
            action: None,
            updated_at: now(),
        };
        store.create(&c)?;
        assert!(store.claim(&c, "pass")?);
        store.start_first_message_window(&c)?;
        let bot = Bot::new(store, Telegram::mock("http://127.0.0.1:1/botTEST/".into()));
        let msg = Message {
            message_id: 80,
            chat: Chat { id: c.chat_id },
            date: now(),
            sender: Some(User {
                id: c.user_id,
                is_bot: false,
            }),
            text: Some("Привет всем!".into()),
            caption: None,
            edit_date: None,
            new_chat_members: Vec::new(),
            entities: Vec::new(),
            caption_entities: Vec::new(),
        };
        bot.message(msg).await?;
        assert_eq!(
            bot.store.get(c.chat_id, c.user_id)?.unwrap().status,
            "passed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn silent_user_after_request_approval_gets_one_kick() -> Result<()> {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"member"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/banChatMember"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok":true,"result":true})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let store = Store::open(":memory:")?;
        let c = Challenge {
            chat_id: -800,
            user_id: 81,
            nonce: "silent".into(),
            flow: "request".into(),
            status: "awaiting_message".into(),
            delivery_chat_id: 81,
            answer: 9,
            options: vec![9],
            attempts: 0,
            expires_at: now() - 1,
            message_id: None,
            action: None,
            updated_at: now() - 601,
        };
        store.create(&c)?;
        let bot = Bot::new(store, Telegram::mock(format!("{}/botTEST/", server.uri())));
        bot.sweep().await?;
        bot.sweep().await?;
        assert_eq!(
            bot.store.get(c.chat_id, c.user_id)?.unwrap().status,
            "failed"
        );
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn external_join_approval_closes_pending_request_without_second_captcha() -> Result<()> {
        use crate::model::{Chat, Member};
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/sendMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"message_id":301}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/deleteMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok":true,"result":true})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"member"}})),
            )
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/restrictChatMember"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        let member = User {
            id: 55,
            is_bot: false,
        };
        bot.join(JoinRequest {
            chat: Chat { id: -600 },
            user: member.clone(),
            user_chat_id: Some(55),
            query_id: None,
            date: now(),
        })
        .await?;
        assert_eq!(bot.store.get(-600, 55)?.unwrap().status, "pending");

        bot.member(MemberChange {
            chat: Chat { id: -600 },
            date: now(),
            old_chat_member: Member {
                user: member.clone(),
                status: "left".into(),
                is_member: None,
            },
            new_chat_member: Member {
                user: member,
                status: "member".into(),
                is_member: None,
            },
        })
        .await?;
        let state = bot
            .store
            .get(-600, 55)?
            .context("missing approved request")?;
        assert_eq!(state.status, "passed");
        assert_eq!(state.flow, "request");
        assert_eq!(bot.store.list_open(-600, 10)?.len(), 0);
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn failed_link_deletion_records_manual_without_redelivery() -> Result<()> {
        use crate::model::{Chat, MessageEntity};
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"member"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/deleteMessage"))
            .respond_with(ResponseTemplate::new(502).set_body_json(
                json!({"ok":false,"error_code":502,"description":"upstream unavailable"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        bot.store.setting(-100, "links", 1)?;
        bot.message(Message {
            message_id: 222,
            chat: Chat { id: -100 },
            date: now(),
            sender: Some(User {
                id: 17,
                is_bot: false,
            }),
            text: Some("http://example.org".into()),
            caption: None,
            edit_date: None,
            new_chat_members: Vec::new(),
            entities: vec![MessageEntity { kind: "url".into() }],
            caption_entities: Vec::new(),
        })
        .await?;
        let events = bot.store.event_counts(-100)?;
        assert!(events.contains(&("manual".to_string(), 1)));
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn edited_caption_link_is_checked_at_edit_time() -> Result<()> {
        use crate::model::{Chat, MessageEntity};
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"member"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/deleteMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok":true,"result":true})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        bot.store.setting(-100, "links", 1)?;
        let message = Message {
            message_id: 250,
            chat: Chat { id: -100 },
            date: now() - 900,
            edit_date: Some(now()),
            sender: Some(User {
                id: 19,
                is_bot: false,
            }),
            text: None,
            caption: Some("see description".into()),
            new_chat_members: Vec::new(),
            entities: Vec::new(),
            caption_entities: vec![MessageEntity {
                kind: "text_link".into(),
            }],
        };
        let update = Update {
            update_id: 999,
            message: None,
            edited_message: Some(message),
            callback_query: None,
            chat_member: None,
            chat_join_request: None,
        };
        bot.handle(update).await?;
        assert!(bot
            .store
            .event_counts(-100)?
            .contains(&("deleted".into(), 1)));
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn telegram_member_lookup_failure_is_visible_in_pending_queue() -> Result<()> {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":false,"error_code":403,"description":"Forbidden"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/restrictChatMember"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/sendMessage"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        bot.challenge(
            -9,
            &User {
                id: 901,
                is_bot: false,
            },
            "member",
            -9,
        )
        .await?;
        let pending = bot.store.list_open(-9, 10)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, "manual");
        assert_eq!(pending[0].user_id, 901);
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn join_request_without_temporary_dm_stays_in_manual_queue() -> Result<()> {
        use crate::model::Chat;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock("http://127.0.0.1:9/botTEST/".into()),
        );
        bot.join(JoinRequest {
            chat: Chat { id: -10 },
            user: User {
                id: 777,
                is_bot: false,
            },
            user_chat_id: None,
            query_id: None,
            date: now(),
        })
        .await?;
        let pending = bot.store.list_open(-10, 10)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].flow, "request");
        assert_eq!(pending[0].status, "manual");
        Ok(())
    }

    #[test]
    fn malformed_update_is_deadlettered_without_stopping_next_event() -> Result<()> {
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock("http://127.0.0.1:9/botTEST/".into()),
        );
        let invalid = json!({
            "update_id":501,
            "message":{"message_id":1,"chat":{"id":"not a number"}}
        });
        assert!(bot.decode_update(invalid)?.is_none());
        assert_eq!(bot.store.offset()?, 502);
        let next = bot
            .decode_update(json!({
                "update_id":502,
                "message":{
                    "message_id":99,"date":now(),"chat":{"id":-4},
                    "from":{"id":6,"is_bot":false},"text":"hello"
                }
            }))?
            .context("valid event lost")?;
        assert_eq!(next.update_id, 502);
        assert_eq!(bot.store.offset()?, 502);
        Ok(())
    }

    #[tokio::test]
    async fn expired_request_already_approved_is_not_declined() -> Result<()> {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"member"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/declineChatJoinRequest"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        let pending = Challenge {
            chat_id: -100,
            user_id: 56,
            nonce: "from-downtime".into(),
            flow: "request".into(),
            status: "pending".into(),
            delivery_chat_id: 56,
            answer: 12,
            options: vec![12],
            attempts: 0,
            expires_at: now() - 30,
            message_id: Some(5),
            action: None,
            updated_at: now() - 90,
        };
        bot.store.create(&pending)?;
        bot.sweep().await?;
        assert_eq!(bot.store.get(-100, 56)?.unwrap().status, "passed");
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn expired_member_promoted_to_admin_is_not_banned() -> Result<()> {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"administrator"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/banChatMember"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        let pending = Challenge {
            chat_id: -100,
            user_id: 57,
            nonce: "promoted".into(),
            flow: "member".into(),
            status: "pending".into(),
            delivery_chat_id: -100,
            answer: 12,
            options: vec![12],
            attempts: 0,
            expires_at: now() - 30,
            message_id: Some(6),
            action: None,
            updated_at: now() - 90,
        };
        bot.store.create(&pending)?;
        bot.sweep().await?;
        assert_eq!(bot.store.get(-100, 57)?.unwrap().status, "passed");
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn expired_unapproved_join_request_is_declined_once() -> Result<()> {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"left"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/declineChatJoinRequest"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok":true,"result":true})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        let c = Challenge {
            chat_id: -101,
            user_id: 70,
            nonce: "still-pending".into(),
            flow: "request".into(),
            status: "pending".into(),
            delivery_chat_id: 70,
            answer: 9,
            options: vec![9],
            attempts: 0,
            expires_at: now() - 10,
            message_id: None,
            action: None,
            updated_at: now() - 80,
        };
        bot.store.create(&c)?;
        bot.sweep().await?;
        bot.sweep().await?;
        assert_eq!(bot.store.get(-101, 70)?.unwrap().status, "failed");
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn timeout_membership_lookup_failure_never_bans_user() -> Result<()> {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":false,"error_code":403,"description":"Forbidden"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/banChatMember"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        let c = Challenge {
            chat_id: -101,
            user_id: 71,
            nonce: "unknown-membership".into(),
            flow: "member".into(),
            status: "pending".into(),
            delivery_chat_id: -101,
            answer: 8,
            options: vec![8],
            attempts: 0,
            expires_at: now() - 10,
            message_id: Some(11),
            action: None,
            updated_at: now() - 80,
        };
        bot.store.create(&c)?;
        bot.sweep().await?;
        assert_eq!(bot.store.get(-101, 71)?.unwrap().status, "manual");
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn regular_member_cannot_approve_someone_elses_request() -> Result<()> {
        use crate::model::Chat;
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"member"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/approveChatJoinRequest"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/sendMessage"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        let target = Challenge {
            chat_id: -100,
            user_id: 77,
            nonce: "moderation".into(),
            flow: "request".into(),
            status: "pending".into(),
            delivery_chat_id: 77,
            answer: 12,
            options: vec![12],
            attempts: 0,
            expires_at: now() + 90,
            message_id: None,
            action: None,
            updated_at: now(),
        };
        bot.store.create(&target)?;
        bot.message(Message {
            message_id: 100,
            chat: Chat { id: -100 },
            date: now(),
            sender: Some(User {
                id: 20,
                is_bot: false,
            }),
            text: Some("/allow 77".into()),
            caption: None,
            edit_date: None,
            new_chat_members: Vec::new(),
            entities: Vec::new(),
            caption_entities: Vec::new(),
        })
        .await?;
        assert_eq!(bot.store.get(-100, 77)?.unwrap().status, "pending");
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn disabling_guard_prevents_new_member_restrictions() -> Result<()> {
        use crate::model::Chat;
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"administrator"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/sendMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"message_id":8}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/restrictChatMember"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        bot.message(Message {
            message_id: 101,
            chat: Chat { id: -100 },
            date: now(),
            sender: Some(User {
                id: 3,
                is_bot: false,
            }),
            text: Some("/guard off".into()),
            caption: None,
            edit_date: None,
            new_chat_members: Vec::new(),
            entities: Vec::new(),
            caption_entities: Vec::new(),
        })
        .await?;
        assert!(!bot.store.settings(-100)?.enabled);
        bot.challenge(
            -100,
            &User {
                id: 71,
                is_bot: false,
            },
            "member",
            -100,
        )
        .await?;
        assert!(bot.store.get(-100, 71)?.is_none());
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn blocked_join_request_dm_is_held_for_manual_review() -> Result<()> {
        use crate::model::Chat;
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"ok":false,"error_code":403,"description":"bot blocked by user"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/declineChatJoinRequest"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        bot.join(JoinRequest {
            chat: Chat { id: -100 },
            user: User {
                id: 91,
                is_bot: false,
            },
            user_chat_id: Some(91),
            query_id: None,
            date: now(),
        })
        .await?;
        let saved = bot
            .store
            .get(-100, 91)?
            .context("missing rejected DM state")?;
        assert_eq!(saved.status, "manual");
        assert_eq!(saved.flow, "request");
        assert!(saved.message_id.is_none());
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn edited_admin_commands_are_not_executed() -> Result<()> {
        use crate::model::Chat;
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/sendMessage"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        let edited = Message {
            message_id: 501,
            chat: Chat { id: -77 },
            date: now() - 90,
            edit_date: Some(now()),
            sender: Some(User {
                id: 7,
                is_bot: false,
            }),
            text: Some("/guard off".into()),
            caption: None,
            new_chat_members: Vec::new(),
            entities: Vec::new(),
            caption_entities: Vec::new(),
        };
        bot.handle(Update {
            update_id: 3,
            message: None,
            edited_message: Some(edited),
            callback_query: None,
            chat_member: None,
            chat_join_request: None,
        })
        .await?;
        assert!(bot.store.settings(-77)?.enabled);
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn commands_addressed_to_other_bots_are_ignored() -> Result<()> {
        use crate::model::Chat;
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"administrator"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/sendMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"message_id":15}})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        bot.username = Some("RebuildExampleBot".into());
        for text in ["/guard@AnotherBot off", "/guard@ off"] {
            bot.message(Message {
                message_id: 1,
                chat: Chat { id: -50 },
                date: now(),
                sender: Some(User {
                    id: 7,
                    is_bot: false,
                }),
                text: Some(text.into()),
                caption: None,
                edit_date: None,
                new_chat_members: Vec::new(),
                entities: Vec::new(),
                caption_entities: Vec::new(),
            })
            .await?;
            assert!(bot.store.settings(-50)?.enabled);
        }
        bot.message(Message {
            message_id: 2,
            chat: Chat { id: -50 },
            date: now(),
            sender: Some(User {
                id: 7,
                is_bot: false,
            }),
            text: Some("/guard@REBUILDEXAMPLEBOT off".into()),
            caption: None,
            edit_date: None,
            new_chat_members: Vec::new(),
            entities: Vec::new(),
            caption_entities: Vec::new(),
        })
        .await?;
        assert!(!bot.store.settings(-50)?.enabled);
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn url_in_unknown_or_other_bot_command_is_still_filtered() -> Result<()> {
        use crate::model::{Chat, MessageEntity};
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"member"}})),
            )
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/deleteMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok":true,"result":true})),
            )
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/sendMessage"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        bot.store.setting(-78, "links", 1)?;
        for (id, text) in [
            (71, "/unknown https://example.org"),
            (72, "/guard@OtherBot https://example.org"),
        ] {
            bot.message(Message {
                message_id: id,
                chat: Chat { id: -78 },
                date: now(),
                sender: Some(User {
                    id: 8,
                    is_bot: false,
                }),
                text: Some(text.into()),
                caption: None,
                edit_date: None,
                new_chat_members: Vec::new(),
                entities: vec![MessageEntity { kind: "url".into() }],
                caption_entities: Vec::new(),
            })
            .await?;
        }
        assert_eq!(
            bot.store
                .event_counts(-78)?
                .iter()
                .find(|(e, _)| e == "deleted")
                .map(|(_, n)| *n),
            Some(2),
        );
        server.verify().await;
        Ok(())
    }

    #[test]
    fn restores_only_existing_group_permissions() -> Result<()> {
        let source = json!({"permissions":{
            "can_send_messages":true,
            "can_send_photos":false,
            "can_invite_users":false
        }});
        let perms = default_chat_permissions(&source)?;
        assert_eq!(perms["can_send_messages"], true);
        assert_eq!(perms["can_send_photos"], false);
        assert_eq!(perms["can_invite_users"], false);
        assert!(perms.get("can_send_polls").is_none());
        assert!(default_chat_permissions(&json!({})).is_err());
        assert!(
            default_chat_permissions(&json!({"permissions":{"can_send_messages":"true"}})).is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn refuses_polling_if_webhook_exists() -> Result<()> {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getMe"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok":true,"result":{"id":123}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getWebhookInfo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"url":"https://example.org/hook"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/deleteWebhook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getUpdates"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let bot = Bot::new(
            Store::open(":memory:")?,
            Telegram::mock(format!("{}/botTEST/", server.uri())),
        );
        let error = bot
            .run()
            .await
            .expect_err("polling must refuse configured webhook");
        assert!(error.to_string().contains("webhook"));
        server.verify().await;
        Ok(())
    }

    #[test]
    fn request_and_member_statuses() {
        let c = Challenge {
            chat_id: -1,
            user_id: 3,
            nonce: "abc".into(),
            flow: "request".into(),
            status: "pending".into(),
            delivery_chat_id: 3,
            answer: 10,
            options: vec![10],
            attempts: 0,
            expires_at: now() + 40,
            message_id: None,
            action: None,
            updated_at: now(),
        };
        assert_eq!(if c.flow == "member" { "kick" } else { "reject" }, "reject");
    }

    #[test]
    fn stale_updates_and_request_approval_grace() {
        assert!(stale_event(100, 701));
        assert!(!stale_event(100, 700));
        let t = 5_000_i64;
        let mut c = Challenge {
            chat_id: -1,
            user_id: 42,
            nonce: "n".into(),
            flow: "request".into(),
            status: "passed".into(),
            delivery_chat_id: 42,
            answer: 5,
            options: vec![5],
            attempts: 0,
            expires_at: t,
            message_id: None,
            action: None,
            updated_at: t,
        };
        assert!(skip_existing(&c, "member", t + 299));
        assert!(!skip_existing(&c, "member", t + 301));
        c.flow = "member".into();
        assert!(skip_existing(&c, "member", t + 11));
        assert!(!skip_existing(&c, "member", t + 13));
        c.status = "manual".into();
        assert!(skip_existing(&c, "member", t + 1000));
    }
    #[tokio::test]
    async fn live_http_mock_restrict_challenge_then_unrestrict() -> Result<()> {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        let success = ResponseTemplate::new(200).set_body_json(json!({"ok":true,"result":true}));
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"member"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"ok":true,"result":{"permissions":{
                    "can_send_messages":true,"can_send_photos":false,
                    "can_send_polls":true,"can_invite_users":false
                }}}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/restrictChatMember"))
            .respond_with(success.clone())
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/sendMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"message_id":501}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/answerCallbackQuery"))
            .respond_with(success.clone())
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/deleteMessage"))
            .respond_with(success)
            .expect(1)
            .mount(&server)
            .await;

        let tg = Telegram::mock(format!("{}/botTEST/", server.uri()));
        let bot = Bot::new(Store::open(":memory:")?, tg);
        let member = User {
            id: 42,
            is_bot: false,
        };
        bot.challenge(-100, &member, "member", -100).await?;
        let c = bot.store.get(-100, 42)?.context("CAPTCHA not stored")?;
        assert_eq!(c.status, "pending");
        assert_eq!(c.options.len(), 9);
        assert_eq!(c.message_id, Some(501));

        let data = format!("q:{}:{}:{}:{}", c.chat_id, c.user_id, c.nonce, c.answer);
        bot.callback(Callback {
            id: "other-user".into(),
            user: User {
                id: 101,
                is_bot: false,
            },
            data: Some(data.clone()),
        })
        .await?;
        assert_eq!(bot.store.get(-100, 42)?.unwrap().status, "pending");

        bot.callback(Callback {
            id: "correct-user".into(),
            user: member,
            data: Some(data),
        })
        .await?;
        assert_eq!(bot.store.get(-100, 42)?.unwrap().status, "awaiting_message");
        Ok(())
    }

    #[tokio::test]
    async fn telegram_forbidden_records_manual_without_fake_challenge() -> Result<()> {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTEST/getChatMember"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"status":"member"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/restrictChatMember"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"ok":false,"error_code":403,"description":"Not enough permissions"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/botTEST/sendMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"result":{"message_id":88}})),
            )
            .expect(0)
            .mount(&server)
            .await;

        let tg = Telegram::mock(format!("{}/botTEST/", server.uri()));
        let bot = Bot::new(Store::open(":memory:")?, tg);
        bot.challenge(
            -100,
            &User {
                id: 43,
                is_bot: false,
            },
            "member",
            -100,
        )
        .await?;
        let c = bot.store.get(-100, 43)?.context("manual state missing")?;
        assert_eq!(c.status, "manual");
        assert!(c.message_id.is_none());
        assert_eq!(bot.store.list_open(-100, 10)?.len(), 1);
        Ok(())
    }
}
