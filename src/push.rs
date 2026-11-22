//! Mobile push notifications through the push proxy: message pushes for new posts, clear and badge pushes when
//! a user reads, the test push of `system/ping?device_id=`, and acks passed on. A bounded queue and a worker pool
//! stand for Go's `PushNotificationsHub`.

// Based on Mattermost server/channels/app/notification_push.go, notification.go (GetChannelName,
// GetSenderName, GetNotificationNameFormat) and public/model/push_notification.go, Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;
use serde::Serialize;
use tokio::sync::{Semaphore, mpsc};

use crate::app::{App, State, new_id, now_ms};
use crate::db::Table;
use crate::model::{Channel, Post, Session, StringMap, User, channel, status};

pub const CLEAR: &str = "clear";
pub const MESSAGE: &str = "message";
pub const UPDATE_BADGE: &str = "update_badge";
pub const TEST: &str = "test";
pub const SESSION: &str = "session";
const PROP_LAST_REMOVED_DEVICE: &str = "last_removed_device_id";
const PROP_LAST_REMOVED_VOIP_DEVICE: &str = "last_removed_voip_device_id";
pub const PROP_DEVICE_NOTIFICATION_DISABLED: &str = "device_notification_disabled";
const REMOVE_DEVICE: &str = "device was reported as removed";

/// `model.PushNotification` with its JSON names and `omitempty`s, in Go's field order.
#[derive(Serialize, Clone, Debug, Default)]
pub struct PushNotification {
    pub ack_id: String,
    pub platform: String,
    pub server_id: String,
    pub device_id: String,
    pub post_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub category: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sound: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub badge: i64,
    #[serde(rename = "cont_ava", skip_serializing_if = "is_zero")]
    pub content_available: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub team_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub channel_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub root_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub channel_name: String,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sub_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub transport: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sender_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sender_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub override_username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub override_icon_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub from_webhook: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    pub is_crt_enabled: bool,
    pub is_id_loaded: bool,
    pub signature: String,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

impl PushNotification {
    /// `SetDeviceIdAndPlatform`: `platform:id`.
    pub fn set_device(&mut self, device: &str) {
        if let Some((p, id)) = device.split_once(':') {
            self.platform = p.to_string();
            self.device_id = id.to_string();
        }
    }
}

/// `model.PushNotificationAck`.
#[derive(Serialize, serde::Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Ack {
    pub id: String,
    #[serde(rename = "received_at")]
    pub client_received_at: i64,
    #[serde(rename = "platform")]
    pub client_platform: String,
    #[serde(rename = "type")]
    pub notification_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub post_id: String,
    pub is_id_loaded: bool,
}

/// The Mattermost-hosted push servers, which need a license.
const HPNS: &[&str] = &[
    "https://push.mattermost.com",
    "https://hpns-de.mattermost.com",
    "https://global.push.mattermost.com",
    "https://us.push.mattermost.com",
    "https://eu.push.mattermost.com",
    "https://ap.push.mattermost.com",
];

/// `canSendPushNotifications`: on, and not a hosted server (no license here).
pub fn can_send(app: &App) -> bool {
    app.cfg.bool("EmailSettings.SendPushNotifications")
        && !HPNS.contains(&app.cfg.str("EmailSettings.PushNotificationServer"))
}

// ---------- names ----------

/// `GetNotificationNameFormat`.
pub async fn name_format(app: &App, user_id: &str) -> String {
    if !app.cfg.bool("PrivacySettings.ShowFullName") {
        return "username".into();
    }
    let pref: Option<String> = sqlx::query_scalar(
        "SELECT Value FROM Preferences WHERE UserId = $1 AND Category = 'display_settings' AND Name = 'name_format'",
    )
    .bind(user_id)
    .fetch_optional(&app.db)
    .await
    .ok()
    .flatten();
    pref.unwrap_or_else(|| app.cfg.str("TeamSettings.TeammateNameDisplay").to_string())
}

/// `GetDisplayNameWithPrefix`.
pub fn display_name_prefixed(u: &User, format: &str, prefix: &str) -> String {
    let base = format!("{prefix}{}", u.username);
    match format {
        "nickname_full_name" if !u.nickname.is_empty() => u.nickname.clone(),
        "nickname_full_name" | "full_name" if !u.full_name().is_empty() => u.full_name(),
        _ => base,
    }
}

