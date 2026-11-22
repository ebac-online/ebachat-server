//! Bulk import (`import_process` jobs): Mattermost's JSONL import format, from a zip in the import directory (or a
//! local file in local mode), line by line into the database as Go's importer writes it. The line types, their
//! validation and error ids, the segment workers, attachments from the zip, scoped imports and the resume
//! checkpoints are Go's; the writes for each line type are in `import_functions.rs`. See docs/JOBS.md.

// Based on Mattermost server/channels/jobs/import_process/worker.go, app/import.go, app/imports/import_types.go,
// app/imports/import_validators.go, app/imports/import_report.go and app/import_utils.go, Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::app::{App, AppError, State};
use crate::jobs::{self, Job};
use crate::user_edit::CustomStatus;

// ---------- errors ----------

/// A `*model.AppError` as the importer makes it: `Error()` is `where: message[, detail][, wrapped]`.
#[derive(Debug, Clone)]
pub struct IErr(Box<Inner>);

#[derive(Debug, Clone)]
struct Inner {
    where_: String,
    id: String,
    params: Vec<(String, String)>,
    /// The message of an error made elsewhere (an `AppError` of the rest of the server), already translated.
    message: Option<String>,
    detail: String,
    wrapped: String,
    status: u16,
}

impl IErr {
    pub fn new(where_: &str, id: &str, status: u16) -> IErr {
        IErr(Box::new(Inner {
            where_: where_.into(),
            id: id.into(),
            params: vec![],
            message: None,
            detail: String::new(),
            wrapped: String::new(),
            status,
        }))
    }
    /// `NewAppError("BulkImport", id, ..., 400)`.
    pub fn bad(id: &str) -> IErr {
        IErr::new("BulkImport", id, 400)
    }
    pub fn id(&self) -> &str {
        &self.0.id
    }
    pub fn p(mut self, k: &str, v: impl ToString) -> IErr {
        self.0.params.push((k.into(), v.to_string()));
        self
    }
    pub fn detail(mut self, d: impl Into<String>) -> IErr {
        self.0.detail = d.into();
        self
    }
    /// `.Wrap(err)`.
    pub fn wrap(mut self, e: impl std::fmt::Display) -> IErr {
        self.0.wrapped = e.to_string();
        self
    }
    /// The error as made at `where_` (Go's `Where`).
    pub fn at(mut self, where_: &str) -> IErr {
        self.0.where_ = where_.into();
        self
    }
    pub fn status(mut self, s: u16) -> IErr {
        self.0.status = s;
        self
    }
    fn message(&self) -> String {
        if let Some(m) = &self.0.message {
            return m.clone();
        }
        let args: Vec<(&str, &str)> = self.0.params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        crate::i18n::tr(&self.0.id, &args).unwrap_or_else(|| self.0.id.clone())
    }
    /// `AppError.Error()`.
    pub fn text(&self) -> String {
        let e = &self.0;
        let mut s = String::new();
        if !e.where_.is_empty() {
            s += &e.where_;
            s += ": ";
        }
        s += &self.message();
        if !e.detail.is_empty() {
            s += ", ";
            s += &e.detail;
        }
        if !e.wrapped.is_empty() {
            s += ", ";
            s += &e.wrapped;
        }
        s
    }
}

impl From<AppError> for IErr {
    fn from(e: AppError) -> IErr {
        IErr(Box::new(Inner {
            where_: String::new(),
            id: e.id,
            params: vec![],
            message: Some(e.message),
            detail: e.detailed_error,
            wrapped: String::new(),
            status: e.status_code,
        }))
    }
}

impl From<sqlx::Error> for IErr {
    fn from(e: sqlx::Error) -> IErr {
        IErr::from(AppError::from(e))
    }
}

pub type IResult<T> = Result<T, IErr>;

// ---------- the line types (imports/import_types.go) ----------

/// Go's decoder leaves a plain string field alone on `null`.
fn null_str<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// A file of the zip (`*zip.File`): its name and uncompressed size.
#[derive(Clone, Debug)]
pub struct ZipRef {
    pub name: String,
    pub size: u64,
}

#[derive(Deserialize, Default, Clone)]
pub struct LineData {
    #[serde(rename = "type", default, deserialize_with = "null_str")]
    pub r#type: String,
    pub role: Option<RoleData>,
    pub scheme: Option<SchemeData>,
    pub team: Option<TeamData>,
    pub channel: Option<ChannelData>,
    pub user: Option<UserData>,
    pub bot: Option<BotData>,
    pub post: Option<PostData>,
    pub direct_channel: Option<DirectChannelData>,
    pub direct_post: Option<DirectPostData>,
    pub emoji: Option<EmojiData>,
    pub version: Option<i64>,
    pub info: Option<VersionInfo>,
}

#[derive(Deserialize, Default, Clone)]
pub struct VersionInfo {
    #[serde(default, deserialize_with = "null_str")]
    pub generator: String,
    #[serde(default, deserialize_with = "null_str")]
    pub version: String,
    #[serde(default, deserialize_with = "null_str")]
    pub created: String,
    #[serde(default)]
    pub additional: Option<Value>,
}

/// `ExportScopeAdditional`.
#[derive(Deserialize, Default)]
struct ExportScope {
    #[serde(default, deserialize_with = "null_str")]
    team_name: String,
    #[serde(default, deserialize_with = "null_str")]
    channel_name: String,
}

#[derive(Deserialize, Default, Clone)]
pub struct TeamData {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub r#type: Option<String>,
    pub description: Option<String>,
    pub allow_open_invite: Option<bool>,
    pub scheme: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
pub struct ChannelData {
    pub team: Option<String>,
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub r#type: Option<String>,
    pub header: Option<String>,
    pub purpose: Option<String>,
    pub scheme: Option<String>,
    pub deleted_at: Option<i64>,
}

#[derive(Deserialize, Default, Clone)]
pub struct UserData {
    pub profile_image: Option<String>,
    #[serde(skip)]
    pub profile_image_data: Option<ZipRef>,
    pub username: Option<String>,
    pub email: Option<String>,
    pub auth_service: Option<String>,
    pub auth_data: Option<String>,
    pub password: Option<String>,
    pub nickname: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub position: Option<String>,
    pub roles: Option<String>,
    pub locale: Option<String>,
    #[serde(rename = "feature_enabled_markdown_preview")]
    pub use_markdown_preview: Option<String>,
    #[serde(rename = "formatting")]
    pub use_formatting: Option<String>,
    pub show_unread_section: Option<String>,
    pub delete_at: Option<i64>,
    pub send_on_ctrl_enter: Option<String>,
    pub code_block_ctrl_enter: Option<String>,
    pub show_join_leave: Option<String>,
    pub show_unread_scroll_position: Option<String>,
    pub sync_drafts: Option<String>,
    pub limit_visible_dms_gms: Option<String>,
    pub name_format: Option<String>,
    pub teams: Option<Vec<UserTeamData>>,
    pub theme: Option<String>,
    #[serde(rename = "military_time")]
    pub use_military_time: Option<String>,
    #[serde(rename = "link_previews")]
    pub collapse_previews: Option<String>,
    pub message_display: Option<String>,
    #[serde(rename = "collapse_consecutive_messages")]
    pub collapse_consecutive: Option<String>,
    pub colorize_usernames: Option<String>,
    pub channel_display_mode: Option<String>,
    pub tutorial_step: Option<String>,
    pub email_interval: Option<String>,
    pub notify_props: Option<UserNotifyProps>,
    pub custom_status: Option<CustomStatus>,
}

#[derive(Deserialize, Default, Clone)]
pub struct BotData {
    pub profile_image: Option<String>,
    #[serde(skip)]
    pub profile_image_data: Option<ZipRef>,
    pub username: Option<String>,
    pub owner: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub delete_at: Option<i64>,
}

#[derive(Deserialize, Default, Clone)]
pub struct UserNotifyProps {
    pub desktop: Option<String>,
    pub desktop_sound: Option<String>,
    pub email: Option<String>,
    pub mobile: Option<String>,
    pub mobile_push_status: Option<String>,
    #[serde(rename = "channel")]
    pub channel_trigger: Option<String>,
    #[serde(rename = "comments")]
    pub comments_trigger: Option<String>,
    pub mention_keys: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
pub struct UserTeamData {
    pub name: Option<String>,
    pub roles: Option<String>,
    pub theme: Option<String>,
    pub channels: Option<Vec<UserChannelData>>,
}

#[derive(Deserialize, Default, Clone)]
pub struct UserChannelData {
    pub name: Option<String>,
    pub roles: Option<String>,
    pub notify_props: Option<ChannelNotifyProps>,
    pub favorite: Option<bool>,
    pub mention_count: Option<i64>,
    pub mention_count_root: Option<i64>,
    /// Go's JSON name, typo included.
    #[serde(rename = "urgend_mention_count")]
    pub urgent_mention_count: Option<i64>,
    pub msg_count: Option<i64>,
    pub msg_count_root: Option<i64>,
    pub last_viewed_at: Option<i64>,
}

#[derive(Deserialize, Default, Clone)]
pub struct DirectChannelMemberData {
    pub username: Option<String>,
    pub notify_props: Option<ChannelNotifyProps>,
    pub scheme_user: Option<bool>,
    pub scheme_admin: Option<bool>,
    pub scheme_guest: Option<bool>,
    pub mention_count: Option<i64>,
    pub mention_count_root: Option<i64>,
    #[serde(rename = "urgend_mention_count")]
    pub urgent_mention_count: Option<i64>,
    pub msg_count: Option<i64>,
    pub msg_count_root: Option<i64>,
    pub last_viewed_at: Option<i64>,
}

#[derive(Deserialize, Default, Clone)]
pub struct ChannelNotifyProps {
    pub desktop: Option<String>,
    pub mobile: Option<String>,
    pub mark_unread: Option<String>,
    pub email: Option<String>,
    pub ignore_channel_mentions: Option<String>,
    pub channel_auto_follow_threads: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
pub struct EmojiData {
    pub name: Option<String>,
    pub image: Option<String>,
    #[serde(skip)]
    pub data: Option<ZipRef>,
}

#[derive(Deserialize, Default, Clone)]
pub struct ReactionData {
    pub user: Option<String>,
    pub create_at: Option<i64>,
    pub emoji_name: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
pub struct ReplyData {
    pub user: Option<String>,
    pub r#type: Option<String>,
    pub message: Option<String>,
    pub props: Option<Map<String, Value>>,
    pub create_at: Option<i64>,
    pub edit_at: Option<i64>,
    pub flagged_by: Option<Vec<String>>,
    pub reactions: Option<Vec<ReactionData>>,
    pub attachments: Option<Vec<AttachmentData>>,
    pub is_pinned: Option<bool>,
}

#[derive(Deserialize, Default, Clone)]
pub struct PostData {
    pub team: Option<String>,
    pub channel: Option<String>,
    pub user: Option<String>,
    pub r#type: Option<String>,
    pub message: Option<String>,
    pub props: Option<Map<String, Value>>,
    pub create_at: Option<i64>,
    pub edit_at: Option<i64>,
    pub flagged_by: Option<Vec<String>>,
    pub reactions: Option<Vec<ReactionData>>,
    pub replies: Option<Vec<ReplyData>>,
    pub attachments: Option<Vec<AttachmentData>>,
    pub is_pinned: Option<bool>,
    pub thread_followers: Option<Vec<ThreadFollowerData>>,
}

#[derive(Deserialize, Default, Clone)]
pub struct DirectChannelData {
    pub members: Option<Vec<String>>,
    pub participants: Option<Vec<DirectChannelMemberData>>,
    pub favorited_by: Option<Vec<String>>,
    pub shown_by: Option<Vec<String>>,
    pub header: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
pub struct DirectPostData {
    pub channel_members: Option<Vec<String>>,
    pub user: Option<String>,
    pub r#type: Option<String>,
    pub message: Option<String>,
    pub props: Option<Map<String, Value>>,
    pub create_at: Option<i64>,
    pub edit_at: Option<i64>,
    pub flagged_by: Option<Vec<String>>,
    pub reactions: Option<Vec<ReactionData>>,
    pub replies: Option<Vec<ReplyData>>,
    pub attachments: Option<Vec<AttachmentData>>,
    pub is_pinned: Option<bool>,
    pub thread_followers: Option<Vec<ThreadFollowerData>>,
}

#[derive(Deserialize, Default, Clone)]
pub struct SchemeData {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub scope: Option<String>,
    pub default_team_admin_role: Option<RoleData>,
    pub default_team_user_role: Option<RoleData>,
    pub default_channel_admin_role: Option<RoleData>,
    pub default_channel_user_role: Option<RoleData>,
    pub default_team_guest_role: Option<RoleData>,
    pub default_channel_guest_role: Option<RoleData>,
}

#[derive(Deserialize, Default, Clone)]
pub struct RoleData {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub permissions: Option<Vec<String>>,
    pub scheme_managed: Option<bool>,
}

#[derive(Deserialize, Default, Clone)]
pub struct AttachmentData {
    pub path: Option<String>,
    #[serde(skip)]
    pub data: Option<ZipRef>,
}

#[derive(Deserialize, Default, Clone)]
pub struct ThreadFollowerData {
    pub user: Option<String>,
    pub last_viewed: Option<i64>,
    pub unread_mentions: Option<i64>,
}

/// `json.Unmarshal` of a line into `LineImportData`: Go's syntax and type errors, keys matched case-insensitively.
pub fn decode_line(raw: &[u8]) -> Result<LineData, String> {
    crate::gojson::unmarshal(raw, &go_types::LINE)
        .and_then(|v| serde_json::from_value::<LineData>(v).map_err(|e| e.to_string()))
}

// ---------- the line types as Go declares them, for its decoding (`json.Unmarshal` into `LineImportData`) ----------

mod go_types {
    use crate::gojson::{Struct, Ty};

