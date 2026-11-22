// Based on Mattermost server/public/model/post.go, post_metadata.go, post_list.go, thread.go,
// post_acknowledgement.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::BTreeMap;

use ebachat_macros::model;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Emoji, FileIds, FileInfo, Reaction, StringArray, StringInterface, User, is_zero};

#[model(table = "posts")]
#[derive(Debug, Clone, PartialEq)]
pub struct Post {
    pub id: String,
    pub create_at: i64,
    pub update_at: i64,
    pub edit_at: i64,
    pub delete_at: i64,
    pub is_pinned: bool,
    pub user_id: String,
    pub channel_id: String,
    pub root_id: String,
    pub original_id: String,
    pub message: String,
    #[mm(transient, omitempty)]
    pub message_source: String,
    pub r#type: String,
    /// What clients see of it: `StripActionIntegrations`.
    #[serde(serialize_with = "crate::integrations::serialize_props")]
    pub props: StringInterface,
    pub hashtags: String,
    #[mm(skip_json)]
    pub filenames: StringArray,
    pub file_ids: FileIds,
    #[mm(transient)]
    pub pending_post_id: String,
    #[mm(omitempty)]
    pub has_reactions: bool,
    #[mm(omitempty)]
    pub remote_id: Option<String>,
    #[mm(transient)]
    pub reply_count: i64,
    #[mm(transient)]
    pub last_reply_at: i64,
    #[mm(transient)]
    #[serde(serialize_with = "serialize_participants_opt")]
    pub participants: Option<Vec<User>>,
    #[mm(transient, omitempty)]
    pub is_following: Option<bool>,
    #[mm(transient, omitempty)]
    pub metadata: Option<PostMetadata>,
}

pub mod kind {
    pub const SYSTEM_PREFIX: &str = "system_";
    pub const JOIN_LEAVE: &str = "system_join_leave";
    pub const JOIN_CHANNEL: &str = "system_join_channel";
    pub const GUEST_JOIN_CHANNEL: &str = "system_guest_join_channel";
    pub const LEAVE_CHANNEL: &str = "system_leave_channel";
    pub const JOIN_TEAM: &str = "system_join_team";
    pub const LEAVE_TEAM: &str = "system_leave_team";
    pub const AUTO_RESPONDER: &str = "system_auto_responder";
    pub const ADD_REMOVE: &str = "system_add_remove";
    pub const ADD_TO_CHANNEL: &str = "system_add_to_channel";
    pub const ADD_GUEST_TO_CHANNEL: &str = "system_add_guest_to_chan";
    pub const REMOVE_FROM_CHANNEL: &str = "system_remove_from_channel";
    pub const ADD_TO_TEAM: &str = "system_add_to_team";
    pub const REMOVE_FROM_TEAM: &str = "system_remove_from_team";
    pub const HEADER_CHANGE: &str = "system_header_change";
    pub const PURPOSE_CHANGE: &str = "system_purpose_change";
    pub const EPHEMERAL: &str = "system_ephemeral";
    pub const ME: &str = "me";
    pub const BURN_ON_READ: &str = "burn_on_read";
    pub const CUSTOM_PREFIX: &str = "custom_";

    /// `Post.IsValid`'s types.
    pub const VALID: &[&str] = &[
        "",
        "system_generic",
        JOIN_LEAVE,
        AUTO_RESPONDER,
        ADD_REMOVE,
        JOIN_CHANNEL,
        GUEST_JOIN_CHANNEL,
        LEAVE_CHANNEL,
        JOIN_TEAM,
        LEAVE_TEAM,
        ADD_TO_CHANNEL,
        ADD_GUEST_TO_CHANNEL,
        REMOVE_FROM_CHANNEL,
        "system_move_channel",
        ADD_TO_TEAM,
        REMOVE_FROM_TEAM,
        "system_team_abac_removal",
        "system_team_abac_addition",
        "slack_attachment",
        HEADER_CHANGE,
        PURPOSE_CHANGE,
        "system_displayname_change",
        "system_convert_channel",
        "system_channel_deleted",
        "system_channel_restored",
        "system_change_chan_privacy",
        "add_bot_teams_channels",
        "reminder",
        ME,
        "system_wrangler",
        "system_gm_to_channel",
        "system_autotranslation",
        BURN_ON_READ,
        "card",
        "system_shared_chan_state",
    ];

    /// `JoinLeaveMessagePostTypes`.
    pub const JOIN_LEAVE_TYPES: &[&str] = &[
        JOIN_LEAVE,
        ADD_REMOVE,
        JOIN_CHANNEL,
        LEAVE_CHANNEL,
        JOIN_TEAM,
        LEAVE_TEAM,
        ADD_TO_CHANNEL,
        REMOVE_FROM_CHANNEL,
        ADD_TO_TEAM,
        REMOVE_FROM_TEAM,
    ];