/// `PostNotification`: what a post's notifications are made of.
#[derive(Clone)]
pub struct PostNotification {
    pub post: Post,
    pub channel: Channel,
    pub profiles: Arc<HashMap<String, Arc<User>>>,
    pub sender: Arc<User>,
}

impl PostNotification {
    /// `GetChannelName`.
    pub fn channel_name(&self, format: &str, exclude: &str) -> String {
        match self.channel.r#type.as_str() {
            channel::DIRECT => display_name_prefixed(&self.sender, format, "@"),
            channel::GROUP => {
                let mut names: Vec<String> = self
                    .profiles
                    .values()
                    .filter(|u| u.id != exclude)
                    .map(|u| display_name_prefixed(u, format, ""))
                    .collect();
                names.sort();
                names.join(", ")
            }
            _ => self.channel.display_name.clone(),
        }
    }
    /// `GetSenderName`.
    pub fn sender_name(&self, format: &str, overrides: bool) -> String {
        if self.post.is_system_message() {
            return crate::i18n::tl("en", "system.message.name", &[]);
        }
        if overrides
            && self.channel.r#type != channel::DIRECT
            && self.post.from_webhook()
            && let Some(serde_json::Value::String(s)) = self.post.prop("override_username")
        {
            return s.clone();
        }
        display_name_prefixed(&self.sender, format, "@")
    }
}

// ---------- the decision ----------

/// `doesNotifyPropsAllowPushNotification`: "" when allowed, the reason otherwise.
fn notify_props_allow(
    user: &User,
    cprops: Option<&StringMap>,
    post: &Post,
    mentioned: bool,
    is_gm: bool,
) -> &'static str {
    let cp = |k: &str| cprops.and_then(|p| p.0.get(k)).map(String::as_str).unwrap_or("");
    let user_notify = user.notify_prop("push");
    let mut channel_notify = cp("push");
    if channel_notify.is_empty() {
        channel_notify = "default";
    }
    let mut notify = channel_notify;
    if channel_notify == "default" {
        notify = user_notify;
        if is_gm && user_notify == "mention" {
            notify = "all";
        }
    }
    if cp("mark_unread") == "mention" {
        return "channel_muted";
    }
    let silent = post.prop("silent_notification") == Some(&serde_json::Value::Bool(true));
    if silent && !post.has_force_notification() {
        return "silent";
    }
    if post.is_system_message() {
        return "system_message";
    }
    if notify == "none" {
        return "level_set_to_none";
    }
    if notify == "mention" && !mentioned {
        return "not_mentioned";
    }
    ""
}

/// `doesStatusAllowPushNotification`.
pub fn status_allows(user: &User, st: &status::Status, channel_id: &str, crt: bool) -> bool {
    if st.status == status::DND || st.status == status::OUT_OF_OFFICE {
        return false;
    }
    let push_status = user.notify_props.0.get("push_status").map(String::as_str);
    let online_ok = st.active_channel != channel_id || now_ms() - st.last_activity_at > 20_000 || crt;
    if (push_status == Some(status::ONLINE) || push_status.is_none()) && online_ok {
        return true;
    }
    if push_status == Some(status::AWAY) && (st.status == status::AWAY || st.status == status::OFFLINE) {
        return true;
    }
    push_status == Some(status::OFFLINE) && st.status == status::OFFLINE
}

/// `ShouldSendPushNotification`.
pub fn should_send(
    user: &User,
    cprops: Option<&StringMap>,
    mentioned: bool,
    st: &status::Status,
    post: &Post,
    is_gm: bool,
) -> bool {
    if user.is_bot {
        return false;
    }
    match post.prop("force_notification") {
        None => {}
        Some(serde_json::Value::String(s)) if s.is_empty() => {}
        Some(serde_json::Value::Null) => {}
        Some(_) => return true,
    }
    notify_props_allow(user, cprops, post, mentioned, is_gm).is_empty()
        && status_allows(user, st, &post.channel_id, false)
}