    const S: Ty = Ty::Str { go: "string", ptr: true };
    const PLAIN: Ty = Ty::Str { go: "string", ptr: false };
    const I64: Ty = Ty::Int { go: "int64" };
    const B: Ty = Ty::Bool;
    const LIST: Ty = Ty::StrList;
    const PROPS: Ty = Ty::Map { go: "model.StringInterface" };
    const fn st(st: &'static Struct) -> Ty {
        Ty::Struct { st, ptr: true }
    }
    const fn list(st: &'static Struct, go: &'static str) -> Ty {
        Ty::StructList { st, go, elem_ptr: false }
    }
    const fn imp(name: &'static str, fields: &'static [(&'static str, Ty)]) -> Struct {
        Struct { name, pkg: "imports", fields }
    }

    pub static LINE: Struct = imp(
        "LineImportData",
        &[
            ("type", PLAIN),
            ("role", st(&ROLE)),
            ("scheme", st(&SCHEME)),
            ("team", st(&TEAM)),
            ("channel", st(&CHANNEL)),
            ("user", st(&USER)),
            ("bot", st(&BOT)),
            ("post", st(&POST)),
            ("direct_channel", st(&DIRECT_CHANNEL)),
            ("direct_post", st(&DIRECT_POST)),
            ("emoji", st(&EMOJI)),
            ("version", Ty::Int { go: "int" }),
            ("info", st(&INFO)),
        ],
    );
    static INFO: Struct = imp(
        "VersionInfoImportData",
        &[("generator", PLAIN), ("version", PLAIN), ("created", PLAIN), ("additional", Ty::Raw)],
    );
    pub static EXPORT_SCOPE: Struct = imp("ExportScopeAdditional", &[("team_name", PLAIN), ("channel_name", PLAIN)]);
    static TEAM: Struct = imp(
        "TeamImportData",
        &[("name", S), ("display_name", S), ("type", S), ("description", S), ("allow_open_invite", B), ("scheme", S)],
    );
    static CHANNEL: Struct = imp(
        "ChannelImportData",
        &[
            ("team", S),
            ("name", S),
            ("display_name", S),
            ("type", Ty::Str { go: "model.ChannelType", ptr: true }),
            ("header", S),
            ("purpose", S),
            ("scheme", S),
            ("deleted_at", I64),
        ],
    );
    static USER: Struct = imp(
        "UserImportData",
        &[
            ("Avatar.profile_image", S),
            ("username", S),
            ("email", S),
            ("auth_service", S),
            ("auth_data", S),
            ("password", S),
            ("nickname", S),
            ("first_name", S),
            ("last_name", S),
            ("position", S),
            ("roles", S),
            ("locale", S),
            ("feature_enabled_markdown_preview", S),
            ("formatting", S),
            ("show_unread_section", S),
            ("delete_at", I64),
            ("send_on_ctrl_enter", S),
            ("code_block_ctrl_enter", S),
            ("show_join_leave", S),
            ("show_unread_scroll_position", S),
            ("sync_drafts", S),
            ("limit_visible_dms_gms", S),
            ("name_format", S),
            ("teams", list(&USER_TEAM, "[]imports.UserTeamImportData")),
            ("theme", S),
            ("military_time", S),
            ("link_previews", S),
            ("message_display", S),
            ("collapse_consecutive_messages", S),
            ("colorize_usernames", S),
            ("channel_display_mode", S),
            ("tutorial_step", S),
            ("email_interval", S),
            ("notify_props", st(&USER_NOTIFY)),
            ("custom_status", st(&CUSTOM_STATUS)),
        ],
    );
    static CUSTOM_STATUS: Struct = Struct {
        name: "CustomStatus",
        pkg: "model",
        fields: &[("emoji", PLAIN), ("text", PLAIN), ("duration", PLAIN), ("expires_at", Ty::Time)],
    };
    static BOT: Struct = imp(
        "BotImportData",
        &[
            ("Avatar.profile_image", S),
            ("username", S),
            ("owner", S),
            ("display_name", S),
            ("description", S),
            ("delete_at", I64),
        ],
    );
    static USER_NOTIFY: Struct = imp(
        "UserNotifyPropsImportData",
        &[
            ("desktop", S),
            ("desktop_sound", S),
            ("email", S),
            ("mobile", S),
            ("mobile_push_status", S),
            ("channel", S),
            ("comments", S),
            ("mention_keys", S),
        ],
    );
    static USER_TEAM: Struct = imp(
        "UserTeamImportData",
        &[
            ("name", S),
            ("roles", S),
            ("theme", S),
            ("channels", list(&USER_CHANNEL, "[]imports.UserChannelImportData")),
        ],
    );
    static USER_CHANNEL: Struct = imp(
        "UserChannelImportData",
        &[
            ("name", S),
            ("roles", S),
            ("notify_props", st(&CHANNEL_NOTIFY)),
            ("favorite", B),
            ("mention_count", I64),
            ("mention_count_root", I64),
            ("urgend_mention_count", I64),
            ("msg_count", I64),
            ("msg_count_root", I64),
            ("last_viewed_at", I64),
        ],
    );
    static DIRECT_MEMBER: Struct = imp(
        "DirectChannelMemberImportData",
        &[
            ("username", S),
            ("notify_props", st(&CHANNEL_NOTIFY)),
            ("scheme_user", B),
            ("scheme_admin", B),
            ("scheme_guest", B),
            ("mention_count", I64),
            ("mention_count_root", I64),
            ("urgend_mention_count", I64),
            ("msg_count", I64),
            ("msg_count_root", I64),
            ("last_viewed_at", I64),
        ],
    );
    static CHANNEL_NOTIFY: Struct = imp(
        "UserChannelNotifyPropsImportData",
        &[
            ("desktop", S),
            ("mobile", S),
            ("mark_unread", S),
            ("email", S),
            ("ignore_channel_mentions", S),
            ("channel_auto_follow_threads", S),
        ],
    );
    static EMOJI: Struct = imp("EmojiImportData", &[("name", S), ("image", S)]);
    static REACTION: Struct = imp("ReactionImportData", &[("user", S), ("create_at", I64), ("emoji_name", S)]);
    static ATTACHMENT: Struct = imp("AttachmentImportData", &[("path", S)]);
    static FOLLOWER: Struct =
        imp("ThreadFollowerImportData", &[("user", S), ("last_viewed", I64), ("unread_mentions", I64)]);
    const REACTIONS: Ty = Ty::StructList { st: &REACTION, go: "[]imports.ReactionImportData", elem_ptr: false };
    const ATTACHMENTS: Ty = Ty::StructList { st: &ATTACHMENT, go: "[]imports.AttachmentImportData", elem_ptr: false };
    const FOLLOWERS: Ty = Ty::StructList { st: &FOLLOWER, go: "[]imports.ThreadFollowerImportData", elem_ptr: false };
    const REPLIES: Ty = Ty::StructList { st: &REPLY, go: "[]imports.ReplyImportData", elem_ptr: false };
    static REPLY: Struct = imp(
        "ReplyImportData",
        &[
            ("user", S),
            ("type", S),
            ("message", S),
            ("props", PROPS),
            ("create_at", I64),
            ("edit_at", I64),
            ("flagged_by", LIST),
            ("reactions", REACTIONS),
            ("attachments", ATTACHMENTS),
            ("is_pinned", B),
        ],
    );
    static POST: Struct = imp(
        "PostImportData",
        &[
            ("team", S),
            ("channel", S),
            ("user", S),
            ("type", S),
            ("message", S),
            ("props", PROPS),
            ("create_at", I64),
            ("edit_at", I64),
            ("flagged_by", LIST),
            ("reactions", REACTIONS),
            ("replies", REPLIES),
            ("attachments", ATTACHMENTS),
            ("is_pinned", B),
            ("thread_followers", FOLLOWERS),
        ],
    );
    static DIRECT_CHANNEL: Struct = imp(
        "DirectChannelImportData",
        &[
            ("members", LIST),
            (
                "participants",
                Ty::StructList { st: &DIRECT_MEMBER, go: "[]*imports.DirectChannelMemberImportData", elem_ptr: true },
            ),
            ("favorited_by", LIST),
            ("shown_by", LIST),
            ("header", S),
        ],
    );
    static DIRECT_POST: Struct = imp(
        "DirectPostImportData",
        &[
            ("channel_members", LIST),
            ("user", S),
            ("type", S),
            ("message", S),
            ("props", PROPS),
            ("create_at", I64),
            ("edit_at", I64),
            ("flagged_by", LIST),
            ("reactions", REACTIONS),
            ("replies", REPLIES),
            ("attachments", ATTACHMENTS),
            ("is_pinned", B),
            ("thread_followers", FOLLOWERS),
        ],
    );
    static SCHEME: Struct = imp(
        "SchemeImportData",
        &[
            ("name", S),
            ("display_name", S),
            ("description", S),
            ("scope", S),
            ("default_team_admin_role", st(&ROLE)),
            ("default_team_user_role", st(&ROLE)),
            ("default_channel_admin_role", st(&ROLE)),
            ("default_channel_user_role", st(&ROLE)),
            ("default_team_guest_role", st(&ROLE)),
            ("default_channel_guest_role", st(&ROLE)),
        ],
    );
    static ROLE: Struct = imp(
        "RoleImportData",
        &[("name", S), ("display_name", S), ("description", S), ("permissions", LIST), ("scheme_managed", B)],
    );
}

/// `UsernameRemap`: source username (lower case) → the destination's, found by auth data.
#[derive(Default)]
pub struct Report {
    remap: Mutex<HashMap<String, String>>,
}

impl Report {
    pub fn add(&self, src: &str, dest: &str) {
        self.remap.lock().unwrap().insert(src.to_lowercase(), dest.to_string());
    }
    pub fn lookup(&self, src: &str) -> Option<String> {
        self.remap.lock().unwrap().get(&src.to_lowercase()).cloned()
    }
}

// ---------- validators (imports/import_validators.go) ----------

pub mod limits {
    pub const TEAM_NAME_MAX: usize = 64;
    pub const TEAM_DISPLAY_NAME_MAX_RUNES: usize = 64;
    pub const TEAM_DESCRIPTION_MAX: usize = 255;
    pub const CHANNEL_NAME_MAX: usize = 64;
    pub const CHANNEL_DISPLAY_NAME_MAX_RUNES: usize = 64;
    pub const CHANNEL_HEADER_MAX_RUNES: usize = 1024;
    pub const CHANNEL_PURPOSE_MAX_RUNES: usize = 250;
    pub const SCHEME_DISPLAY_NAME_MAX: usize = 128;
    pub const SCHEME_DESCRIPTION_MAX: usize = 1024;
    pub const ROLE_DISPLAY_NAME_MAX: usize = 128;
    pub const ROLE_DESCRIPTION_MAX: usize = 1024;
    pub const USER_EMAIL_MAX: usize = 128;
    pub const USER_AUTH_DATA_MAX: usize = 128;
    pub const USER_PASSWORD_MAX: usize = 72;
    pub const USER_NICKNAME_MAX_RUNES: usize = 64;
    pub const USER_FIRST_NAME_MAX_RUNES: usize = 64;
    pub const USER_LAST_NAME_MAX_RUNES: usize = 64;
    pub const USER_POSITION_MAX_RUNES: usize = 128;
    pub const EMOJI_NAME_MAX: usize = 64;
    pub const POST_PROPS_MAX_RUNES: usize = 800000;
    pub const GROUP_MIN_USERS: usize = 3;
    pub const GROUP_MAX_USERS: usize = 8;
    pub const MENTION_KEYS_MAX_COUNT: usize = 500;
    pub const MENTION_KEYS_MAX_LENGTH: usize = 50000;
}
use limits::*;

/// The directory of the zip with the attachments (`model.ExportDataDir`).
pub const EXPORT_DATA_DIR: &str = "data";

fn runes(s: &str) -> usize {
    s.chars().count()
}

/// `IsRootJsonlFile`.
pub fn is_root_jsonl(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    base.rfind('.').map(|i| &base[i..]) == Some(".jsonl") && go_dir(name) == "."
}

/// `filepath.Dir`.
fn go_dir(p: &str) -> String {
    let d = match p.rfind('/') {
        Some(i) => &p[..i + 1],
        None => "",
    };
    go_clean(d)
}

/// `filepath.Clean`.
pub fn go_clean(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let rooted = p.starts_with('/');
    let mut parts: Vec<&str> = vec![];
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

/// `ValidateAttachmentPathForImport`: `path` under `base` (`.` when empty), or none when it escapes.
pub fn attachment_path(path: &str, base: &str) -> Option<String> {
    let base = if base.is_empty() { "." } else { base };
    let joined = go_clean(&format!("{base}/{path}"));
    // filepath.Rel(base, joined)
    let b = go_clean(base);
    if joined.starts_with('/') != b.starts_with('/') {
        return None;
    }
    let rel = if b == "." {
        joined.clone()
    } else if joined == b {
        ".".into()
    } else if let Some(r) = joined.strip_prefix(&format!("{b}/")) {
        r.to_string()
    } else {
        "..".into()
    };
    if rel == ".." || rel.starts_with("../") {
        return None;
    }
    Some(joined)
}

/// `IsValidSchemeName`.
pub fn valid_scheme_name(s: &str) -> bool {
    (2..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// `IsValidUserRoles`.
fn valid_user_roles(roles: &str) -> bool {
    let r: Vec<&str> = roles.split_whitespace().collect();
    r.iter().all(|n| crate::roles::valid_role_name(n)) && !(r.len() == 1 && r[0] == "system_admin")
}

/// `IsInRole`.
pub fn in_role(roles: &str, role: &str) -> bool {
    roles.split_whitespace().any(|r| r == role)
}

pub fn validate_scheme(d: &SchemeData) -> IResult<()> {
    let Some(scope) = &d.scope else {
        return Err(IErr::bad("app.import.validate_scheme_import_data.null_scope.error"));
    };
    let wrong = || Err(IErr::bad("app.import.validate_scheme_import_data.wrong_roles_for_scope.error"));
    match scope.as_str() {
        "team" => {
            if d.default_team_admin_role.is_none()
                || d.default_team_user_role.is_none()
                || d.default_channel_admin_role.is_none()
                || d.default_channel_user_role.is_none()
            {
                return wrong();
            }
        }
        "channel" => {
            if d.default_team_admin_role.is_some()
                || d.default_team_user_role.is_some()
                || d.default_channel_admin_role.is_none()
                || d.default_channel_user_role.is_none()
            {
                return wrong();
            }
        }
        _ => return Err(IErr::bad("app.import.validate_scheme_import_data.unknown_scheme.error")),
    }
    if !d.name.as_deref().is_some_and(valid_scheme_name) {
        return Err(IErr::bad("app.import.validate_scheme_import_data.name_invalid.error"));
    }
    if d.display_name.as_deref().is_none_or(|n| n.is_empty() || n.len() > SCHEME_DISPLAY_NAME_MAX) {
        return Err(IErr::bad("app.import.validate_scheme_import_data.display_name_invalid.error"));
    }
    if d.description.as_deref().is_some_and(|n| n.len() > SCHEME_DESCRIPTION_MAX) {
        return Err(IErr::bad("app.import.validate_scheme_import_data.description_invalid.error"));
    }
    for r in [
        &d.default_team_admin_role,
        &d.default_team_user_role,
        &d.default_team_guest_role,
        &d.default_channel_admin_role,
        &d.default_channel_user_role,
        &d.default_channel_guest_role,
    ]
    .into_iter()
    .flatten()
    {
        validate_role(r)?;
    }
    Ok(())
}

pub fn validate_role(d: &RoleData) -> IResult<()> {
    if !d.name.as_deref().is_some_and(crate::roles::valid_role_name) {
        return Err(IErr::bad("app.import.validate_role_import_data.name_invalid.error"));
    }
    if d.display_name.as_deref().is_none_or(|n| n.is_empty() || n.len() > ROLE_DISPLAY_NAME_MAX) {
        return Err(IErr::bad("app.import.validate_role_import_data.display_name_invalid.error"));
    }
    if d.description.as_deref().is_some_and(|n| n.len() > ROLE_DESCRIPTION_MAX) {
        return Err(IErr::bad("app.import.validate_role_import_data.description_invalid.error"));
    }
    for p in d.permissions.iter().flatten() {
        if !crate::roles::ALL_PERMISSIONS.lines().any(|l| l == p) && !DEPRECATED_PERMISSIONS.contains(&p.as_str()) {
            return Err(IErr::bad("app.import.validate_role_import_data.invalid_permission.error")
                .detail(format!("permission{p}")));
        }
    }
    Ok(())
}

/// `model.DeprecatedPermissions`.
const DEPRECATED_PERMISSIONS: &[&str] = &[
    "permanent_delete_user",
    "create_user_access_token",
    "read_user_access_token",
    "revoke_user_access_token",
    "manage_emojis",
    "manage_others_emojis",
    "manage_webhooks",
    "manage_others_webhooks",
    "manage_oauth",
    "manage_system_wide_oauth",
    "manage_slash_commands",
    "manage_others_slash_commands",
];

/// `IsReservedTeamName`.
fn reserved_team_name(s: &str) -> bool {
    const RESERVED: &[&str] = &[
        "admin",
        "api",
        "channel",
        "claim",
        "error",
        "files",
        "help",
        "landing",
        "login",
        "mfa",
        "oauth",
        "plug",
        "plugins",
        "post",
        "signup",
        "boards",
        "playbooks",
    ];
    let l = s.to_lowercase();
    RESERVED.iter().any(|r| l.starts_with(r))
}

pub fn validate_team(d: &TeamData) -> IResult<()> {
    let e = |id: &str| Err(IErr::bad(&format!("app.import.validate_team_import_data.{id}.error")));
    match &d.name {
        None => return e("name_missing"),
        Some(n) if n.len() > TEAM_NAME_MAX => return e("name_length"),
        Some(n) if reserved_team_name(n) => return e("name_reserved"),
        Some(n) if !crate::team_admin::valid_team_name(&n.to_lowercase()) => return e("name_characters"),
        _ => {}
    }
    match &d.display_name {
        None => return e("display_name_missing"),
        Some(n) if runes(n) == 0 || runes(n) > TEAM_DISPLAY_NAME_MAX_RUNES => return e("display_name_length"),
        _ => {}
    }
    match d.r#type.as_deref() {
        None => return e("type_missing"),
        Some("O") | Some("I") => {}
        Some(_) => return e("type_invalid"),
    }
    if d.description.as_deref().is_some_and(|s| s.len() > TEAM_DESCRIPTION_MAX) {
        return e("description_length");
    }
    if d.scheme.as_deref().is_some_and(|s| !valid_scheme_name(s)) {
        return e("scheme_invalid");
    }
    Ok(())
}

/// `ValidateChannelImportData`; a missing display name becomes the name.
pub fn validate_channel(d: &mut ChannelData) -> IResult<()> {
    let e = |id: &str| Err(IErr::bad(&format!("app.import.validate_channel_import_data.{id}.error")));
    if d.team.is_none() {
        return e("team_missing");
    }
    match &d.name {
        None => return e("name_missing"),
        Some(n) if n.len() > CHANNEL_NAME_MAX => return e("name_length"),
        Some(n) if !crate::channels::is_valid_channel_name(&n.to_lowercase()) => return e("name_characters"),
        _ => {}
    }
    match &d.display_name {
        None => d.display_name = d.name.clone(),
        Some(n) if n.is_empty() => d.display_name = d.name.clone(),
        Some(n) if runes(n) > CHANNEL_DISPLAY_NAME_MAX_RUNES => return e("display_name_length"),
        _ => {}
    }
    match d.r#type.as_deref() {
        None => return e("type_missing"),
        Some("O") | Some("P") => {}
        Some(_) => return e("type_invalid"),
    }
    if d.header.as_deref().is_some_and(|s| runes(s) > CHANNEL_HEADER_MAX_RUNES) {
        return e("header_length");
    }
    if d.purpose.as_deref().is_some_and(|s| runes(s) > CHANNEL_PURPOSE_MAX_RUNES) {
        return e("purpose_length");
    }
    if d.scheme.as_deref().is_some_and(|s| !valid_scheme_name(s)) {
        return e("scheme_invalid");
    }
    Ok(())
}

/// The profile image checks of user and bot lines without the image in the zip: under `data/`, and on disk.
fn validate_profile_image(path: &Option<String>, data: &Option<ZipRef>) -> IResult<()> {
    let (Some(p), None) = (path, data) else { return Ok(()) };
    if attachment_path(p, EXPORT_DATA_DIR).is_none() {
        return Err(IErr::bad("app.import.validate_user_import_data.invalid_image_path.error").p("Path", p));
    }
    match std::fs::metadata(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(IErr::bad("app.import.validate_user_import_data.profile_image.error")
                .status(404)
                .wrap(format!("stat {p}: no such file or directory")))
        }
        Err(e) => Err(IErr::bad("app.import.validate_user_import_data.profile_image.error").wrap(e)),
        Ok(_) => Ok(()),
    }
}

pub fn validate_user(d: &UserData) -> IResult<()> {
    let e = |id: &str| Err(IErr::bad(&format!("app.import.validate_user_import_data.{id}.error")));
    validate_profile_image(&d.profile_image, &d.profile_image_data)?;
    match &d.username {
        None => return e("username_missing"),
        Some(u) if !crate::user_edit::valid_username(&u.to_lowercase()) => return e("username_invalid"),
        _ => {}
    }
    match &d.email {
        None => return e("email_missing"),
        Some(m) if m.is_empty() || m.len() > USER_EMAIL_MAX => return e("email_length"),
        _ => {}
    }
    if d.auth_data.is_some() && d.password.is_some() {
        return e("auth_data_and_password");
    }
    if d.auth_data.as_deref().is_some_and(|a| a.len() > USER_AUTH_DATA_MAX) {
        return e("auth_data_length");
    }
    let blank = |s: &Option<String>| s.as_deref().is_none_or(str::is_empty);
    if blank(&d.auth_service) != blank(&d.auth_data) {
        return e("auth_data_and_service_dependency");
    }
    if let Some(s) = &d.auth_service
        && !["", "email", "gitlab", "saml", "ldap", "google", "office365", "openid"].contains(&s.as_str())
    {
        return Err(
            IErr::bad("app.import.validate_user_teams_import_data.invalid_auth_service.error").p("AuthService", s)
        );
    }
    if d.password.as_deref().is_some_and(|p| p.is_empty() || p.len() > USER_PASSWORD_MAX) {
        return e("password_length");
    }
    if d.nickname.as_deref().is_some_and(|s| runes(s) > USER_NICKNAME_MAX_RUNES) {
        return e("nickname_length");
    }
    if d.first_name.as_deref().is_some_and(|s| runes(s) > USER_FIRST_NAME_MAX_RUNES) {
        return e("first_name_length");
    }
    if d.last_name.as_deref().is_some_and(|s| runes(s) > USER_LAST_NAME_MAX_RUNES) {
        return e("last_name_length");
    }
    if d.position.as_deref().is_some_and(|s| runes(s) > USER_POSITION_MAX_RUNES) {
        return e("position_length");
    }
    if d.roles.as_deref().is_some_and(|r| !valid_user_roles(r)) {
        return e("roles_invalid");
    }
    validate_guest_roles(d)?;
    if let Some(np) = &d.notify_props {
        let level = |v: &str| ["all", "mention", "none"].contains(&v);
        let tf = |v: &str| v == "true" || v == "false";
        if np.desktop.as_deref().is_some_and(|v| !level(v)) {
            return e("notify_props_desktop_invalid");
        }
        if np.desktop_sound.as_deref().is_some_and(|v| !tf(v)) {
            return e("notify_props_desktop_sound_invalid");
        }
        if np.email.as_deref().is_some_and(|v| !tf(v)) {
            return e("notify_props_email_invalid");
        }
        if np.mobile.as_deref().is_some_and(|v| !level(v)) {
            return e("notify_props_mobile_invalid");
        }
        if np.mobile_push_status.as_deref().is_some_and(|v| !["online", "away", "offline"].contains(&v)) {
            return e("notify_props_mobile_push_status_invalid");
        }
        if np.channel_trigger.as_deref().is_some_and(|v| !tf(v)) {
            return e("notify_props_channel_trigger_invalid");
        }
        if np.comments_trigger.as_deref().is_some_and(|v| !["any", "root", "never"].contains(&v)) {
            return e("notify_props_comments_trigger_invalid");
        }
        if np.mention_keys.as_deref().is_some_and(mention_keys_exceed_limits) {
            return Err(IErr::bad("app.import.validate_user_import_data.notify_props_mention_keys_invalid.error")
                .p("MaxCount", MENTION_KEYS_MAX_COUNT)
                .p("MaxLength", MENTION_KEYS_MAX_LENGTH));
        }
    }
    let tf = |v: &Option<String>| v.as_deref().is_some_and(|v| v != "true" && v != "false");
    if tf(&d.use_markdown_preview) {
        return e("advanced_props_feature_markdown_preview");
    }
    if tf(&d.use_formatting) {
        return e("advanced_props_formatting");
    }
    if tf(&d.show_unread_section) {
        return e("advanced_props_show_unread_section");
    }
    if d.email_interval.as_deref().is_some_and(|v| !["immediately", "fifteen", "hour"].contains(&v)) {
        return e("advanced_props_email_interval");
    }
    if let Some(teams) = &d.teams {
        return validate_user_teams(teams);
    }
    Ok(())
}

/// `MentionKeysExceedLimits`.
fn mention_keys_exceed_limits(raw: &str) -> bool {
    if raw.len() > MENTION_KEYS_MAX_LENGTH {
        return true;
    }
    let keys: Vec<String> = raw.split(',').map(|k| k.trim().to_lowercase()).filter(|k| !k.is_empty()).collect();
    keys.len() > MENTION_KEYS_MAX_COUNT || keys.join(",").len() > MENTION_KEYS_MAX_LENGTH
}

pub fn validate_bot(d: &BotData) -> IResult<()> {
    validate_profile_image(&d.profile_image, &d.profile_image_data)?;
    match &d.username {
        None => return Err(IErr::bad("app.import.validate_user_import_data.username_missing.error")),
        Some(u) if !crate::user_edit::valid_username(&u.to_lowercase()) => {
            return Err(IErr::bad("app.import.validate_user_import_data.username_invalid.error"));
        }
        _ => {}
    }
    if d.display_name.as_deref().is_some_and(|s| runes(s) > USER_FIRST_NAME_MAX_RUNES) {
        return Err(IErr::bad("app.import.validate_user_import_data.first_name_length.error"));
    }
    match &d.owner {
        None => Err(IErr::bad("app.import.validate_bot_import_data.owner_missing.error")),
        Some(o) if !crate::user_edit::valid_username(o) => {
            Err(IErr::bad("app.import.validate_user_import_data.username_invalid.error"))
        }
        _ => Ok(()),
    }
}

fn validate_user_teams(teams: &[UserTeamData]) -> IResult<()> {
    let e = |id: &str| Err(IErr::bad(&format!("app.import.validate_user_teams_import_data.{id}.error")));
    for t in teams {
        if t.name.is_none() {
            return e("team_name_missing");
        }
        if t.roles.as_deref().is_some_and(|r| !valid_user_roles(r)) {
            return e("invalid_roles");
        }
        if let Some(c) = &t.channels {
            validate_user_channels(c)?;
        }
        if let Some(theme) = &t.theme
            && !theme.trim_matches([' ', '\t', '\r']).is_empty()
            && let Err(err) = serde_json::from_str::<HashMap<String, String>>(theme)
        {
            return Err(IErr::bad("app.import.validate_user_teams_import_data.invalid_team_theme.error").wrap(err));
        }
    }
    Ok(())
}

fn validate_user_channels(channels: &[UserChannelData]) -> IResult<()> {
    let e = |id: &str| Err(IErr::bad(&format!("app.import.validate_user_channels_import_data.{id}.error")));
    // IsChannelNotifyLevelValid, IsChannelMarkUnreadLevelValid
    let level = |v: &str| ["default", "all", "mention", "none"].contains(&v);
    for c in channels {
        if c.name.is_none() {
            return e("channel_name_missing");
        }
        if c.roles.as_deref().is_some_and(|r| !valid_user_roles(r)) {
            return e("invalid_roles");
        }
        if let Some(np) = &c.notify_props {
            if np.desktop.as_deref().is_some_and(|v| !level(v)) {
                return e("invalid_notify_props_desktop");
            }
            if np.mobile.as_deref().is_some_and(|v| !level(v)) {
                return e("invalid_notify_props_mobile");
            }
            if np.mark_unread.as_deref().is_some_and(|v| v != "all" && v != "mention") {
                return e("invalid_notify_props_mark_unread");
            }
        }
    }
    Ok(())
}

pub fn validate_reaction(d: &ReactionData, parent_create_at: i64) -> IResult<()> {
    let e = |id: &str| Err(IErr::bad(&format!("app.import.validate_reaction_import_data.{id}.error")));
    if d.user.is_none() {
        return e("user_missing");
    }
    match &d.emoji_name {
        None => return e("emoji_name_missing"),
        Some(n) if runes(n) > EMOJI_NAME_MAX => return e("emoji_name_length"),
        _ => {}
    }
    match d.create_at {
        None => e("create_at_missing"),
        Some(0) => e("create_at_zero"),
        Some(t) if t < parent_create_at => e("create_at_before_parent"),
        _ => Ok(()),
    }
}

/// `StringInterfaceToJSON` length in runes.
fn props_runes(p: &Map<String, Value>) -> usize {
    runes(&String::from_utf8(crate::bulk_export::go_json(p)).unwrap_or_default())
}

pub fn validate_reply(d: &ReplyData, parent_create_at: i64, max_post_size: usize) -> IResult<()> {
    let e = |id: &str| Err(IErr::bad(&format!("app.import.validate_reply_import_data.{id}.error")));
    if d.user.is_none() {
        return e("user_missing");
    }
    match &d.message {
        None => return e("message_missing"),
        Some(m) if runes(m) > max_post_size => return e("message_length"),
        _ => {}
    }
    match d.create_at {
        None => return e("create_at_missing"),
        Some(0) => return e("create_at_zero"),
        Some(t) if t < parent_create_at => {
            crate::logs::warn(
                &format!(
                    "Reply CreateAt is before parent post CreateAt reply_create_at={t} parent_create_at={parent_create_at}"
                ),
                &[],
            );
        }
        _ => {}
    }
    if d.props.as_ref().is_some_and(|p| props_runes(p) > POST_PROPS_MAX_RUNES) {
        return Err(IErr::bad("app.import.validate_post_import_data.props_too_large.error"));
    }
    let create_at = d.create_at.unwrap_or_default();
    for r in d.reactions.iter().flatten() {
        validate_reaction(r, create_at)?;
    }
    for a in d.attachments.iter().flatten() {
        if let Err(err) = validate_attachment(a) {
            return Err(IErr::bad("app.import.validate_reply_import_data.attachment.error")
                .status(404)
                .wrap(err.text()));
        }
    }
    Ok(())
}

pub fn validate_post(d: &PostData, max_post_size: usize) -> IResult<()> {
    let e = |id: &str| Err(IErr::bad(&format!("app.import.validate_post_import_data.{id}.error")));
    if d.team.is_none() {
        return e("team_missing");
    }
    if d.channel.is_none() {
        return e("channel_missing");
    }
    if d.user.is_none() {
        return e("user_missing");
    }
    match &d.message {
        None => return e("message_missing"),
        Some(m) if runes(m) > max_post_size => return e("message_length"),
        _ => {}
    }
    let create_at = match d.create_at {
        None => return e("create_at_missing"),
        Some(0) => return e("create_at_zero"),
        Some(t) => t,
    };
    for r in d.reactions.iter().flatten() {
        validate_reaction(r, create_at)?;
    }
    for r in d.replies.iter().flatten() {
        validate_reply(r, create_at, max_post_size)?;
    }
    if d.props.as_ref().is_some_and(|p| props_runes(p) > POST_PROPS_MAX_RUNES) {
        return e("props_too_large");
    }
    for a in d.attachments.iter().flatten() {
        if let Err(err) = validate_attachment(a) {
            return Err(IErr::bad("app.import.validate_post_import_data.attachment.error")
                .status(404)
                .wrap(err.text()));
        }
    }
    for f in d.thread_followers.iter().flatten() {
        if let Err(err) = validate_thread_follower(f) {
            return Err(IErr::bad("app.import.validate_post_import_data.thread_follower.error").wrap(err.text()));
        }
    }
    Ok(())
}

pub fn validate_direct_channel(d: &DirectChannelData) -> IResult<()> {
    let e = |id: &str| Err(IErr::bad(&format!("app.import.validate_direct_channel_import_data.{id}.error")));
    if d.participants.is_none() && d.members.is_none() {
        return e("members_required");
    }
    for n in [d.participants.as_ref().map(Vec::len), d.members.as_ref().map(Vec::len)].into_iter().flatten() {
        if n != 2 {
            if n < GROUP_MIN_USERS {
                return e("members_too_few");
            } else if n > GROUP_MAX_USERS {
                return e("members_too_many");
            }
        }
    }
    if d.header.as_deref().is_some_and(|h| runes(h) > CHANNEL_HEADER_MAX_RUNES) {
        return e("header_length");
    }
    for f in d.favorited_by.iter().flatten() {
        let in_participants = d.participants.iter().flatten().any(|m| m.username.as_deref() == Some(f.as_str()));
        let in_members = d.members.iter().flatten().any(|m| m == f);
        if !in_participants && !in_members {
            return Err(
                IErr::bad("app.import.validate_direct_channel_import_data.unknown_favoriter.error").p("Username", f)
            );
        }
    }
    Ok(())
}

pub fn validate_direct_post(d: &DirectPostData, max_post_size: usize) -> IResult<()> {
    let e = |id: &str| Err(IErr::bad(&format!("app.import.validate_direct_post_import_data.{id}.error")));
    let Some(members) = &d.channel_members else { return e("channel_members_required") };
    if members.len() != 2 {
        if members.len() < GROUP_MIN_USERS {
            return e("channel_members_too_few");
        } else if members.len() > GROUP_MAX_USERS {
            return e("channel_members_too_many");
        }
    }
    if d.user.is_none() {
        return e("user_missing");
    }
    match &d.message {
        None => return e("message_missing"),
        Some(m) if runes(m) > max_post_size => return e("message_length"),
        _ => {}
    }
    let create_at = match d.create_at {
        None => return e("create_at_missing"),
        Some(0) => return e("create_at_zero"),
        Some(t) => t,
    };
    for f in d.flagged_by.iter().flatten() {
        if !members.contains(f) {
            return Err(IErr::bad("app.import.validate_direct_post_import_data.unknown_flagger.error").p("Username", f));
        }
    }
    for r in d.reactions.iter().flatten() {
        validate_reaction(r, create_at)?;
    }
    for r in d.replies.iter().flatten() {
        validate_reply(r, create_at, max_post_size)?;
    }
    for a in d.attachments.iter().flatten() {
        if let Err(err) = validate_attachment(a) {
            return Err(IErr::bad("app.import.validate_direct_post_import_data.attachment.error")
                .status(404)
                .wrap(err.text()));
        }
    }
    for f in d.thread_followers.iter().flatten() {
        if let Err(err) = validate_thread_follower(f) {
            return Err(IErr::bad("app.import.validate_direct_post_import_data.thread_follower.error").wrap(err.text()));
        }
    }
    Ok(())
}

pub fn validate_emoji(d: &EmojiData) -> IResult<()> {
    let e = |id: &str| Err(IErr::bad(&format!("app.import.validate_emoji_import_data.{id}.error")));
    if d.name.as_deref().is_none_or(str::is_empty) {
        return e("name_missing");
    }
    let image = match &d.image {
        Some(i) if !i.is_empty() => i,
        _ => return e("image_missing"),
    };
    if attachment_path(image, EXPORT_DATA_DIR).is_none() {
        return Err(IErr::bad("app.import.validate_emoji_import_data.invalid_image_path.error").p("Path", image));
    }
    crate::emoji::check_name(d.name.as_deref().unwrap_or_default()).map_err(|e| IErr::from(e).at("Emoji.IsValid"))
}

fn validate_thread_follower(d: &ThreadFollowerData) -> IResult<()> {
    if d.user.as_deref().is_none_or(str::is_empty) {
        return Err(IErr::bad("app.import.validate_thread_follower_data.user_missing.error"));
    }
    Ok(())
}

fn validate_attachment(d: &AttachmentData) -> IResult<()> {
    let Some(p) = d.path.as_deref().filter(|p| !p.is_empty()) else { return Ok(()) };
    if attachment_path(p, EXPORT_DATA_DIR).is_none() {
        return Err(IErr::bad("app.import.validate_attachment_import_data.invalid_path.error").p("Path", p));
    }
    Ok(())
}

/// `validateGuestRoles`: a system guest is a guest in all its teams and channels, and only a system guest is.
fn validate_guest_roles(d: &UserData) -> IResult<()> {
    let Some(roles) = &d.roles else { return Ok(()) };
    let system_guest = in_role(roles, "system_guest");
    let Some(teams) = d.teams.as_ref().filter(|t| !t.is_empty()) else { return Ok(()) };
    let (mut team_guests, mut channel_guests, mut channels) = (0, 0, 0);
    let mut has_channels = false;
    for t in teams {
        if t.roles.as_deref().is_some_and(|r| in_role(r, "team_guest")) {
            team_guests += 1;
        }
        if let Some(cs) = t.channels.as_ref().filter(|c| !c.is_empty()) {
            has_channels = true;
            channels += cs.len();
            channel_guests +=
                cs.iter().filter(|c| c.roles.as_deref().is_some_and(|r| in_role(r, "channel_guest"))).count();
        }
    }
    let team_guest = team_guests == teams.len();
    let channel_guest = has_channels && channel_guests == channels;
    if system_guest {
        if !team_guest {
            return Err(IErr::bad("app.import.validate_user_import_data.system_guest_missing_team_guest_roles.error")
                .p("TeamGuestCount", team_guests)
                .p("TotalTeams", teams.len()));
        }
        if has_channels && !channel_guest {
            return Err(IErr::bad(
                "app.import.validate_user_import_data.system_guest_missing_channel_guest_roles.error",
            )
            .p("ChannelGuestCount", channel_guests)
            .p("TotalChannels", channels));
        }
        return Ok(());
    }
    if team_guest {
        return Err(IErr::bad("app.import.validate_user_import_data.team_guest_missing_system_guest_role.error"));
    }
    if channel_guest {
        return Err(IErr::bad("app.import.validate_user_import_data.channel_guest_missing_system_guest_role.error"));
    }
    Ok(())
}

// ---------- the zip ----------

/// The import zip, read where it lies: a file of the local store (shared, read at offsets) or the bytes of an S3
/// object. Cloning is cheap, so each reader gets its own cursor.
#[derive(Clone)]
pub enum Src {
    File(Arc<std::fs::File>, u64, u64),
    Mem(std::io::Cursor<bytes::Bytes>),
}

impl Read for Src {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Src::File(f, pos, _) => {
                use std::os::unix::fs::FileExt;
                let n = f.read_at(buf, *pos)?;
                *pos += n as u64;
                Ok(n)
            }
            Src::Mem(c) => c.read(buf),
        }
    }
}

impl Seek for Src {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        match self {
            Src::File(_, pos, len) => {
                let p = match to {
                    SeekFrom::Start(o) => o as i64,
                    SeekFrom::End(o) => *len as i64 + o,
                    SeekFrom::Current(o) => *pos as i64 + o,
                };
                if p < 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "negative position"));
                }
                *pos = p as u64;
                Ok(*pos)
            }
            Src::Mem(c) => c.seek(to),
        }
    }
}

pub type Archive = zip::ZipArchive<Src>;

/// A zip error as Go's `archive/zip` words the common one.
fn zip_error_text(e: &zip::result::ZipError) -> String {
    match e {
        zip::result::ZipError::InvalidArchive(_) => "zip: not a valid zip file".into(),
        e => e.to_string(),
    }
}

/// A file of the zip, whole (at most `limit` bytes when given; one more is read to tell it is longer).
pub async fn read_zip_file(archive: &Archive, name: &str, limit: Option<u64>) -> Result<Vec<u8>, String> {
    let mut a = archive.clone();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let f = a.by_name(&name).map_err(|e| zip_error_text(&e))?;
        let mut out = vec![];
        match limit {
            Some(l) => f.take(l + 1).read_to_end(&mut out),
            None => {
                let mut f = f;
                f.read_to_end(&mut out)
            }
        }
        .map_err(|e| e.to_string())?;
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------- the job (jobs/import_process/worker.go) ----------

/// `BulkImportOpts`.
#[derive(Default, Clone)]
pub struct Opts {
    pub destination_team: String,
    pub destination_channel: String,
    pub skip_preflight: bool,
    pub imported_users: String,
    pub resume_from_line: usize,
}

/// Checkpoints of a big import, persisted in the job's data.
pub struct Checkpoints<'a> {
    app: &'a App,
    job: &'a mut Job,
    file: String,
}

impl Checkpoints<'_> {
    async fn save(&mut self, line: i64) {
        if line < 0 {
            self.job.data_mut().insert("total_lines".into(), (-line).to_string());
        } else {
            self.job.data_mut().insert("checkpoint".into(), line.to_string());
            self.job.data_mut().insert("checkpoint_file".into(), self.file.clone());
        }
        if let Err(e) = jobs::update_in_progress_data(self.app, self.job).await {
            crate::logs::warn(&format!("Failed to persist import checkpoint err={}", e.detailed_error), &[]);
        }
    }
}

