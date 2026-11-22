// Based on Mattermost server/public/model (preference.go, reaction.go, emoji.go, file_info.go, draft.go,
// channel_sidebar.go, role.go, scheme.go, thread.go, bot.go), Copyright (c) 2015-present Mattermost, Inc.
// Licensed under AGPL-3.0.

use ebachat_macros::model;
use serde_json::Value;

use super::{Spaced, StringArray};

#[model(table = "preferences")]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Preference {
    pub user_id: String,
    pub category: String,
    pub name: String,
    pub value: String,
}

#[model(table = "reactions")]
#[derive(Debug, Clone, PartialEq)]
pub struct Reaction {
    pub user_id: String,
    pub post_id: String,
    pub emoji_name: String,
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
    pub remote_id: Option<String>,
    pub channel_id: String,
}

#[model(table = "emoji")]
#[derive(Debug, Clone, PartialEq)]
pub struct Emoji {
    pub id: String,
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
    pub creator_id: String,
    pub name: String,
}

#[model(table = "fileinfo")]
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub id: String,
    #[mm(json = "user_id")]
    pub creator_id: String,
    #[mm(omitempty)]
    pub post_id: String,
    pub channel_id: String,
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
    #[mm(skip_json)]
    pub path: String,
    #[mm(skip_json)]
    pub thumbnail_path: String,
    #[mm(skip_json)]
    pub preview_path: String,
    pub name: String,
    pub extension: String,
    pub size: i64,
    pub mime_type: String,
    #[mm(omitempty)]
    pub width: i32,
    #[mm(omitempty)]
    pub height: i32,
    #[mm(omitempty)]
    pub has_preview_image: bool,
    pub mini_preview: Option<Base64>,
    #[mm(skip_json)]
    pub content: String,
    pub remote_id: Option<String>,
    pub archived: bool,
}

/// Go's `[]byte` in JSON: standard base64.
#[derive(Debug, Clone, Default, PartialEq, sqlx::Type)]
#[sqlx(transparent)]
pub struct Base64(pub Vec<u8>);

impl serde::Serialize for Base64 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use base64::Engine;
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}

impl<'de> serde::Deserialize<'de> for Base64 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use base64::Engine;
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD.decode(s).map(Base64).map_err(serde::de::Error::custom)
    }
}

#[model(table = "drafts")]
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
    pub user_id: String,
    pub channel_id: String,
    pub root_id: String,
    pub message: String,
    pub r#type: String,
    pub props: super::JsonText<serde_json::Map<String, Value>>,
    #[mm(omitempty)]
    pub file_ids: StringArray,
    #[mm(transient, omitempty)]
    pub metadata: Option<Value>,
    #[mm(omitempty)]
    pub priority: super::JsonText<serde_json::Map<String, Value>>,
}

#[model(table = "sidebarcategories")]
#[derive(Debug, Clone, PartialEq)]
pub struct SidebarCategory {
    pub id: String,
    pub user_id: String,
    pub team_id: String,
    pub sort_order: i64,
    pub sorting: String,
    pub r#type: String,
    pub display_name: String,
    pub muted: bool,
    pub collapsed: bool,
    #[mm(transient, json = "channel_ids")]
    pub channel_ids: Vec<String>,
}

pub mod category {
    pub const FAVORITES: &str = "favorites";
    pub const CHANNELS: &str = "channels";
    pub const DIRECT_MESSAGES: &str = "direct_messages";
    pub const CUSTOM: &str = "custom";
    pub const SORT_DEFAULT: &str = "";
    pub const SORT_MANUAL: &str = "manual";
    pub const SORT_RECENT: &str = "recent";
    pub const SORT_ALPHA: &str = "alpha";
}

#[model(table = "roles")]
#[derive(Debug, Clone, PartialEq)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
    pub permissions: Spaced,
    pub scheme_managed: bool,
    #[mm(column = "builtin")]
    pub built_in: bool,
    pub scheme_id: Option<String>,
}

#[model(table = "schemes")]
#[derive(Debug, Clone, PartialEq)]
pub struct Scheme {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
    pub scope: String,
    pub default_team_admin_role: String,
    pub default_team_user_role: String,
    pub default_channel_admin_role: String,
    pub default_channel_user_role: String,
    pub default_team_guest_role: String,
    pub default_channel_guest_role: String,
    pub default_playbook_admin_role: String,
    pub default_playbook_member_role: String,
    pub default_run_admin_role: String,
    pub default_run_member_role: String,
}

#[model(table = "threads")]
#[derive(Debug, Clone, PartialEq)]
pub struct Thread {
    #[mm(json = "id")]
    pub post_id: String,
    pub channel_id: String,
    pub reply_count: i64,
    pub last_reply_at: i64,
    pub participants: sqlx::types::Json<Vec<String>>,
    #[mm(column = "threaddeleteat")]
    pub delete_at: i64,
    #[mm(column = "threadteamid")]
    pub team_id: String,
}

#[model(table = "threadmemberships")]
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadMembership {
    pub post_id: String,
    pub user_id: String,
    pub following: bool,
    #[mm(json = "last_update_at", column = "lastupdated")]
    pub last_updated: i64,
    #[mm(json = "last_view_at", column = "lastviewed")]
    pub last_viewed: i64,
    pub unread_mentions: i64,
}

#[model(table = "bots")]
#[derive(Debug, Clone, PartialEq)]
pub struct Bot {
    pub user_id: String,
    #[mm(transient)]
    pub username: String,
    #[mm(transient, omitempty)]
    pub display_name: String,
    #[mm(omitempty)]
    pub description: String,
    pub owner_id: String,
    #[mm(omitempty)]
    pub last_icon_update: i64,
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
}