/// `GetStatus`, offline when there is none.
pub async fn status_or_offline(app: &App, user_id: &str) -> status::Status {
    match app.status(user_id).await {
        Ok(Some(s)) => s,
        _ => status::Status { user_id: user_id.into(), status: status::OFFLINE.into(), ..Default::default() },
    }
}

// ---------- the hub ----------

enum Job {
    Message {
        n: Arc<PostNotification>,
        user: Arc<User>,
        sender_name: String,
        channel_name: String,
        explicit: bool,
        channel_wide: bool,
        reply_type: String,
    },
    Clear {
        session_id: String,
        user_id: String,
        channel_id: String,
        root_id: String,
    },
    Badge {
        user_id: String,
    },
}

static HUB: OnceLock<mpsc::Sender<Job>> = OnceLock::new();

/// `createPushNotificationsHub`: a queue of `PushNotificationBuffer` and `NumCPU * 8` workers.
pub fn spawn(app: State) {
    let buffer = app.cfg.i64("EmailSettings.PushNotificationBuffer").max(1) as usize;
    let (tx, mut rx) = mpsc::channel::<Job>(buffer);
    if HUB.set(tx).is_err() {
        return;
    }
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) * 8;
    let sema = Arc::new(Semaphore::new(workers));
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(permit) = sema.clone().acquire_owned().await else { return };
            let app = app.clone();
            tokio::spawn(async move {
                let r = match job {
                    Job::Message { n, user, sender_name, channel_name, explicit, channel_wide, reply_type } => {
                        send_message_sync(
                            &app,
                            &n,
                            &user,
                            &channel_name,
                            &sender_name,
                            explicit,
                            channel_wide,
                            &reply_type,
                        )
                        .await
                    }
                    Job::Clear { session_id, user_id, channel_id, root_id } => {
                        clear_sync(&app, &session_id, &user_id, &channel_id, &root_id).await
                    }
                    Job::Badge { user_id } => badge_sync(&app, &user_id).await,
                };
                if let Err(e) = r {
                    eprintln!("Unable to send push notification: {e}");
                }
                drop(permit);
            });
        }
    });
}

async fn enqueue(job: Job) {
    if let Some(tx) = HUB.get() {
        let _ = tx.send(job).await;
    }
}

/// `sendPushNotification`: the recipient's names, then queued.
pub async fn send_message(
    app: &App,
    n: &Arc<PostNotification>,
    user: &Arc<User>,
    explicit: bool,
    channel_wide: bool,
    reply_type: &str,
) {
    let format = name_format(app, &user.id).await;
    let channel_name = n.channel_name(&format, &user.id);
    let sender_name = n.sender_name(&format, app.cfg.bool("ServiceSettings.EnablePostUsernameOverride"));
    enqueue(Job::Message {
        n: n.clone(),
        user: user.clone(),
        sender_name,
        channel_name,
        explicit,
        channel_wide,
        reply_type: reply_type.to_string(),
    })
    .await;
}

/// `clearPushNotification`: the user read a channel (or a thread); the other devices drop its notifications.
/// Only when pushes can be sent (Go queues it regardless and posts to whatever server is configured).
pub async fn clear(app: &App, session_id: &str, user_id: &str, channel_id: &str, root_id: &str) {
    if !can_send(app) {
        return;
    }
    enqueue(Job::Clear {
        session_id: session_id.into(),
        user_id: user_id.into(),
        channel_id: channel_id.into(),
        root_id: root_id.into(),
    })
    .await;
}

/// `UpdateMobileAppBadge` (same condition as `clear`).
pub async fn update_badge(app: &App, user_id: &str) {
    if !can_send(app) {
        return;
    }
    enqueue(Job::Badge { user_id: user_id.into() }).await;
}

// ---------- building and sending ----------