/// `ImportProcess` worker's `execute`.
pub async fn import_job(app: &State, job: &mut Job) -> Result<(), String> {
    let Some(file) = job.get("import_file").map(str::to_string) else {
        return Err(jobs::go_error("ImportProcessWorker", "import_process.worker.do_job.missing_file", ""));
    };
    let local_mode = job.get("local_mode") == Some("true");
    let mut store_path = String::new();
    let src = if local_mode {
        let f = match std::fs::File::open(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(format!("file {file} doesn't exist.")),
            Err(e) => return Err(format!("open {file}: {e}")),
            Ok(f) => f,
        };
        let len = f.metadata().map_err(|e| e.to_string())?.len();
        Src::File(Arc::new(f), 0, len)
    } else {
        store_path = go_clean(&format!("{}/{file}", app.cfg.str("ImportSettings.Directory")));
        if !app.files.exists(&store_path).await {
            return Err(jobs::go_error("ImportProcessWorker", "import_process.worker.do_job.file_exists", ""));
        }
        match app.files.local_path(&store_path).and_then(|p| std::fs::File::open(p).ok()) {
            Some(f) => {
                let len = f.metadata().map_err(|e| e.to_string())?.len();
                Src::File(Arc::new(f), 0, len)
            }
            None => {
                let bytes = app.files.read(&store_path).await.map_err(|e| {
                    IErr::new("ReadFile", "api.file.read_file.reading_local.app_error", 500).wrap(e).text()
                })?;
                Src::Mem(std::io::Cursor::new(bytes))
            }
        }
    };
    let archive =
        tokio::task::spawn_blocking(move || Archive::new(src)).await.map_err(|e| e.to_string())?.map_err(|e| {
            IErr::new("ImportProcessWorker", "import_process.worker.do_job.open_file", 500)
                .wrap(zip_error_text(&e))
                .text()
        })?;
    let jsonl = archive.file_names().find(|n| is_root_jsonl(n)).map(str::to_string);
    let Some(jsonl) = jsonl else {
        return Err(IErr::new("ImportProcessWorker", "import_process.worker.do_job.missing_jsonl", 400)
            .detail("jsonFile was nil")
            .text());
    };
    if jsonl.contains("..") {
        return Err(IErr::new("ImportProcessWorker", "import_process.worker.do_job.open_file", 403)
            .detail("jsonFilePath contains path traversal")
            .text());
    }
    let jsonl_size = {
        let mut a = archive.clone();
        let name = jsonl.clone();
        tokio::task::spawn_blocking(move || a.by_name(&name).map(|f| f.size()))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| {
                IErr::new("ImportProcessWorker", "import_process.worker.do_job.open_file", 500)
                    .wrap(zip_error_text(&e))
                    .text()
            })?
    };
    let extract_content = job.get("extract_content") == Some("true");
    let workers = job
        .get("workers")
        .and_then(|w| w.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .map(|n| n as usize)
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
    let mut resume_from_line = 0;
    if let Some(cp) = job.get("checkpoint").filter(|c| !c.is_empty())
        && let Ok(n) = cp.parse::<i64>()
    {
        if job.get("checkpoint_file") == Some(file.as_str()) {
            resume_from_line = n.max(0) as usize;
            crate::logs::info(&format!("Resuming import from checkpoint resume_from_line={n} import_file={file}"), &[]);
        } else {
            crate::logs::warn("Import file changed since last checkpoint; starting from beginning", &[]);
        }
    }
    let opts = Opts {
        destination_team: job.get("destination_team_name").unwrap_or_default().to_string(),
        destination_channel: job.get("destination_channel_name").unwrap_or_default().to_string(),
        skip_preflight: job.get("skip_preflight") == Some("true"),
        imported_users: job.get("imported_users").unwrap_or_default().to_string(),
        resume_from_line,
    };
    // Checkpoints for imports of 100 MB of JSONL or more.
    const CHECKPOINT_THRESHOLD: u64 = 100 * 1024 * 1024;
    let mut cp = Checkpoints { app, job, file: file.clone() };
    let checkpoints = if jsonl_size >= CHECKPOINT_THRESHOLD { Some(&mut cp) } else { None };
    let (line, err) =
        bulk_import(app, &archive, &jsonl, false, extract_content, workers, EXPORT_DATA_DIR, &opts, checkpoints).await;
    if let Some(err) = err {
        job.data_mut().insert("line_number".into(), line.to_string());
        return Err(err.text());
    }
    if !local_mode && let Err(e) = app.files.remove(&store_path).await {
        return Err(IErr::new("RemoveFile", "api.file.remove_file.app_error", 500).wrap(e).text());
    }
    Ok(())
}

