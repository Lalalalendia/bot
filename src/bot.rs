use crate::{
    model::{Callback, JoinRequest, MemberChange, Message, Update, User},
    store::{now, Challenge, Store},
    telegram::Telegram,
};
use anyhow::{Context, Result};
use rand::{seq::SliceRandom, Rng};
use serde_json::{json, Value};
use std::{collections::HashMap, time::Duration};

pub struct Bot {
    store: Store,
    tg: Telegram,
    id: i64,
}

impl Bot {
    pub fn new(store: Store, tg: Telegram) -> Self { Self { store,tg,id:0 } }

    pub async fn run(mut self) -> Result<()> {
        let me=self.tg.call("getMe",json!({})).await.context("Bot token validation")?;
        self.id=me["id"].as_i64().context("getMe.id missing")?;
        tracing::info!(bot_id=self.id,"polling initialized");
        // Polling cannot run alongside a webhook without deleting it. We only delete
        // it at startup; drop_pending_updates=false preserves pending events.
        self.tg.call("deleteWebhook",json!({"drop_pending_updates":false})).await?;
        let mut failures: HashMap<i64,u8>=HashMap::new();
        let mut poll_errors=0u32;
        loop {
            if let Err(e)=self.sweep().await {tracing::error!(error=%e,"maintenance");}
            let offset=self.store.offset()?;
            match self.tg.call("getUpdates",json!({
                "offset":offset,"limit":50,"timeout":25,
                "allowed_updates":["message","callback_query","chat_member","chat_join_request"]
            })).await {
                Ok(raw)=>{
                    poll_errors=0;
                    let updates: Vec<Update>=serde_json::from_value(raw).context("updates JSON")?;
                    for update in updates {
                        if update.update_id<self.store.offset()? {continue;}
                        let id=update.update_id;
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
        }
    }

    async fn admin(&self, chat: i64, user: i64) -> Result<bool> {
        let result=self.tg.call("getChatMember",json!({"chat_id":chat,"user_id":user})).await?;
        Ok(matches!(result["status"].as_str(),Some("administrator"|"creator")))
    }

    async fn challenge(&self, chat: i64, user: &User, flow: &str, delivery: i64) -> Result<()> {
        if user.is_bot || user.id==self.id { return Ok(()); }
        let settings=self.store.settings(chat)?;
        if !settings.enabled || self.admin(chat,user.id).await? {return Ok(());}
        if let Some(previous)=self.store.get(chat,user.id)? {
            if matches!(previous.status.as_str(),"pending"|"acting"|"manual") {
                return Ok(());
            }
        }
        if settings.lockdown {
            if flow=="request" {
                self.tg.call("declineChatJoinRequest",json!({"chat_id":chat,"user_id":user.id})).await?;
            } else {
                self.tg.call("banChatMember",json!({"chat_id":chat,"user_id":user.id,
                    "until_date":now()+60})).await?;
            }
            self.store.audit(chat,user.id,"lockdown",flow)?;
            return Ok(());
        }
        // Restrict first. If Telegram refuses, never leave a published CAPTCHA
        // that gives the appearance of protection while allowing unrestricted chat.
        if flow=="member" {
            self.tg.call("restrictChatMember",json!({
                "chat_id":chat,"user_id":user.id,
                "permissions":{"can_send_messages":false},
                "use_independent_chat_permissions":true
            })).await?;
        }
        let a=rand::thread_rng().gen_range(1..=9);
        let b=rand::thread_rng().gen_range(1..=9);
        let answer=a+b;
        let mut options: Vec<i64>=(2..=18).collect();
        options.shuffle(&mut rand::thread_rng());
        options.truncate(8);
        if !options.contains(&answer) { options.pop(); options.push(answer); }
        options.sort_unstable();
        let nonce=format!("{:016x}",rand::random::<u64>());
        let c=Challenge {
            chat_id:chat,user_id:user.id,nonce,flow:flow.into(),
            status:"pending".into(),delivery_chat_id:delivery,answer,options,
            attempts:0,expires_at:now()+settings.timeout,message_id:None,action:None
        };
        self.store.create(&c)?;
        self.deliver(&c,a,b).await?;
        Ok(())
    }

    async fn deliver(&self, c:&Challenge, a:i64,b:i64)->Result<()> {
        let keyboard:Vec<Vec<Value>>=c.options.iter().map(|value|vec![json!({
            "text":value.to_string(),
            "callback_data":format!("q:{}:{}:{}:{}",c.chat_id,c.user_id,c.nonce,value)
        })]).collect();
        let msg=self.tg.call("sendMessage",json!({
            "chat_id":c.delivery_chat_id,
            "text":format!("Проверка для {}. Сколько будет {} + {}?",c.user_id,a,b),
            "reply_markup":{"inline_keyboard":keyboard}
        })).await;
        match msg {
            Ok(msg)=>{
                self.store.mark_sent(c,msg["message_id"].as_i64().context("missing CAPTCHA id")?)?;
            }
            Err(e)=>{
                self.store.manual(c,&format!("CAPTCHA delivery failed: {e}"))?;
                tracing::warn!(chat=c.chat_id,user=c.user_id,error=%e,"manual admission required");
            }
        }
        Ok(())
    }

    async fn join(&self, request:JoinRequest)->Result<()> {
        if request.query_id.is_some() {
            tracing::warn!(chat=request.chat.id,"Guardian join requests are not implemented; keeping pending");
            return Ok(());
        }
        let Some(delivery)=request.user_chat_id else {
            self.store.audit(request.chat.id,request.user.id,"manual","missing user_chat_id")?;
            return Ok(());
        };
        self.challenge(request.chat.id,&request.user,"request",delivery).await
    }

    async fn member(&self,change:MemberChange)->Result<()> {
        if !change.old_chat_member.in_chat() && change.new_chat_member.in_chat() {
            self.challenge(change.chat.id,&change.new_chat_member.user,"member",change.chat.id).await?;
        }
        Ok(())
    }

    async fn callback(&self,c:Callback)->Result<()> {
        let Some(data)=c.data.as_deref() else {return Ok(());};
        let fields:Vec<&str>=data.split(':').collect();
        if fields.len()!=5 || fields[0]!="q" {return Ok(());}
        let Some(chat)=fields[1].parse::<i64>().ok() else {return Ok(());};
        let Some(user)=fields[2].parse::<i64>().ok() else {return Ok(());};
        let Some(value)=fields[4].parse::<i64>().ok() else {return Ok(());};
        let mut reply="Проверка устарела";
        if user==c.user.id {
            if let Some(state)=self.store.get(chat,user)? {
                if state.nonce==fields[3] && state.status=="pending" && state.expires_at>now() {
                    if value==state.answer {
                        if self.store.claim(&state,"pass")? {
                            self.perform(&state,"pass").await?;
                        }
                        reply="Ответ принят";
                    } else {
                        let limit=self.store.settings(chat)?.max_attempts;
                        let (left,reject)=self.store.wrong(&state,limit)?;
                        if reject && self.store.claim(&state,if state.flow=="member"{"kick"}else{"reject"})? {
                            self.perform(&state,if state.flow=="member"{"kick"}else{"reject"}).await?;
                        }
                        reply=if left==0 {"Попытки закончились"} else {"Неверный ответ"};
                    }
                }
            }
        } else {reply="Это проверка другого участника";}
        self.tg.call("answerCallbackQuery",json!({"callback_query_id":c.id,"text":reply})).await?;
        Ok(())
    }

    async fn perform(&self,c:&Challenge,action:&str)->Result<()> {
        // The intended action is stored as "acting" before calling Telegram.
        // On a crash/uncertain response we do not blindly reissue the action.
        let result=match (c.flow.as_str(),action) {
            ("member","pass")=>self.tg.call("restrictChatMember",json!({
                "chat_id":c.chat_id,"user_id":c.user_id,
                "permissions":{"can_send_messages":true,"can_send_audios":true,
                  "can_send_documents":true,"can_send_photos":true,"can_send_videos":true,
                  "can_send_video_notes":true,"can_send_voice_notes":true,
                  "can_send_polls":true,"can_send_other_messages":true,
                  "can_add_web_page_previews":true},
                "use_independent_chat_permissions":true
            })).await,
            ("member","kick")=>self.tg.call("banChatMember",json!({
                "chat_id":c.chat_id,"user_id":c.user_id,"until_date":now()+60
            })).await,
            ("request","pass")=>self.tg.call("approveChatJoinRequest",json!({
                "chat_id":c.chat_id,"user_id":c.user_id
            })).await,
            ("request","reject")=>self.tg.call("declineChatJoinRequest",json!({
                "chat_id":c.chat_id,"user_id":c.user_id
            })).await,
            _=>anyhow::bail!("invalid external action {action}"),
        };
        match result {
            Ok(_)=>{
                self.store.finish(c,if action=="pass"{"passed"}else{"failed"})?;
                if let Some(message)=c.message_id {
                    if let Err(e)=self.tg.call("deleteMessage",json!({
                        "chat_id":c.delivery_chat_id,"message_id":message
                    })).await {tracing::debug!(error=%e,"CAPTCHA cleanup failed");}
                }
            }
            Err(e)=>{
                self.store.manual(c,&e.to_string())?;
                tracing::error!(chat=c.chat_id,user=c.user_id,error=%e,"manual action required");
            }
        }
        Ok(())
    }

    async fn message(&self,m:Message)->Result<()> {
        for user in &m.new_chat_members {self.challenge(m.chat.id,user,"member",m.chat.id).await?;}
        let Some(text)=m.text.as_deref() else {return Ok(());};
        let Some(sender)=m.sender.as_ref() else {return Ok(());};
        if text.starts_with('/') {
            let words:Vec<&str>=text.split_whitespace().collect();
            let command=words[0].split('@').next().unwrap_or("");
            if command=="/help" {
                self.tg.send(m.chat.id,"/help /status /guard on|off /links on|off /lockdown on|off /timeout 30..300 /attempts 1..5").await?;
                return Ok(());
            }
            if !self.admin(m.chat.id,sender.id).await? {return Ok(());}
            if command=="/status" {
                let s=self.store.settings(m.chat.id)?;
                self.tg.send(m.chat.id,&format!(
                    "Protection: {} | timeout: {}s | attempts: {} | links: {} | lockdown: {}",
                    s.enabled,s.timeout,s.max_attempts,s.links,s.lockdown
                )).await?;
                return Ok(());
            }
            let update=match (command,words.get(1).copied()) {
                ("/guard",Some("on"))=>Some(("enabled",1)),
                ("/guard",Some("off"))=>Some(("enabled",0)),
                ("/links",Some("on"))=>Some(("links",1)),
                ("/links",Some("off"))=>Some(("links",0)),
                ("/lockdown",Some("on"))=>Some(("lockdown",1)),
                ("/lockdown",Some("off"))=>Some(("lockdown",0)),
                ("/timeout",Some(v))=>v.parse::<i64>().ok().filter(|n|(30..=300).contains(n)).map(|n|("timeout_s",n)),
                ("/attempts",Some(v))=>v.parse::<i64>().ok().filter(|n|(1..=5).contains(n)).map(|n|("attempts",n)),
                _=>None
            };
            if let Some((key,value))=update {
                self.store.setting(m.chat.id,key,value)?;
                self.store.audit(m.chat.id,sender.id,"setting",key)?;
                self.tg.send(m.chat.id,"Настройка сохранена").await?;
            }
            return Ok(());
        }
        if self.store.settings(m.chat.id)?.links
            && m.entities.iter().any(|entity|matches!(entity.kind.as_str(),"url"|"text_link"))
            && !self.admin(m.chat.id,sender.id).await? {
            self.tg.call("deleteMessage",json!({"chat_id":m.chat.id,"message_id":m.message_id})).await?;
            self.store.audit(m.chat.id,sender.id,"deleted","link")?;
        }
        Ok(())
    }

    async fn handle(&self,u:Update)->Result<()> {
        if let Some(request)=u.chat_join_request {return self.join(request).await;}
        if let Some(change)=u.chat_member {return self.member(change).await;}
        if let Some(c)=u.callback_query {return self.callback(c).await;}
        if let Some(m)=u.message {return self.message(m).await;}
        Ok(())
    }

    async fn sweep(&self)->Result<()> {
        for c in self.store.open_challenges()? {
            if c.status=="acting" {
                self.store.manual(&c,"uncertain previous API action, verify manually")?;
            } else if c.status=="pending" && c.expires_at<=now() {
                let action=if c.flow=="member"{"kick"}else{"reject"};
                if self.store.claim(&c,action)? {
                    self.perform(&c,action).await?;
                }
            } else if c.status=="pending" && c.message_id.is_none() {
                self.store.manual(&c,"delivery not confirmed before restart")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_member_statuses() {
        let c=Challenge{chat_id:-1,user_id:3,nonce:"abc".into(),flow:"request".into(),
            status:"pending".into(),delivery_chat_id:3,answer:10,options:vec![10],
            attempts:0,expires_at:now()+40,message_id:None,action:None};
        assert_eq!(if c.flow=="member"{"kick"}else{"reject"},"reject");
    }
}