/// `getUserBadgeCount`: mentions in channels (root mentions with collapsed threads) plus unread thread mentions.
async fn badge_count(app: &App, user_id: &str, crt: bool) -> Result<i64, String> {
    let col = if crt { "cm.MentionCountRoot" } else { "cm.MentionCount" };
    let q = format!(
        "SELECT COALESCE(SUM({col}), 0)::bigint FROM Channels c INNER JOIN ChannelMembers cm ON cm.ChannelId = c.Id
         AND cm.UserId = $1 AND c.DeleteAt = 0 WHERE c.Type NOT IN ('S')"
    );
    let mut n: i64 =
        sqlx::query_scalar(sqlx::AssertSqlSafe(q)).bind(user_id).fetch_one(&app.db).await.map_err(|e| e.to_string())?;
    if crt {
        let t: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(ThreadMemberships.UnreadMentions), 0)::bigint FROM ThreadMemberships
             LEFT JOIN Threads ON Threads.PostId = ThreadMemberships.PostId
             WHERE ThreadMemberships.UserId = $1 AND ThreadMemberships.Following = true
               AND (Threads.ThreadTeamId = '' OR EXISTS (SELECT 1 FROM ChannelMembers WHERE ChannelMembers.ChannelId = Threads.ChannelId
                    AND ChannelMembers.UserId = ThreadMemberships.UserId))
               AND COALESCE(Threads.ThreadDeleteAt, 0) = 0",
        )
        .bind(user_id)
        .fetch_one(&app.db)
        .await
        .map_err(|e| e.to_string())?;
        n += t;
    }
    Ok(n)
}

/// `getPushNotificationMessage`.
#[allow(clippy::too_many_arguments)]
fn message_text(
    contents: &str,
    msg: &str,
    explicit: bool,
    channel_wide: bool,
    has_files: bool,
    sender: &str,
    ctype: &str,
    reply: &str,
    locale: &str,
) -> String {
    let t = |id: &str| crate::i18n::tl(locale, id, &[]);
    if msg.is_empty() && has_files {
        if ctype == channel::DIRECT {
            return t("api.post.send_notifications_and_forget.push_image_only").trim_matches(' ').to_string();
        }
        return sender.to_string() + &t("api.post.send_notifications_and_forget.push_image_only");
    }
    let clear_tags = |s: &str| s.replace("<mention>", "").replace("</mention>", "");
    if contents == "full" {
        if ctype == channel::DIRECT && reply != "crt" {
            return clear_tags(msg);
        }
        return format!("{sender}: {}", clear_tags(msg));
    }
    if ctype == channel::DIRECT {
        if reply == "crt" {
            if contents == "generic_no_channel" {
                return sender.to_string() + &t("api.post.send_notification_and_forget.push_comment_on_crt_thread");
            }
            return sender.to_string() + &t("api.post.send_notification_and_forget.push_comment_on_crt_thread_dm");
        }
        return t("api.post.send_notifications_and_forget.push_message");
    }
    let suffix = if reply == "crt" {
        "api.post.send_notification_and_forget.push_comment_on_crt_thread"
    } else if channel_wide {
        "api.post.send_notification_and_forget.push_channel_mention"
    } else if explicit {
        "api.post.send_notifications_and_forget.push_explicit_mention"
    } else if reply == "root" {
        "api.post.send_notification_and_forget.push_comment_on_post"
    } else if reply == "any" {
        "api.post.send_notification_and_forget.push_comment_on_thread"
    } else if reply == "all" {
        "api.post.send_notification_and_forget.push_comment_on_crt_thread"
    } else {
        "api.post.send_notifications_and_forget.push_general_message"
    };
    sender.to_string() + &t(suffix)
}