// ---------- the import (app/import.go) ----------

/// A line for the segment's workers.
pub struct WorkLine {
    pub data: LineData,
    pub line: usize,
}

/// What the workers share.
pub struct Ctx {
    pub app: State,
    pub archive: Archive,
    pub dry_run: bool,
    pub extract_content: bool,
    pub deactivate_missing_users: bool,
    pub imported_users: String,
    pub destination_team: String,
    pub report: Report,
}

const MULTIPLE_POSTS_THRESHOLD: usize = 1000;
const MAX_LINE: usize = 16 * 1024 * 1024;

/// `stopOnError`: the errors the import goes on after.
fn stop_on_error(e: &IErr, deactivate_missing_users: bool) -> bool {
    match e.id() {
        "api.file.upload_file.large_image.app_error" => {
            crate::logs::warn(&format!("Large image import error err={}", e.text()), &[]);
            false
        }
        "app.import.validate_direct_channel_import_data.members_too_few.error"
        | "app.import.validate_direct_channel_import_data.members_too_many.error" => {
            crate::logs::warn(&format!("Invalid direct channel import data err={}", e.text()), &[]);
            false
        }
        "app.user.save.email_exists.app_error" | "app.user.save.username_exists.app_error" => !deactivate_missing_users,
        "app.user.update.lastAdmin.app_error" => !deactivate_missing_users,
        _ => true,
    }
}