    /// `MembershipSystemPostTypes`.
    pub fn is_membership(t: &str) -> bool {
        JOIN_LEAVE_TYPES.contains(&t) || t == GUEST_JOIN_CHANNEL || t == ADD_GUEST_TO_CHANNEL
    }
    /// `IsAddMembershipSystemPost`.
    pub fn is_add_membership(t: &str) -> bool {
        t == ADD_TO_CHANNEL || t == ADD_GUEST_TO_CHANNEL || t == ADD_TO_TEAM
    }
}

impl Post {
    pub fn prop(&self, k: &str) -> Option<&Value> {
        self.props.0.get(k)
    }
    pub fn prop_str(&self, k: &str) -> &str {
        self.prop(k).and_then(Value::as_str).unwrap_or("")
    }
    pub fn is_system_message(&self) -> bool {
        self.r#type.starts_with(kind::SYSTEM_PREFIX)
    }
    pub fn is_join_leave(&self) -> bool {
        kind::JOIN_LEAVE_TYPES.contains(&self.r#type.as_str())
    }
    pub fn is_remote(&self) -> bool {
        self.remote_id.as_deref().is_some_and(|r| !r.is_empty())
    }
    pub fn from_webhook(&self) -> bool {
        self.prop_str("from_webhook") == "true"
    }
    /// `HasForceNotification`.
    pub fn has_force_notification(&self) -> bool {
        match self.prop("force_notification") {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => !s.is_empty(),
            _ => false,
        }
    }
    /// `IsNotificationSuppressed`.
    pub fn is_notification_suppressed(&self) -> bool {
        !self.has_force_notification() && self.prop("silent_notification") == Some(&Value::Bool(true))
    }
    /// `ExcludesFromChannelMessageCount`.
    pub fn excludes_from_message_count(&self) -> bool {
        self.is_join_leave() || self.is_notification_suppressed()
    }
    pub fn priority(&self) -> Option<&PostPriority> {
        self.metadata.as_ref()?.priority.as_ref()
    }
    /// `IsUrgent`.
    pub fn is_urgent(&self) -> bool {
        self.priority().and_then(|p| p.priority.as_deref()) == Some("urgent")
    }
    /// `SanitizeProps`: what a client may not set.
    pub fn sanitize_props(&mut self) {
        let p = &mut self.props.0;
        p.remove("add_channel_member");
        if !self.remote_id.as_deref().is_some_and(|r| !r.is_empty()) {
            for k in [
                "force_notification",
                "silent_notification",
                "from_webhook",
                "from_bot",
                "from_oauth_app",
                "from_plugin",
                "override_username",
                "override_icon_url",
                "override_icon_emoji",
                "webhook_display_name",
            ] {
                p.remove(k);
            }
        }
    }
    /// `SanitizeNonIdentityProps`.
    pub fn sanitize_non_identity_props(&mut self) {
        let remote = self.is_remote();
        let p = &mut self.props.0;
        p.remove("add_channel_member");
        if !remote {
            p.remove("force_notification");
            p.remove("silent_notification");
        }
    }
    /// `Post.Etag`-free `PreSave`: id, times, no original id.
    pub fn pre_save(&mut self, now: i64) {
        if self.id.is_empty() {
            self.id = crate::app::new_id();
        }
        self.original_id.clear();
        crate::integrations::generate_action_ids(&mut self.props.0);
        if self.create_at == 0 {
            self.create_at = now;
        }
        self.update_at = self.create_at;
        let mut seen = std::collections::HashSet::new();
        let ids = std::mem::take(&mut self.file_ids.0).into_iter().filter(|id| seen.insert(id.clone())).collect();
        self.file_ids = FileIds(ids, false);
        self.strip_nul();
    }

    /// Postgres text and jsonb cannot hold NUL.
    pub fn strip_nul(&mut self) {
        self.message.retain(|c| c != '\0');
        self.hashtags.retain(|c| c != '\0');
        let props = std::mem::take(&mut self.props.0);
        self.props.0 = props.into_iter().map(|(k, v)| (k.replace('\0', ""), strip_nul(v))).collect();
    }
}

fn strip_nul(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace('\0', "")),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nul).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k.replace('\0', ""), strip_nul(v))).collect()),
        v => v,
    }
}

/// `PostMetadata`: what `PreparePostForClient` adds; empty fields are left out.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct PostMetadata {
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "super::null_default")]
    pub embeds: Vec<PostEmbed>,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "super::null_default")]
    pub emojis: Vec<Emoji>,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "super::null_default")]
    pub files: Vec<FileInfo>,
    #[serde(skip_serializing_if = "is_zero")]
    pub redacted_file_count: i64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "super::null_default")]
    pub images: BTreeMap<String, crate::opengraph::PostImage>,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "super::null_default")]
    pub reactions: Vec<Reaction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<PostPriority>,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "super::null_default")]
    pub acknowledgements: Vec<PostAcknowledgement>,
}

/// `model.PostEmbed`: `type`, then `url` and `data` when set, in Go's order.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct PostEmbed {
    pub r#type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<EmbedData>,
}