/// `BuildPushNotificationMessage`: id-loaded needs a license, so it is generic here.
#[allow(clippy::too_many_arguments)]
pub async fn build_message(
    app: &App,
    post: &Post,
    user: &User,
    c: &Channel,
    channel_name: &str,
    sender_name: &str,
    explicit: bool,
    channel_wide: bool,
    reply: &str,
) -> Result<PushNotification, String> {
    let mut contents = app.cfg.str("EmailSettings.PushNotificationContents");
    if contents == "id_loaded" {
        contents = "generic";
    }
    let crt = app.is_crt_enabled(&user.id).await;
    let mut msg = PushNotification {
        category: "CAN_REPLY".into(),
        version: "v2".into(),
        kind: MESSAGE.into(),
        team_id: c.team_id.clone(),
        channel_id: c.id.clone(),
        post_id: post.id.clone(),
        root_id: post.root_id.clone(),
        sender_id: post.user_id.clone(),
        ..Default::default()
    };
    let locale = user.locale.as_str();
    if contents != "generic_no_channel" || c.r#type == channel::DIRECT {
        msg.channel_name = channel_name.to_string();
    }
    if crt {
        msg.is_crt_enabled = true;
        if !post.root_id.is_empty() && contents != "generic_no_channel" {
            msg.channel_name = crate::i18n::tl(
                locale,
                "api.push_notification.title.collapsed_threads",
                &[("channelName", channel_name)],
            );
            if c.r#type == channel::DIRECT {
                msg.channel_name = crate::i18n::tl(locale, "api.push_notification.title.collapsed_threads_dm", &[]);
            }
        }
    }
    msg.sender_name = sender_name.to_string();
    if let Some(serde_json::Value::String(ou)) = post.prop("override_username")
        && app.cfg.bool("ServiceSettings.EnablePostUsernameOverride")
    {
        msg.override_username = ou.clone();
        msg.sender_name = ou.clone();
    }
    if let Some(serde_json::Value::String(oi)) = post.prop("override_icon_url")
        && app.cfg.bool("ServiceSettings.EnablePostIconOverride")
    {
        msg.override_icon_url = oi.clone();
    }
    if let Some(serde_json::Value::String(fw)) = post.prop("from_webhook") {
        msg.from_webhook = fw.clone();
    }
    let mut text = crate::markdown::strip_and_decode(&post.message);
    for a in crate::integrations::attachments(&post.props.0) {
        if !a.fallback.is_empty() {
            text.push('\n');
            text.push_str(&a.fallback);
        }
    }
    msg.message = message_text(
        contents,
        &text,
        explicit,
        channel_wide,
        !post.file_ids.0.is_empty(),
        &msg.sender_name,
        &c.r#type,
        reply,
        locale,
    );
    msg.badge = badge_count(app, &user.id, crt).await?;
    Ok(msg)
}

#[allow(clippy::too_many_arguments)]
async fn send_message_sync(
    app: &App,
    n: &PostNotification,
    user: &User,
    channel_name: &str,
    sender_name: &str,
    explicit: bool,
    channel_wide: bool,
    reply: &str,
) -> Result<(), String> {
    let msg =
        build_message(app, &n.post, user, &n.channel, channel_name, sender_name, explicit, channel_wide, reply).await?;
    send_to_all_sessions(app, msg, &user.id, "").await
}

async fn clear_sync(app: &App, session_id: &str, user_id: &str, channel_id: &str, root_id: &str) -> Result<(), String> {
    let crt = app.is_crt_enabled(user_id).await;
    let badge = badge_count(app, user_id, crt).await?;
    let msg = PushNotification {
        kind: CLEAR.into(),
        version: "v2".into(),
        channel_id: channel_id.into(),
        root_id: root_id.into(),
        content_available: 1,
        badge,
        is_crt_enabled: crt,
        ..Default::default()
    };
    send_to_all_sessions(app, msg, user_id, session_id).await
}

async fn badge_sync(app: &App, user_id: &str) -> Result<(), String> {
    let crt = app.is_crt_enabled(user_id).await;
    let badge = badge_count(app, user_id, crt).await?;
    let msg = PushNotification {
        kind: UPDATE_BADGE.into(),
        version: "v2".into(),
        sound: "none".into(),
        content_available: 1,
        badge,
        ..Default::default()
    };
    send_to_all_sessions(app, msg, user_id, "").await
}