/// `splitTrimNames`.
fn split_trim_names(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect()
}

/// `processAttachmentPaths`: each path under `base`, its file in the zip when there is one.
fn process_attachment_paths(
    files: &mut Option<Vec<AttachmentData>>,
    base: &str,
    map: &HashMap<String, ZipRef>,
) -> Result<(), String> {
    let mut errs = vec![];
    for f in files.iter_mut().flatten() {
        let Some(original) = f.path.clone() else { continue };
        let joined = attachment_path(&original, base);
        f.path = Some(joined.clone().unwrap_or_default());
        let Some(joined) = joined else {
            errs.push(format!("invalid attachment path {original:?}"));
            continue;
        };
        if !map.is_empty() {
            f.data = map.get(&normalize_name(&joined)).cloned();
            if f.data.is_none() {
                errs.push(format!("attachment {original:?} not found in map"));
            }
        }
    }
    if errs.is_empty() { Ok(()) } else { Err(errs.join("\n")) }
}

/// `utils.NormalizeFilename`: NFC.
fn normalize_name(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc().collect()
}

/// `processAttachments`.
fn process_attachments(line: &mut LineData, base: &str, map: &HashMap<String, ZipRef>) -> Result<(), String> {
    let image = |path: &mut Option<String>, data: &mut Option<ZipRef>, what: &str| -> Result<(), String> {
        let Some(p) = path.clone() else { return Ok(()) };
        let Some(joined) = attachment_path(&p, base) else { return Err(format!("invalid {what} path {p:?}")) };
        *path = Some(joined.clone());
        if !map.is_empty() {
            *data = map.get(&normalize_name(&joined)).cloned();
            if data.is_none() {
                return Err(format!("attachment {joined:?} not found in map"));
            }
        }
        Ok(())
    };
    match line.r#type.as_str() {
        "post" | "direct_post" => {
            let (attachments, replies) = if line.r#type == "direct_post" {
                let Some(p) = line.direct_post.as_mut() else { return Ok(()) };
                (&mut p.attachments, &mut p.replies)
            } else {
                let Some(p) = line.post.as_mut() else { return Ok(()) };
                (&mut p.attachments, &mut p.replies)
            };
            process_attachment_paths(attachments, base, map)?;
            for r in replies.iter_mut().flatten() {
                process_attachment_paths(&mut r.attachments, base, map)?;
            }
            Ok(())
        }
        "user" => match line.user.as_mut() {
            Some(u) => image(&mut u.profile_image, &mut u.profile_image_data, "profile image"),
            None => Ok(()),
        },
        "bot" => match line.bot.as_mut() {
            Some(b) => image(&mut b.profile_image, &mut b.profile_image_data, "bot profile image"),
            None => Ok(()),
        },
        "emoji" => match line.emoji.as_mut() {
            Some(e) => image(&mut e.image, &mut e.data, "emoji image"),
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

/// `rewriteTeamName`.
fn rewrite_team_name(line: &mut LineData, src: &str, dest: &str) {
    let set = |v: &mut Option<String>| {
        if v.as_deref() == Some(src) {
            *v = Some(dest.to_string());
        }
    };
    match line.r#type.as_str() {
        "team" => line.team.as_mut().into_iter().for_each(|t| set(&mut t.name)),
        "channel" => line.channel.as_mut().into_iter().for_each(|c| set(&mut c.team)),
        "user" => line.user.iter_mut().flat_map(|u| u.teams.iter_mut().flatten()).for_each(|t| set(&mut t.name)),
        "post" => line.post.as_mut().into_iter().for_each(|p| set(&mut p.team)),
        _ => {}
    }
}