impl PostEmbed {
    pub fn new(kind: &str, url: &str, data: Option<EmbedData>) -> PostEmbed {
        PostEmbed { r#type: kind.into(), url: url.into(), data }
    }
}

/// What an embed carries, each serialized in its Go struct's field order.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum EmbedData {
    /// Anything a client sent (read back as it came).
    Other(Value),
    OpenGraph(Box<crate::opengraph::OpenGraph>),
    Permalink(Box<PreviewPost>),
}

/// `model.PreviewPost`: a permalink's embed.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct PreviewPost {
    pub post_id: String,
    pub post: Option<Post>,
    pub team_name: String,
    pub channel_display_name: String,
    pub channel_type: String,
    pub channel_id: String,
}

/// `PostPriority` (`PostsPriority`); `PostId` and `ChannelId` keep Go's JSON names.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, sqlx::FromRow)]
#[serde(default)]
pub struct PostPriority {
    pub priority: Option<String>,
    #[sqlx(rename = "requestedack")]
    pub requested_ack: Option<bool>,
    #[sqlx(rename = "persistentnotifications")]
    pub persistent_notifications: Option<bool>,
    #[serde(rename = "PostId", skip_serializing_if = "String::is_empty")]
    #[sqlx(rename = "postid")]
    pub post_id: String,
    #[serde(rename = "ChannelId", skip_serializing_if = "String::is_empty")]
    #[sqlx(rename = "channelid")]
    pub channel_id: String,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, sqlx::FromRow)]
#[serde(default)]
pub struct PostAcknowledgement {
    #[sqlx(rename = "userid")]
    pub user_id: String,
    #[sqlx(rename = "postid")]
    pub post_id: String,
    #[sqlx(rename = "acknowledgedat")]
    pub acknowledged_at: i64,
    #[sqlx(rename = "channelid", default)]
    pub channel_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[sqlx(rename = "remoteid")]
    pub remote_id: Option<String>,
}

/// `PostList`.
#[derive(Serialize, Default, Debug, Clone)]
pub struct PostList {
    pub order: Vec<String>,
    pub posts: BTreeMap<String, Post>,
    pub next_post_id: String,
    pub prev_post_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_next: Option<bool>,
    pub first_inaccessible_post_time: i64,
}

impl PostList {
    pub fn add(&mut self, p: Post) {
        self.posts.insert(p.id.clone(), p);
    }
    pub fn add_ordered(&mut self, p: Post) {
        self.order.push(p.id.clone());
        self.add(p);
    }
}

/// `ThreadResponse`.
#[derive(Serialize, Default, Debug, Clone)]
pub struct ThreadResponse {
    pub id: String,
    pub reply_count: i64,
    pub last_reply_at: i64,
    pub last_viewed_at: i64,
    #[serde(serialize_with = "serialize_participants")]
    pub participants: Vec<User>,
    pub post: Option<Post>,
    pub unread_replies: i64,
    pub unread_mentions: i64,
    pub is_urgent: bool,
    pub delete_at: i64,
}

/// `Threads`.
#[derive(Serialize, Default, Debug)]
pub struct Threads {
    pub total: i64,
    pub total_unread_threads: i64,
    pub total_unread_mentions: i64,
    pub total_unread_urgent_mentions: i64,
    /// `null` with `totalsOnly` (Go never sets the slice), a list (maybe empty) otherwise.
    pub threads: Option<Vec<ThreadResponse>>,
}

/// Thread participants as Go writes them: without `extended` they are `&model.User{Id: id}`, whose nil `Timezone`
/// is `null` (a loaded user's is an object, `{}` at least). Such a user is told by its missing `CreateAt`.
fn serialize_participants<S: serde::Serializer>(users: &[User], s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let mut seq = s.serialize_seq(Some(users.len()))?;
    for u in users {
        if u.create_at == 0 && u.timezone.0.is_empty() {
            let mut v = serde_json::to_value(u).map_err(serde::ser::Error::custom)?;
            v["timezone"] = Value::Null;
            seq.serialize_element(&v)?;
        } else {
            seq.serialize_element(u)?;
        }
    }
    seq.end()
}

fn serialize_participants_opt<S: serde::Serializer>(users: &Option<Vec<User>>, s: S) -> Result<S::Ok, S::Error> {
    match users {
        Some(u) => serialize_participants(u, s),
        None => s.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_save_drops_repeated_file_ids() {
        let mut p = Post { file_ids: FileIds(vec!["a".into(), "b".into(), "a".into()], true), ..Default::default() };
        p.pre_save(1);
        assert_eq!(p.file_ids.0, vec!["a".to_string(), "b".to_string()]);
        assert!(!p.file_ids.1);
    }

    #[test]
    fn nul_stripped() {
        let mut p = Post { message: "a\0b".into(), hashtags: "#a\0".into(), ..Default::default() };
        p.props.0.insert("k\0".into(), serde_json::json!({"x": ["y\0"]}));
        p.strip_nul();
        assert_eq!((p.message.as_str(), p.hashtags.as_str()), ("ab", "#a"));
        assert_eq!(p.props.0["k"], serde_json::json!({"x": ["y"]}));
    }
}
