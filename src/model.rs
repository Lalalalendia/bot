//! Minimal typed subset of Telegram Bot API updates.
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    #[serde(default)]
    pub date: i64,
    #[serde(rename = "from")]
    pub sender: Option<User>,
    pub text: Option<String>,
    #[serde(default)]
    pub new_chat_members: Vec<User>,
    #[serde(default)]
    pub entities: Vec<MessageEntity>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MessageEntity {
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Member {
    pub user: User,
    pub status: String,
    pub is_member: Option<bool>,
}

impl Member {
    pub fn in_chat(&self) -> bool {
        match self.status.as_str() {
            "member" | "administrator" | "creator" => true,
            "restricted" => self.is_member.unwrap_or(false),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct MemberChange {
    pub chat: Chat,
    pub date: i64,
    pub old_chat_member: Member,
    pub new_chat_member: Member,
}

#[derive(Clone, Debug, Deserialize)]
pub struct JoinRequest {
    pub chat: Chat,
    #[serde(rename = "from")]
    pub user: User,
    pub user_chat_id: Option<i64>,
    pub query_id: Option<String>,
    pub date: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Callback {
    pub id: String,
    #[serde(rename = "from")]
    pub user: User,
    pub data: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<Callback>,
    pub chat_member: Option<MemberChange>,
    pub chat_join_request: Option<JoinRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_user_requires_is_member() {
        let user = User { id: 9, is_bot: false };
        let mut member = Member { user, status: "restricted".into(), is_member: Some(false) };
        assert!(!member.in_chat());
        member.is_member = Some(true);
        assert!(member.in_chat());
    }
}