/// `rewriteChannelName`.
fn rewrite_channel_name(line: &mut LineData, src: &str, dest: &str) {
    let set = |v: &mut Option<String>| {
        if v.as_deref() == Some(src) {
            *v = Some(dest.to_string());
        }
    };
    match line.r#type.as_str() {
        "channel" => line.channel.as_mut().into_iter().for_each(|c| set(&mut c.name)),
        "user" => line
            .user
            .iter_mut()
            .flat_map(|u| u.teams.iter_mut().flatten())
            .flat_map(|t| t.channels.iter_mut().flatten())
            .for_each(|c| set(&mut c.name)),
        "post" => line.post.as_mut().into_iter().for_each(|p| set(&mut p.channel)),
        _ => {}
    }
}

/// The segment's workers: lines in, errors out.
struct Segment {
    tx: tokio::sync::mpsc::Sender<WorkLine>,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl Segment {
    fn start(ctx: &Arc<Ctx>, workers: usize, errors: &tokio::sync::mpsc::UnboundedSender<(IErr, usize)>) -> Segment {
        let (tx, rx) = tokio::sync::mpsc::channel::<WorkLine>(workers.max(1));
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let handles = (0..workers.max(1))
            .map(|_| {
                let (ctx, rx, errors) = (ctx.clone(), rx.clone(), errors.clone());
                tokio::spawn(async move { worker(ctx, rx, errors).await })
            })
            .collect();
        Segment { tx, handles }
    }

    /// `drainWorkers`.
    async fn drain(self) {
        drop(self.tx);
        for h in self.handles {
            let _ = h.await;
        }
    }
}

/// `bulkImportWorker`: post lines batched by a thousand, the others one by one.
async fn worker(
    ctx: Arc<Ctx>,
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<WorkLine>>>,
    errors: tokio::sync::mpsc::UnboundedSender<(IErr, usize)>,
) {
    let mut posts: Vec<WorkLine> = vec![];
    let mut direct_posts: Vec<WorkLine> = vec![];
    loop {
        let next = rx.lock().await.recv().await;
        let Some(l) = next else { break };
        match l.data.r#type.as_str() {
            "post" => {
                if l.data.post.is_none() {
                    let _ = errors.send((IErr::bad("app.import.import_line.null_post.error"), l.line));
                    continue;
                }
                posts.push(l);
                if posts.len() >= MULTIPLE_POSTS_THRESHOLD
                    && let Err((line, e)) = ctx.import_multiple_post_lines(std::mem::take(&mut posts)).await
                {
                    let _ = errors.send((e, line));
                }
            }
            "direct_post" => {
                if l.data.direct_post.is_none() {
                    let _ = errors.send((IErr::bad("app.import.import_line.null_direct_post.error"), l.line));
                    continue;
                }
                direct_posts.push(l);
                if direct_posts.len() >= MULTIPLE_POSTS_THRESHOLD
                    && let Err((line, e)) =
                        ctx.import_multiple_direct_post_lines(std::mem::take(&mut direct_posts)).await
                {
                    let _ = errors.send((e, line));
                }
            }
            _ => {
                let line = l.line;
                if let Err(e) = ctx.import_line(l.data).await {
                    let _ = errors.send((e, line));
                }
            }
        }
    }
    if !posts.is_empty()
        && let Err((line, e)) = ctx.import_multiple_post_lines(posts).await
    {
        let _ = errors.send((e, line));
    }
    if !direct_posts.is_empty()
        && let Err((line, e)) = ctx.import_multiple_direct_post_lines(direct_posts).await
    {
        let _ = errors.send((e, line));
    }
}