/// The ES256 JWT of `pushJWTClaims` (`ack_id`, `device_id`, and `user_id`/`iat` for a wipe).
async fn sign(app: &App, ack_id: &str, device_id: &str, wipe_user: Option<&str>) -> Result<String, String> {
    use p256::ecdsa::signature::Signer;
    let key = app.integration_keys().await.map_err(|e| e.detailed_error.clone())?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = b64.encode(r#"{"alg":"ES256","typ":"JWT"}"#);
    let claims = match wipe_user {
        None => serde_json::json!({"ack_id": ack_id, "device_id": device_id}).to_string(),
        Some(u) => format!(
            r#"{{"ack_id":{},"device_id":{},"user_id":{},"iat":{}}}"#,
            serde_json::to_string(ack_id).unwrap(),
            serde_json::to_string(device_id).unwrap(),
            serde_json::to_string(u).unwrap(),
            now_ms() / 1000
        ),
    };
    let input = format!("{header}.{}", b64.encode(claims));
    let sig: p256::ecdsa::Signature = key.signing.sign(input.as_bytes());
    Ok(format!("{input}.{}", b64.encode(sig.to_bytes())))
}

/// `GetSessionsWithActiveDeviceIds`.
async fn mobile_sessions(app: &App, user_id: &str) -> Result<Vec<Session>, String> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{} WHERE UserId = $1 AND ExpiresAt <> 0 AND ExpiresAt >= $2 AND (
           (DeviceId <> '' AND DeviceId != COALESCE(Props->>'last_removed_device_id', ''))
           OR (VoIPDeviceId <> '' AND VoIPDeviceId != COALESCE(Props->>'last_removed_voip_device_id', '')))",
        Session::SELECT
    )))
    .bind(user_id)
    .bind(now_ms())
    .fetch_all(&app.db)
    .await
    .map_err(|e| e.to_string())
}

/// `sendPushNotificationToAllSessions`: a copy per live device, each with its own ack id and signature.
pub async fn send_to_all_sessions(
    app: &App,
    msg: PushNotification,
    user_id: &str,
    skip_session: &str,
) -> Result<(), String> {
    let sessions = mobile_sessions(app, user_id).await?;
    for s in sessions {
        if s.is_expired(now_ms()) || (!skip_session.is_empty() && skip_session == s.id) {
            continue;
        }
        let mut m = msg.clone();
        let standard = !s.device_id.is_empty() && s.prop(PROP_LAST_REMOVED_DEVICE) != s.device_id;
        let voip = !s.voip_device_id.is_empty() && s.prop(PROP_LAST_REMOVED_VOIP_DEVICE) != s.voip_device_id;
        let device = if m.transport == "voip" {
            if voip {
                s.voip_device_id.clone()
            } else if standard {
                m.transport.clear();
                s.device_id.clone()
            } else {
                continue;
            }
        } else if standard {
            s.device_id.clone()
        } else {
            continue;
        };
        m.set_device(&device);
        m.ack_id = new_id();
        m.signature = match sign(app, &m.ack_id, &m.device_id, None).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Err(e) = send_to_proxy(app, &mut m, &s).await {
            eprintln!("Failed to send to push proxy: user_id={} session_id={} err={e}", s.user_id, s.id);
        }
    }
    Ok(())
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default())
}

/// `rawSendToPushProxy`: `POST <server>/api/v1/send_push`.
pub async fn raw_send(app: &App, msg: &PushNotification) -> Result<HashMap<String, String>, String> {
    let url = format!("{}/api/v1/send_push", app.cfg.str("EmailSettings.PushNotificationServer").trim_end_matches('/'));
    let body = serde_json::to_vec(msg).map_err(|e| format!("failed to encode to JSON: {e}"))?;
    let r = client().post(url).body(body).send().await.map_err(|e| e.to_string())?;
    if r.status() != reqwest::StatusCode::OK {
        return Err(format!("response returned error code: {}", r.status().as_u16()));
    }
    let b = r.bytes().await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&b).map_err(|e| format!("failed to decode from JSON: {e}"))
}

/// `sendToPushProxy`: a `REMOVE` answer retires the device on the session.
pub async fn send_to_proxy(app: &App, msg: &mut PushNotification, s: &Session) -> Result<(), String> {
    msg.server_id = app.cfg.diagnostic_id.clone();
    let r = raw_send(app, msg).await?;
    match r.get("status").map(String::as_str) {
        Some("REMOVE") => {
            let (prop, value) = if msg.transport == "voip" {
                (PROP_LAST_REMOVED_VOIP_DEVICE, s.voip_device_id.clone())
            } else {
                (PROP_LAST_REMOVED_DEVICE, s.device_id.clone())
            };
            set_session_props(app, s, &[(prop, &value)])
                .await
                .map_err(|e| format!("Failed to set extra session properties: {e}"))?;
            app.clear_sessions_for_user(&s.user_id);
            Err(REMOVE_DEVICE.into())
        }
        Some("FAIL") => Err(r.get("error").cloned().unwrap_or_default()),
        _ => Ok(()),
    }
}