/// The JSONL file of the zip, line by line (`bufio.Scanner` with a 16 MB token limit) from a blocking reader.
fn read_lines(archive: &Archive, name: &str) -> tokio::sync::mpsc::Receiver<Result<Vec<u8>, String>> {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    let mut a = archive.clone();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let f = match a.by_name(&name) {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.blocking_send(Err(zip_error_text(&e)));
                return;
            }
        };
        let mut r = std::io::BufReader::with_capacity(64 * 1024, f);
        loop {
            let mut buf = vec![];
            match r.by_ref().take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf) {
                Ok(0) => return,
                Ok(_) => {
                    let ended = buf.last() == Some(&b'\n');
                    if ended {
                        buf.pop();
                    }
                    if buf.len() > MAX_LINE || (!ended && buf.len() == MAX_LINE + 1) {
                        let _ = tx.blocking_send(Err("bufio.Scanner: token too long".into()));
                        return;
                    }
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                    if tx.blocking_send(Ok(buf)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(e.to_string()));
                    return;
                }
            }
        }
    });
    rx
}

/// The files of the zip by their normalized names.
async fn zip_files(archive: &Archive) -> HashMap<String, ZipRef> {
    let mut a = archive.clone();
    tokio::task::spawn_blocking(move || {
        let mut m = HashMap::new();
        for i in 0..a.len() {
            if let Ok(f) = a.by_index_raw(i) {
                m.insert(normalize_name(f.name()), ZipRef { name: f.name().to_string(), size: f.size() });
            }
        }
        m
    })
    .await
    .unwrap_or_default()
}

/// `bulkImport`: the line number and error that stopped it, or `(0, None)`.
#[allow(clippy::too_many_arguments)]
pub async fn bulk_import(
    app: &State,
    archive: &Archive,
    jsonl: &str,
    dry_run: bool,
    extract_content: bool,
    workers: usize,
    import_path: &str,
    o: &Opts,
    mut checkpoints: Option<&mut Checkpoints<'_>>,
) -> (usize, Option<IErr>) {
    let attached = zip_files(archive).await;
    let mut lines = read_lines(archive, jsonl);
    let mut line_number = 0usize;
    let mut deactivate_missing_users = false;
    let mut source_team = String::new();
    let mut source_team_inferred = false;
    let mut source_channel = String::new();
    let mut last_type = String::new();
    let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<(IErr, usize)>();
    let mut segment: Option<Segment> = None;
    let mut ctx: Option<Arc<Ctx>> = None;
    let mk_ctx = |deactivate: bool| {
        Arc::new(Ctx {
            app: app.clone(),
            archive: archive.clone(),
            dry_run,
            extract_content,
            deactivate_missing_users: deactivate,
            imported_users: o.imported_users.clone(),
            destination_team: o.destination_team.clone(),
            report: Report::default(),
        })
    };
    let mut scan_error: Option<String> = None;

    while let Some(raw) = lines.recv().await {
        let raw = match raw {
            Ok(r) => r,
            Err(e) => {
                scan_error = Some(e);
                break;
            }
        };
        line_number += 1;
        if line_number.is_multiple_of(8192) {
            crate::logs::info(&format!("Reader progress processed_lines={line_number}"), &[]);
        }
        let mut line: LineData = match decode_line(&raw) {
            Ok(l) => l,
            Err(text) => {
                if let Some(s) = segment.take() {
                    s.drain().await;
                }
                return (line_number, Some(IErr::bad("app.import.bulk_import.json_decode.error").wrap(text)));
            }
        };
        if let Err(e) = process_attachments(&mut line, import_path, &attached) {
            crate::logs::warn(
                &format!("Error while processing import attachments. Objects might be broken. err={e}"),
                &[],
            );
        }

        if line_number == 1 {
            if line.r#type != "version" || line.version.is_none() {
                return (
                    line_number,
                    Some(IErr::bad("app.import.process_import_data_file_version_line.invalid_version.error")),
                );
            }
            if line.version != Some(1) {
                return (line_number, Some(IErr::bad("app.import.bulk_import.unsupported_version.error")));
            }
            if let Some(add) = line.info.as_ref().and_then(|i| i.additional.clone()) {
                match crate::gojson::unmarshal_value(&add, &go_types::EXPORT_SCOPE)
                    .and_then(|v| serde_json::from_value::<ExportScope>(v).map_err(|e| e.to_string()))
                {
                    Err(e) => crate::logs::warn(
                        &format!("Failed to decode export scope metadata; proceeding as unscoped import err={e}"),
                        &[],
                    ),
                    Ok(scope) => {
                        source_team = scope.team_name.clone();
                        source_channel = scope.channel_name.clone();
                        if !o.destination_team.is_empty() && split_trim_names(&source_team).len() > 1 {
                            return (line_number, Some(IErr::bad("app.import.bulk_import.destination_team_requires_single_team_scope.error")
                                .detail("--destination-team-name requires a single-team export; this export contains multiple teams")));
                        }
                        if !o.destination_channel.is_empty() && split_trim_names(&source_channel).len() > 1 {
                            return (line_number, Some(IErr::bad("app.import.bulk_import.destination_channel_requires_single_channel_scope.error")
                                .detail("--destination-channel-name requires a single-channel export; this export contains multiple channels")));
                        }
                        if !scope.channel_name.is_empty() || !scope.team_name.is_empty() {
                            deactivate_missing_users = true;
                        }
                    }
                }
            }
            if !o.destination_channel.is_empty() && source_channel.is_empty() {
                return (
                    line_number,
                    Some(
                        IErr::bad("app.import.bulk_import.destination_channel_requires_channel_scope.error")
                            .detail("--destination-channel-name requires a channel-scoped export"),
                    ),
                );
            }
            if !o.destination_channel.is_empty() && split_trim_names(&source_team).len() != 1 {
                return (line_number, Some(IErr::bad("app.import.bulk_import.destination_channel_requires_single_team_scope.error")
                    .detail("--destination-channel-name requires an export scoped to exactly one team, otherwise the channel name is ambiguous")));
            }
            if deactivate_missing_users && o.imported_users != "active" && o.imported_users != "inactive" {
                return (line_number, Some(IErr::bad("app.import.bulk_import.imported_users_choice_required.error")));
            }
            let c = mk_ctx(deactivate_missing_users);
            if deactivate_missing_users {
                if let Err(e) = c.check_sso_provider_config(jsonl, o.skip_preflight).await {
                    return (line_number, Some(e));
                }
                let total = c.pre_create_sso_users(jsonl).await;
                if total > 0
                    && let Some(cp) = checkpoints.as_mut()
                {
                    cp.save(-(total as i64)).await;
                }
            }
            ctx = Some(c);
            last_type = line.r#type.clone();
            continue;
        }
        let Some(c) = ctx.clone() else { break };

        if o.resume_from_line > 0
            && line_number <= o.resume_from_line
            && (line.r#type == "post" || line.r#type == "direct_post")
        {
            continue;
        }

        if line.r#type != last_type {
            if let Some(s) = segment.take() {
                crate::logs::info(
                    &format!(
                        "Finished parsing segment, waiting for workers to finish old_segment={last_type} new_segment={}",
                        line.r#type
                    ),
                    &[],
                );
                s.drain().await;
                while let Ok((e, n)) = erx.try_recv() {
                    if stop_on_error(&e, deactivate_missing_users) {
                        return (n, Some(e));
                    }
                }
                if let Some(cp) = checkpoints.as_mut() {
                    cp.save((line_number - 1).max(o.resume_from_line) as i64).await;
                }
            }
            crate::logs::info(
                &format!(
                    "Starting workers for new segment old_segment={last_type} new_segment={} workers={workers}",
                    line.r#type
                ),
                &[],
            );
            last_type = line.r#type.clone();
            segment = Some(Segment::start(&c, workers, &etx));
        }

        if !o.destination_team.is_empty()
            && line.r#type == "team"
            && let Some(name) = line.team.as_ref().and_then(|t| t.name.clone())
        {
            if source_team.is_empty() {
                source_team = name;
                source_team_inferred = true;
            } else if source_team_inferred && name != source_team {
                if let Some(s) = segment.take() {
                    s.drain().await;
                }
                return (
                    line_number,
                    Some(IErr::bad("app.import.bulk_import.destination_team_requires_single_team_scope.error").detail(
                        "--destination-team-name requires a single-team export; this export contains multiple teams",
                    )),
                );
            }
        }
        if !o.destination_team.is_empty() && !source_team.is_empty() {
            rewrite_team_name(&mut line, &source_team, &o.destination_team);
        }
        if !o.destination_channel.is_empty() && !source_channel.is_empty() {
            rewrite_channel_name(&mut line, &source_channel, &o.destination_channel);
        }

        // Errors so far are looked at before each line is handed off; the line is never dropped.
        while let Ok((e, n)) = erx.try_recv() {
            if stop_on_error(&e, deactivate_missing_users) {
                if let Some(s) = segment.take() {
                    s.drain().await;
                }
                return (n, Some(e));
            }
        }
        let Some(s) = segment.as_ref() else { break };
        if s.tx.send(WorkLine { data: line, line: line_number }).await.is_err() {
            break;
        }
    }

    if let Some(s) = segment.take() {
        s.drain().await;
    }
    while let Ok((e, n)) = erx.try_recv() {
        if stop_on_error(&e, deactivate_missing_users) {
            return (n, Some(e));
        }
    }
    if let Some(e) = scan_error {
        return (0, Some(IErr::new("BulkImport", "app.import.bulk_import.file_scan.error", 500).wrap(e)));
    }
    if let Some(cp) = checkpoints.as_mut() {
        cp.save(line_number as i64).await;
    }
    (0, None)
}

/// `generatePassword`: an upper case letter, a digit, a lower case letter and a special character, then random
/// characters of all four up to the minimum length.
pub fn generate_password(min: usize) -> String {
    const SPECIAL: &str = "!$%^&*(),.";
    const NUMBERS: &str = "0123456789";
    const UPPER: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const LOWER: &str = "abcdefghijklmnopqrstuvwxyz";
    let all: String = [SPECIAL, NUMBERS, UPPER, LOWER].concat();
    let pick = |s: &str| {
        let b = s.as_bytes();
        b[(rand::random::<u64>() % b.len() as u64) as usize] as char
    };
    let mut p: String = [pick(UPPER), pick(NUMBERS), pick(LOWER), pick(SPECIAL)].into_iter().collect();
    while p.len() < min {
        p.push(pick(&all));
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_paths() {
        assert_eq!(attachment_path("a/b.png", "data").as_deref(), Some("data/a/b.png"));
        assert_eq!(attachment_path("/abs/b.png", "data").as_deref(), Some("data/abs/b.png"));
        assert_eq!(attachment_path("../x", "data"), None);
        assert_eq!(attachment_path("a/../../x", "data"), None);
        assert_eq!(attachment_path("", "data").as_deref(), Some("data"));
        assert_eq!(attachment_path("x", "").as_deref(), Some("x"));
    }

    #[test]
    fn root_jsonl() {
        assert!(is_root_jsonl("import.jsonl"));
        assert!(is_root_jsonl("./import.jsonl"));
        assert!(!is_root_jsonl("data/import.jsonl"));
        assert!(!is_root_jsonl("import.json"));
    }

    #[test]
    fn password() {
        let p = generate_password(10);
        assert_eq!(p.len(), 10);
        assert!(p.chars().next().unwrap().is_ascii_uppercase());
    }

    #[test]
    fn line_decoding() {
        let l: LineData = serde_json::from_str(r#"{"type":"user","user":{"username":"a","urgend_mention_count":1,"feature_enabled_markdown_preview":"true"}}"#).unwrap();
        assert_eq!(l.r#type, "user");
        assert_eq!(l.user.unwrap().use_markdown_preview.as_deref(), Some("true"));
        let l: LineData = serde_json::from_str(r#"{"type":null,"version":1}"#).unwrap();
        assert_eq!(l.r#type, "");
        let e = IErr::bad("app.import.import_line.unknown_line_type.error").p("Type", "x");
        assert!(e.text().starts_with("BulkImport: "), "{}", e.text());
    }

    /// Lines and what Go 1.26's `json.Unmarshal` into `LineImportData` says of them ("" when it decodes them).
    #[test]
    fn lines_decode_as_go() {
        let cases: &[(&str, &str)] = &[
            ("{\"type\":\"version\",\"version\":1}", ""),
            ("{\"Type\":\"version\",\"VERSION\":1}", ""),
            (
                "{\"type\":\"user\",\"User\":{\"Username\":\"a\",\"EMAIL\":\"a@x.org\",\"Teams\":[{\"NAME\":\"t\",\"Channels\":[{\"Name\":\"c\",\"Favorite\":true}]}]}}",
                "",
            ),
            (
                "{\"type\":\"user\",\"user\":{\"username\":5}}",
                "json: cannot unmarshal number into Go struct field UserImportData.user.username of type string",
            ),
            (
                "{\"type\":\"user\",\"user\":{\"teams\":[{\"name\":true}]}}",
                "json: cannot unmarshal bool into Go struct field UserTeamImportData.user.teams.name of type string",
            ),
            (
                "{\"type\":\"user\",\"user\":{\"teams\":\"x\"}}",
                "json: cannot unmarshal string into Go struct field UserImportData.user.teams of type []imports.UserTeamImportData",
            ),
            (
                "{\"type\":\"user\",\"user\":{\"delete_at\":1.5}}",
                "json: cannot unmarshal number 1.5 into Go struct field UserImportData.user.delete_at of type int64",
            ),
            (
                "{\"type\":\"user\",\"user\":{\"delete_at\":\"1\"}}",
                "json: cannot unmarshal string into Go struct field UserImportData.user.delete_at of type int64",
            ),
            (
                "{\"type\":\"user\",\"user\":{\"delete_at\":1e3}}",
                "json: cannot unmarshal number 1e3 into Go struct field UserImportData.user.delete_at of type int64",
            ),
            (
                "{\"type\":\"user\",\"user\":{\"custom_status\":{\"expires_at\":5}}}",
                "Time.UnmarshalJSON: input is not a JSON string",
            ),
            (
                "{\"type\":\"user\",\"user\":\"x\"}",
                "json: cannot unmarshal string into Go struct field LineImportData.user of type imports.UserImportData",
            ),
            (
                "{\"type\":\"post\",\"post\":{\"props\":[1]}}",
                "json: cannot unmarshal array into Go struct field PostImportData.post.props of type model.StringInterface",
            ),
            (
                "{\"type\":\"post\",\"post\":{\"reactions\":[{\"user\":1}]}}",
                "json: cannot unmarshal number into Go struct field ReactionImportData.post.reactions.user of type string",
            ),
            (
                "{\"type\":\"post\",\"post\":{\"flagged_by\":[1]}}",
                "json: cannot unmarshal number into Go struct field PostImportData.post.flagged_by of type string",
            ),
            (
                "{\"type\":\"direct_channel\",\"direct_channel\":{\"participants\":\"x\"}}",
                "json: cannot unmarshal string into Go struct field DirectChannelImportData.direct_channel.participants of type []*imports.DirectChannelMemberImportData",
            ),
            (
                "{\"type\":\"direct_channel\",\"direct_channel\":{\"participants\":[5]}}",
                "json: cannot unmarshal number into Go struct field DirectChannelImportData.direct_channel.participants of type imports.DirectChannelMemberImportData",
            ),
            (
                "{\"type\":\"channel\",\"channel\":{\"type\":1}}",
                "json: cannot unmarshal number into Go struct field ChannelImportData.channel.type of type model.ChannelType",
            ),
            (
                "{\"type\":\"version\",\"version\":\"1\"}",
                "json: cannot unmarshal string into Go struct field LineImportData.version of type int",
            ),
            (
                "{\"type\":\"version\",\"version\":1.0}",
                "json: cannot unmarshal number 1.0 into Go struct field LineImportData.version of type int",
            ),
            ("{\"type\":5}", "json: cannot unmarshal number into Go struct field LineImportData.type of type string"),
            ("[1]", "json: cannot unmarshal array into Go value of type imports.LineImportData"),
            ("\"x\"", "json: cannot unmarshal string into Go value of type imports.LineImportData"),
            ("x", "invalid character 'x' looking for beginning of value"),
            ("{\"type\":\"user\",}", "invalid character '}' looking for beginning of object key string"),
            ("{\"type\" \"user\"}", "invalid character '\"' after object key"),
            ("{\"type\":\"user\" \"x\":1}", "invalid character '\"' after object key:value pair"),
            ("[1 2]", "invalid character '2' after array element"),
            ("{\"a\":\"\\x\"}", "invalid character 'x' in string escape code"),
            ("{\"a\":\"\\u12g4\"}", "invalid character 'g' in \\u hexadecimal character escape"),
            ("{\"a\":-x}", "invalid character 'x' in numeric literal"),
            ("{\"a\":1.e}", "invalid character 'e' after decimal point in numeric literal"),
            ("{\"a\":1ex}", "invalid character 'x' in exponent of numeric literal"),
            ("{\"a\":tru}", "invalid character '}' in literal true (expecting 'e')"),
            ("{\"a\":nul}", "invalid character '}' in literal null (expecting 'l')"),
            ("{\"a\":", "unexpected end of JSON input"),
            ("{\"type\":\"user\"} x", "invalid character 'x' after top-level value"),
            ("{\"type\":\"user\",\"user\":{\"username\":\"a\",\"username\":\"b\",\"Username\":\"c\"}}", ""),
            (
                "{\"type\":\"role\",\"role\":{\"permissions\":[\"a\",null,\"b\"],\"scheme_managed\":\"yes\"}}",
                "json: cannot unmarshal string into Go struct field RoleImportData.role.scheme_managed of type bool",
            ),
            ("{\"type\":\"emoji\",\"emoji\":{\"name\":\"x\"},\"emoji\":{\"image\":\"y\"}}", ""),
            ("{\"type\":\"emoji\",\"emoji\":{\"name\":\"x\"},\"emoji\":null}", ""),
            (
                "{\"type\":\"scheme\",\"scheme\":{\"default_team_admin_role\":{\"name\":3}}}",
                "json: cannot unmarshal number into Go struct field RoleImportData.scheme.default_team_admin_role.name of type string",
            ),
            (
                "{\"type\":\"post\",\"post\":{\"replies\":[{\"attachments\":[{\"path\":2}]}]}}",
                "json: cannot unmarshal number into Go struct field AttachmentImportData.post.replies.attachments.path of type string",
            ),
            (
                "{\"type\":\"user\",\"user\":{\"notify_props\":{\"channel\":false}}}",
                "json: cannot unmarshal bool into Go struct field UserNotifyPropsImportData.user.notify_props.channel of type string",
            ),
            (
                "{\"type\":\"user\",\"user\":{\"profile_image\":7}}",
                "json: cannot unmarshal number into Go struct field UserImportData.user.Avatar.profile_image of type string",
            ),
            ("{\"type\":\"version\",\"version\":1,\"info\":{\"additional\":5}}", ""),
        ];
        for (line, want) in cases {
            let got = decode_line(line.as_bytes()).err().unwrap_or_default();
            assert_eq!(&got, want, "{line}");
        }
        let l = decode_line(
            br#"{"Type":"user","User":{"Username":"a","Teams":[{"NAME":"t","Channels":[{"Favorite":true}]}]}}"#,
        )
        .unwrap();
        let u = l.user.unwrap();
        assert_eq!((l.r#type.as_str(), u.username.as_deref()), ("user", Some("a")));
        assert_eq!(u.teams.unwrap()[0].channels.as_ref().unwrap()[0].favorite, Some(true));
        let l = decode_line(br#"{"type":"user","user":{"username":"a","username":"b","Username":"c"}}"#).unwrap();
        assert_eq!(l.user.unwrap().username.as_deref(), Some("c"));
        let l = decode_line(br#"{"type":"emoji","emoji":{"name":"x"},"emoji":{"image":"y"}}"#).unwrap();
        let e = l.emoji.unwrap();
        assert_eq!((e.name.as_deref(), e.image.as_deref()), (Some("x"), Some("y")));
        assert!(decode_line(br#"{"type":"emoji","emoji":{"name":"x"},"emoji":null}"#).unwrap().emoji.is_none());
    }
}