/// `SetExtraSessionProps`: saved only when something changed.
pub async fn set_session_props(app: &App, s: &Session, props: &[(&str, &str)]) -> Result<(), String> {
    let mut p = s.props.clone();
    let mut changed = false;
    for (k, v) in props {
        if p.0.get(*k).map(String::as_str) != Some(*v) {
            p.0.insert(k.to_string(), v.to_string());
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let json = serde_json::to_string(&p.0).map_err(|e| e.to_string())?;
    sqlx::query("UPDATE Sessions SET Props = $1::jsonb WHERE Id = $2")
        .bind(json)
        .bind(&s.id)
        .execute(&app.db)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// `SendTestPushNotification`: "true", "false" or "unknown" for `system/ping?device_id=`.
pub async fn send_test(app: &App, device_id: &str) -> &'static str {
    if !can_send(app) {
        return "false";
    }
    let mut msg = PushNotification {
        version: "2".into(),
        kind: TEST.into(),
        server_id: app.cfg.diagnostic_id.clone(),
        badge: -1,
        ..Default::default()
    };
    msg.set_device(device_id);
    match raw_send(app, &msg).await {
        Err(_) => "unknown",
        Ok(r) => match r.get("status").map(String::as_str) {
            Some("REMOVE") => "false",
            Some("FAIL") => "unknown",
            _ => "true",
        },
    }
}

/// `SendAckToPushProxy`: `POST <server>/api/v1/ack`.
pub async fn send_ack(app: &App, ack: &Ack) -> Result<(), String> {
    let url = format!("{}/api/v1/ack", app.cfg.str("EmailSettings.PushNotificationServer").trim_end_matches('/'));
    let body = serde_json::to_vec(ack).map_err(|e| format!("failed to encode to JSON: {e}"))?;
    let r = client().post(url).body(body).send().await.map_err(|e| format!("failed to send: {e}"))?;
    if r.status() != reqwest::StatusCode::OK {
        return Err(format!("response returned error code: {}", r.status().as_u16()));
    }
    let _ = r.bytes().await;
    Ok(())
}

/// `sendMobileWipeSignal`: a silent `session` push to each revoked mobile session (ephemeral mode only).
pub async fn wipe(app: &App, sessions: &[Session]) {
    if !app.cfg.bool("MobileEphemeralModeSettings.Enable") || !can_send(app) {
        return;
    }
    for s in sessions {
        if s.device_id.is_empty() || s.device_id == s.prop(PROP_LAST_REMOVED_DEVICE) {
            continue;
        }
        let mut msg = PushNotification {
            version: "v2".into(),
            kind: SESSION.into(),
            content_available: 1,
            sound: "none".into(),
            ..Default::default()
        };
        msg.set_device(&s.device_id);
        msg.ack_id = new_id();
        let Ok(sig) = sign(app, &msg.ack_id, &msg.device_id, Some(&s.user_id)).await else { continue };
        msg.signature = sig;
        msg.server_id = app.cfg.diagnostic_id.clone();
        if let Err(e) = raw_send(app, &msg).await {
            eprintln!("Failed to send session wipe push: session_id={} err={e}", s.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_as_go() {
        let mut m = PushNotification {
            kind: CLEAR.into(),
            version: "v2".into(),
            content_available: 1,
            badge: 2,
            ..Default::default()
        };
        m.set_device("android_rn:abc");
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"ack_id":"","platform":"android_rn","server_id":"","device_id":"abc","post_id":"","badge":2,"cont_ava":1,"type":"clear","version":"v2","is_crt_enabled":false,"is_id_loaded":false,"signature":""}"#
        );
    }

    #[test]
    fn texts() {
        let t = message_text("full", "hi", false, false, false, "@a", "O", "", "en");
        assert_eq!(t, "@a: hi");
        let t = message_text("generic", "hi", true, false, false, "@a", "O", "", "en");
        assert_eq!(t, "@a mentioned you.");
        let t = message_text("generic", "hi", false, false, false, "@a", "D", "", "en");
        assert_eq!(t, "sent you a message.");
    }
}
