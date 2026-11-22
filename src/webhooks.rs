//! Incoming webhooks: `POST /hooks/{id}` (what bots and Jira post through) and the `/api/v4/hooks/incoming` API,
//! with the webhook cache.

// Based on Mattermost server/channels/web/webhook.go, app/webhook.go, app/slack.go, api4/webhook.go,
// store/localcachelayer/webhook_layer.go and model/incoming_webhook.go, Copyright (c) 2015-present Mattermost,
// Inc. Licensed under AGPL-3.0.

use std::sync::{Arc, OnceLock};

use crate::app::Query;
use axum::extract::{Path, State as St};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ebachat_macros::model;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::app::{ApiResult, App, AppError, Auth, Body, State, is_valid_id, now_ms};
use crate::db::{self, Table};
use crate::integrations::{MessageAttachment, form_decode, slack_links_to_markdown};
use crate::model::post::{PostMetadata, PostPriority};
use crate::model::{Channel, Post, Session, channel};
use crate::perm;
use crate::posts::CreateFlags;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/hooks/{hook_id}", post(incoming_webhook))
        .route("/api/v4/hooks/incoming", post(create_hook).get(list_hooks))
        .route("/api/v4/hooks/incoming/{hook_id}", get(get_hook).put(update_hook).delete(delete_hook))
}

const MANAGE_OWN: &str = "manage_own_incoming_webhooks";
const MANAGE_OTHERS: &str = "manage_others_incoming_webhooks";
const BYPASS_LOCK: &str = "bypass_incoming_webhook_channel_lock";
/// `PostPropsMaxUserRunes`.
const PROPS_MAX_RUNES: usize = 800_000 - 40_000;

#[model(table = "incomingwebhooks")]
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingWebhook {
    pub id: String,
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
    pub user_id: String,
    pub channel_id: String,
    pub team_id: String,
    pub display_name: String,
    pub description: String,
    pub username: String,
    pub icon_url: String,
    pub channel_locked: bool,
    pub last_used: i64,
}

impl IncomingWebhook {
    /// `IncomingWebhook.IsValid`.
    fn validate(&self) -> ApiResult<()> {
        let bad = |id: &str| Err(AppError::bad_request(id, ""));
        if !is_valid_id(&self.id) {
            return bad("model.incoming_hook.id.app_error");
        }
        if self.create_at == 0 {
            return bad("model.incoming_hook.create_at.app_error");
        }
        if self.update_at == 0 {
            return bad("model.incoming_hook.update_at.app_error");
        }
        if !is_valid_id(&self.user_id) {
            return bad("model.incoming_hook.user_id.app_error");
        }
        if !is_valid_id(&self.channel_id) {
            return bad("model.incoming_hook.channel_id.app_error");
        }
        if !is_valid_id(&self.team_id) {
            return bad("model.incoming_hook.team_id.app_error");
        }
        if self.display_name.len() > 64 {
            return bad("model.incoming_hook.display_name.app_error");
        }
        if self.description.len() > 500 {
            return bad("model.incoming_hook.description.app_error");
        }
        if self.username.len() > 64 {
            return bad("model.incoming_hook.username.app_error");
        }
        if self.icon_url.len() > 1024 {
            return bad("model.incoming_hook.icon_url.app_error");
        }
        Ok(())
    }
}

fn disabled() -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, "api.incoming_webhook.disabled.app_error", "")
}

impl App {
    fn hooks_on(&self) -> bool {
        self.cfg.bool("ServiceSettings.EnableIncomingWebhooks")
    }

    /// `WebhookStore.GetIncoming` through the webhook cache.
    pub async fn incoming_webhook(&self, id: &str) -> ApiResult<Option<Arc<IncomingWebhook>>> {
        if let Some(h) = self.caches.webhook.get(id) {
            return Ok(Some(h));
        }
        let sql = format!("{} WHERE Id = $1 AND DeleteAt = 0", IncomingWebhook::SELECT);
        let h: Option<IncomingWebhook> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(id).fetch_optional(&self.db).await?;
        Ok(h.map(|h| {
            let h = Arc::new(h);
            self.caches.webhook.put(id.to_string(), h.clone());
            h
        }))
    }

    /// `GetIncomingWebhook`.
    async fn get_incoming_webhook(&self, id: &str) -> ApiResult<Arc<IncomingWebhook>> {
        if !self.hooks_on() {
            return Err(disabled());
        }
        self.incoming_webhook(id).await?.ok_or_else(|| AppError::not_found("app.webhooks.get_incoming.app_error", ""))
    }
}

// ---------- POST /hooks/{id} ----------

/// `IncomingWebhookRequest`.
#[derive(Deserialize, Serialize, Default, Debug)]
#[serde(default)]
pub struct HookRequest {
    #[serde(deserialize_with = "crate::model::null_default")]
    text: String,
    #[serde(deserialize_with = "crate::model::null_default")]
    username: String,
    #[serde(deserialize_with = "crate::model::null_default")]
    icon_url: String,
    #[serde(rename = "channel", deserialize_with = "crate::model::null_default")]
    channel_name: String,
    #[serde(deserialize_with = "crate::model::null_default")]
    root_id: String,
    props: Option<Map<String, Value>>,
    attachments: Option<Vec<Option<MessageAttachment>>>,
    #[serde(rename = "type", deserialize_with = "crate::model::null_default")]
    kind: String,
    #[serde(deserialize_with = "crate::model::null_default")]
    icon_emoji: String,
    priority: Option<PostPriority>,
    #[serde(deserialize_with = "crate::model::null_default")]
    silent: bool,
}

/// `escapeControlCharsFromPayload`: raw newlines and tabs inside the usual string fields, escaped.
fn escape_control_chars(b: &[u8]) -> Vec<u8> {
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::bytes::Regex::new(r#"(?s)"(text|fallback|pretext|author_name|title|value)"\s*:\s*"(\\"|[^"])*""#)
            .unwrap()
    });
    re.replace_all(b, |c: &regex::bytes::Captures| {
        let m = c[0].to_vec();
        let s = String::from_utf8_lossy(&m).replace('\n', "\\n").replace('\t', "\\t");
        s.into_bytes()
    })
    .into_owned()
}

/// `IncomingWebhookRequestFromJSON`, then `StringifyMessageAttachmentFieldValue`.
fn decode_request(b: &[u8]) -> ApiResult<HookRequest> {
    let r: HookRequest = match serde_json::from_slice(b) {
        Ok(r) => r,
        Err(_) => serde_json::from_slice(&escape_control_chars(b))
            .map_err(|e| AppError::bad_request("model.incoming_hook.parse_data.app_error", "").detail(e.to_string()))?,
    };
    Ok(r)
}

/// `StringifyMessageAttachmentFieldValue`: nil attachments and fields dropped, field values as `%v` strings.
fn stringify_fields(list: Vec<Option<MessageAttachment>>) -> Vec<MessageAttachment> {
    list.into_iter()
        .flatten()
        .map(|mut a| {
            if let Some(fields) = a.fields.as_mut() {
                for f in fields.iter_mut() {
                    if !f.value.is_null() {
                        f.value = Value::String(go_sprint(&f.value));
                    }
                }
            }
            a
        })
        .collect()
}

/// Go's `fmt.Sprintf("%v", v)` of a decoded JSON value.
fn go_sprint(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{}", f as i64),
            Some(f) if f.abs() >= 1e21 || (f != 0.0 && f.abs() < 1e-4) => {
                let s = format!("{f:e}");
                // Go: 1e+21, 1.5e-05
                match s.split_once('e') {
                    Some((m, e)) => {
                        let (sign, digits) = if let Some(d) = e.strip_prefix('-') { ("-", d) } else { ("+", e) };
                        format!("{m}e{sign}{digits:0>2}")
                    }
                    None => s,
                }
            }
            Some(f) => f.to_string(),
            None => n.to_string(),
        },
        Value::Null => "<nil>".into(),
        Value::Array(a) => format!("[{}]", a.iter().map(go_sprint).collect::<Vec<_>>().join(" ")),
        Value::Object(m) => {
            format!("map[{}]", m.iter().map(|(k, v)| format!("{k}:{}", go_sprint(v))).collect::<Vec<_>>().join(" "))
        }
    }
}

fn hook_error(id: &str, e: AppError, hook_id: &str, media: &str) -> AppError {
    let status = StatusCode::from_u16(e.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    AppError::new(status, id, "")
        .with_params(&[("hook_id", hook_id), ("media_type", media)])
        .detail(format!("{}: {}", e.id, e.message))
}

/// `incomingWebhook`: JSON, a `payload=` form or a multipart form; `ok` as plain text.
async fn incoming_webhook(
    St(app): St<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    req: axum::extract::Request,
) -> Response {
    match handle(&app, &id, &headers, req).await {
        Ok(()) => {
            let mut r = "ok".into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
            r
        }
        Err(e) => e.into_response(),
    }
}

async fn handle(app: &State, id: &str, headers: &HeaderMap, req: axum::extract::Request) -> ApiResult<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(AppError::not_found("api.context.404.app_error", ""));
    }
    let ctype = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let media = ctype.split(';').next().unwrap_or("").trim().to_lowercase();
    let decode_err = |e: AppError| hook_error("web.incoming_webhook.decode.app_error", e, id, &media);
    let form_err = |id_: &str, e: String| {
        AppError::bad_request(id_, "").with_params(&[("hook_id", id), ("media_type", &media)]).detail(e)
    };
    // ServiceSettings.MaximumPayloadSizeBytes plus 512 bytes: a runaway payload is refused, not split into a
    // flood of posts. A form too large is 413, a JSON body that cannot be read whole a decode error.
    let max = usize::try_from(app.cfg.i64("ServiceSettings.MaximumPayloadSizeBytes")).unwrap_or(0).saturating_add(512);
    let body = axum::body::to_bytes(req.into_body(), max).await.map_err(|e| {
        if media == "application/x-www-form-urlencoded" || media == "multipart/form-data" {
            AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "api.context.request_body_too_large.app_error",
                "Unable to process request. Request body too large.",
            )
            .detail("Use the setting `MaximumPayloadSizeBytes` in the config to configure allowed payload limit.")
        } else {
            form_err("web.incoming_webhook.decode.app_error", e.to_string())
        }
    })?;
    let r = if media == "application/x-www-form-urlencoded" {
        let payload = form_value(&body, "payload").unwrap_or_default();
        decode_request(payload.as_bytes()).map_err(|e| AppError { status_code: 400, ..decode_err(e) })?
    } else if media == "multipart/form-data" {
        let boundary = multer::parse_boundary(&ctype)
            .map_err(|e| form_err("web.incoming_webhook.parse_multipart.app_error", e.to_string()))?;
        let mut mp =
            multer::Multipart::new(futures_util::stream::once(async move { Ok::<_, std::io::Error>(body) }), boundary);
        let mut form: Map<String, Value> = Map::new();
        while let Some(f) = mp
            .next_field()
            .await
            .map_err(|e| form_err("web.incoming_webhook.parse_multipart.app_error", e.to_string()))?
        {
            if f.file_name().is_some() {
                continue;
            }
            let name = f.name().unwrap_or("").to_lowercase();
            let text = f.text().await.unwrap_or_default();
            let key = match name.as_str() {
                "text" | "username" | "icon_url" | "root_id" | "type" | "icon_emoji" => name.clone(),
                "iconurl" => "icon_url".into(),
                "channel" | "channelname" => "channel".into(),
                "rootid" => "root_id".into(),
                "iconemoji" => "icon_emoji".into(),
                "silent" => {
                    form.insert("silent".into(), Value::Bool(text == "true" || text == "1" || text == "on"));
                    continue;
                }
                _ => continue,
            };
            form.insert(key, Value::String(text));
        }
        serde_json::from_value(Value::Object(form))
            .map_err(|e| form_err("web.incoming_webhook.decode.app_error", e.to_string()))?
    } else {
        decode_request(&body).map_err(decode_err)?
    };
    app.handle_incoming_webhook(id, r)
        .await
        .map_err(|e| hook_error("web.incoming_webhook.general.app_error", e, id, &media))
}

/// A field of an `application/x-www-form-urlencoded` body.
fn form_value(body: &[u8], key: &str) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    s.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (form_decode(k) == key).then(|| form_decode(v))
    })
}

/// `expandAnnouncement` and `replaceUserIds`.
async fn process_slack_text(app: &App, text: &str) -> String {
    let mut t = text.replace("<!channel>", "@channel").replace("<!here>", "@here").replace("<!all>", "@all");
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"<@([a-zA-Z0-9]+)>").unwrap());
    let ids: Vec<String> = re.captures_iter(&t).map(|c| c[1].to_string()).collect();
    if !ids.is_empty()
        && let Ok(users) = app.users(&ids).await
    {
        for u in users {
            t = t.replace(&format!("<@{}>", u.id), &format!("@{}", u.username));
        }
    }
    t
}

impl App {
    /// `HandleIncomingWebhook`.
    async fn handle_incoming_webhook(&self, hook_id: &str, req: HookRequest) -> ApiResult<()> {
        if !self.hooks_on() {
            return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "web.incoming_webhook.disabled.app_error", ""));
        }
        let mut req = req;
        let attachments = req.attachments.take().map(stringify_fields);
        let interactive = req.props.as_ref().is_some_and(|p| {
            ["attachments", "mm_blocks", "blocks", "cards"]
                .iter()
                .any(|k| p.get(*k).and_then(Value::as_array).is_some_and(|a| !a.is_empty()))
        });
        if req.text.is_empty() && attachments.is_none() && !interactive {
            return Err(AppError::bad_request("web.incoming_webhook.text.app_error", ""));
        }
        let hook = self
            .incoming_webhook(hook_id)
            .await?
            .ok_or_else(|| AppError::bad_request("web.incoming_webhook.invalid.app_error", ""))?;
        let mut props = req.props.take().unwrap_or_default();
        props.insert("webhook_display_name".into(), hook.display_name.clone().into());
        let text = process_slack_text(self, &req.text).await;
        let mut kind = req.kind.clone();
        let mut attachments = attachments.unwrap_or_default();
        // ProcessMessageAttachments
        for a in &mut attachments {
            a.pretext = process_slack_text(self, &a.pretext).await;
            a.text = process_slack_text(self, &a.text).await;
            a.title = process_slack_text(self, &a.title).await;
            if let Some(fields) = a.fields.as_mut() {
                for f in fields.iter_mut() {
                    if !f.value.is_null() {
                        f.value = Value::String(process_slack_text(self, &go_sprint(&f.value)).await);
                    }
                }
            }
        }
        let typed_attachments = if attachments.is_empty() {
            None
        } else {
            kind = "slack_attachment".into();
            Some(attachments)
        };

        let user = self.user(&hook.user_id).await?;
        let channel: Arc<Channel> = if req.channel_name.is_empty() {
            self.channel(&hook.channel_id)
                .await?
                .ok_or_else(|| AppError::not_found("app.channel.get.existing.app_error", ""))?
        } else if let Some(name) = req.channel_name.strip_prefix('@') {
            let target: Option<(String,)> =
                sqlx::query_as("SELECT Id FROM Users WHERE Username = $1").bind(name).fetch_optional(&self.db).await?;
            let Some((target,)) = target else {
                return Err(AppError::bad_request("web.incoming_webhook.user.app_error", "").with_param("user", name));
            };
            let restricted = self.cfg.str("TeamSettings.RestrictDirectMessage") == "team";
            if hook.user_id != target || restricted {
                let common: Vec<String> = sqlx::query_scalar(
                    "SELECT a.TeamId FROM TeamMembers a JOIN TeamMembers b ON a.TeamId = b.TeamId JOIN Teams t ON t.Id = a.TeamId
                     WHERE a.UserId = $1 AND b.UserId = $2 AND a.DeleteAt = 0 AND b.DeleteAt = 0 AND t.DeleteAt = 0",
                )
                .bind(&hook.user_id)
                .bind(&target)
                .fetch_all(&self.db)
                .await?;
                let allowed = if restricted { common.contains(&hook.team_id) } else { !common.is_empty() };
                if !allowed {
                    return Err(AppError::new(StatusCode::FORBIDDEN, "web.incoming_webhook.permissions.app_error", ""));
                }
            }
            let owner = user
                .clone()
                .ok_or_else(|| AppError::new(StatusCode::FORBIDDEN, "web.incoming_webhook.user.app_error", ""))?;
            let s = Session { user_id: owner.id.clone(), roles: owner.roles.clone(), ..Default::default() };
            Arc::new(self.get_or_create_direct(&s, &hook.user_id, &target).await?)
        } else {
            let name = req.channel_name.strip_prefix('#').unwrap_or(&req.channel_name);
            self.channel_by_name(&hook.team_id, name, false, true)
                .await?
                .ok_or_else(|| AppError::not_found("web.incoming_webhook.channel.app_error", ""))?
        };
        if hook.channel_locked && hook.channel_id != channel.id {
            return Err(AppError::new(StatusCode::FORBIDDEN, "web.incoming_webhook.channel_locked.app_error", ""));
        }
        let Some(user) = user else {
            return Err(AppError::new(StatusCode::FORBIDDEN, "web.incoming_webhook.user.app_error", "")
                .with_param("user", &hook.user_id));
        };
        if channel.r#type != channel::OPEN
            && !self.user_channel_perm(&user.id, &channel.id, perm::READ_CHANNEL_CONTENT).await.0
        {
            return Err(AppError::new(StatusCode::FORBIDDEN, "web.incoming_webhook.permissions.app_error", ""));
        }
        // checkIncomingWebhookDirectChannelTeamScope
        if self.cfg.str("TeamSettings.RestrictDirectMessage") == "team" && channel.is_group_or_direct() {
            let common: Vec<String> = sqlx::query_scalar(
                "SELECT t.Id FROM Teams t JOIN TeamMembers tm ON tm.TeamId = t.Id
                 WHERE tm.UserId IN (SELECT UserId FROM ChannelMembers WHERE ChannelId = $1) AND tm.DeleteAt = 0 AND t.DeleteAt = 0
                 GROUP BY t.Id HAVING COUNT(DISTINCT tm.UserId) = (SELECT COUNT(*) FROM ChannelMembers WHERE ChannelId = $1)",
            )
            .bind(&channel.id)
            .fetch_all(&self.db)
            .await?;
            if !common.contains(&hook.team_id) {
                return Err(AppError::new(StatusCode::FORBIDDEN, "web.incoming_webhook.permissions.app_error", ""));
            }
        }
        let mut root = String::new();
        if !req.root_id.is_empty() {
            if !is_valid_id(&req.root_id) {
                return Err(
                    AppError::bad_request("api.context.invalid_param.app_error", "").with_param("Name", "root_id")
                );
            }
            let rp = self
                .get_single_post(&req.root_id, false)
                .await?
                .ok_or_else(|| AppError::bad_request("api.post.create_post.root_id.app_error", ""))?;
            if rp.channel_id != channel.id {
                return Err(AppError::bad_request("api.post.create_post.channel_root_id.app_error", ""));
            }
            if !rp.root_id.is_empty() {
                return Err(AppError::bad_request("api.post.create_post.root_id.app_error", ""));
            }
            root = rp.id;
        }
        let username = if req.username.is_empty() { hook.username.clone() } else { req.username.clone() };
        let icon_url = if req.icon_url.is_empty() { hook.icon_url.clone() } else { req.icon_url.clone() };
        let hook_post = HookPost {
            user_id: hook.user_id.clone(),
            text,
            username,
            icon_url,
            icon_emoji: req.icon_emoji.clone(),
            props,
            attachments: typed_attachments,
            kind,
            root,
            priority: req.priority.take(),
            silent: req.silent,
        };
        self.create_webhook_post(&channel, hook_post).await?;
        // UpdateIncomingLastUsed: Go drops the cached hook; it keeps it with the new time instead.
        let now = now_ms();
        if sqlx::query("UPDATE IncomingWebhooks SET LastUsed = $1 WHERE Id = $2 AND DeleteAt = 0")
            .bind(now)
            .bind(&hook.id)
            .execute(&self.db)
            .await
            .is_ok()
        {
            let mut h = (*hook).clone();
            h.last_used = now;
            self.caches.webhook.put(h.id.clone(), Arc::new(h));
        }
        Ok(())
    }
}

/// What `CreateWebhookPost` takes.
pub struct HookPost {
    pub user_id: String,
    pub text: String,
    pub username: String,
    pub icon_url: String,
    pub icon_emoji: String,
    pub props: Map<String, Value>,
    pub attachments: Option<Vec<MessageAttachment>>,
    pub kind: String,
    pub root: String,
    pub priority: Option<PostPriority>,
    pub silent: bool,
}

fn rune_len(s: &str) -> usize {
    s.chars().count()
}

fn props_len(p: &Map<String, Value>) -> usize {
    rune_len(&serde_json::to_string(p).unwrap_or_default())
}

impl App {
    /// `CreateWebhookPost`: the post (split as Go's `splitWebhookPost` when too long), as from the webhook.
    pub async fn create_webhook_post(&self, channel: &Channel, h: HookPost) -> ApiResult<Post> {
        let mut post = Post {
            user_id: h.user_id.clone(),
            channel_id: channel.id.clone(),
            message: slack_links_to_markdown(&h.text),
            r#type: h.kind.clone(),
            root_id: h.root.clone(),
            ..Default::default()
        };
        post.props.0.insert("from_webhook".into(), "true".into());
        if let Some(p) = h.priority {
            if p.priority.is_none() {
                return Err(AppError::bad_request("api.context.invalid_param.app_error", "")
                    .with_param("Name", "webhook.priority.priority")
                    .detail("Setting the priority of a post is required to use priority."));
            }
            post.metadata = Some(PostMetadata { priority: Some(p), ..Default::default() });
        }
        if post.r#type.starts_with("system_") {
            return Err(
                AppError::bad_request("api.context.invalid_param.app_error", "").with_param("Name", "post.type")
            );
        }
        let mut flags = CreateFlags { silent: h.silent, from_incoming_webhook: true, ..Default::default() };
        if self.cfg.bool("ServiceSettings.EnablePostUsernameOverride") {
            flags.override_username = if h.username.is_empty() { "webhook".into() } else { h.username.clone() };
        }
        if self.cfg.bool("ServiceSettings.EnablePostIconOverride") {
            flags.override_icon_url = h.icon_url.clone();
            flags.override_icon_emoji = h.icon_emoji.clone();
        }
        if let Some(Value::String(d)) = h.props.get("webhook_display_name") {
            flags.webhook_display_name = d.clone();
        }
        let mut attachments: Option<Vec<MessageAttachment>> = None;
        for (k, v) in &h.props {
            match k.as_str() {
                // Raw attachments in props are not Go's typed ones: dropped, as there.
                "attachments"
                | "override_icon_url"
                | "override_icon_emoji"
                | "override_username"
                | "webhook_display_name"
                | "from_webhook" => {}
                _ => {
                    post.props.0.insert(k.clone(), v.clone());
                }
            }
        }
        if let Some(list) = h.attachments {
            // ParseMessageAttachment
            if post.r#type.is_empty() {
                post.r#type = "slack_attachment".into();
            }
            let parsed: Vec<MessageAttachment> = list
                .into_iter()
                .map(|mut a| {
                    a.text = slack_links_to_markdown(&a.text);
                    a.pretext = slack_links_to_markdown(&a.pretext);
                    if let Some(fields) = a.fields.as_mut() {
                        for f in fields.iter_mut() {
                            if let Value::String(s) = &f.value {
                                f.value = Value::String(slack_links_to_markdown(s));
                            }
                        }
                    }
                    a
                })
                .collect();
            attachments = Some(parsed);
        }
        if let Some(a) = &attachments {
            post.props.0.insert("attachments".into(), serde_json::to_value(a).unwrap_or(Value::Null));
        }
        let max = self.max_post_size().await as usize;
        let splits = split_webhook_post(&post, max, attachments.as_deref())?;
        let mut first = None;
        for split in splits {
            let f = CreateFlags {
                allow_mm_blocks_actions: split.props.0.contains_key("mm_blocks_actions"),
                ..flags.clone()
            };
            let created = self.create_post_in(split, channel, f, None).await.map_err(|e| {
                eprintln!("webhook post in {}: {} {}", channel.id, e.id, e.detailed_error);
                AppError::internal(
                    "api.post.create_webhook_post.creating.app_error",
                    format!("{}: {}", e.id, e.detailed_error),
                )
            })?;
            if first.is_none() {
                first = Some(created);
            }
        }
        Ok(first.unwrap_or_default())
    }
}

fn split_too_large() -> AppError {
    AppError::bad_request("web.incoming_webhook.split_props_length.app_error", "")
        .with_param("Max", &PROPS_MAX_RUNES.to_string())
}

/// `splitWebhookPost`: messages over `max` runes in several posts, attachments (and blocks) spread over them so
/// each post's props stay under `PostPropsMaxUserRunes`.
fn split_webhook_post(post: &Post, max: usize, attachments: Option<&[MessageAttachment]>) -> ApiResult<Vec<Post>> {
    if rune_len(&post.message) <= max && props_len(&post.props.0) <= PROPS_MAX_RUNES {
        return Ok(vec![post.clone()]);
    }
    const DEFERRED: [&str; 5] = ["attachments", "mm_blocks", "blocks", "cards", "mm_blocks_actions"];
    let mut base = post.clone();
    base.message.clear();
    base.props.0 = post
        .props
        .0
        .iter()
        .filter(|(k, _)| !DEFERRED.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Some(a) = post.props.0.get("mm_blocks_actions") {
        base.props.0.insert("mm_blocks_actions".into(), a.clone());
    }
    if props_len(&base.props.0) > PROPS_MAX_RUNES {
        return Err(split_too_large());
    }
    let mut splits = vec![];
    let mut rest: &str = &post.message;
    while rune_len(rest) > max {
        let cut = rest.char_indices().nth(max).map(|(i, _)| i).unwrap_or(rest.len());
        let mut s = base.clone();
        s.message = rest[..cut].to_string();
        rest = &rest[cut..];
        splits.push(s);
    }
    let mut s = base.clone();
    s.message = rest.to_string();
    splits.push(s);

    let mut typed: Vec<Vec<MessageAttachment>> = vec![vec![]; splits.len()];
    for a in attachments.unwrap_or_default() {
        let mut a = a.clone();
        loop {
            let last = typed.len() - 1;
            let mut candidate = typed[last].clone();
            candidate.push(a.clone());
            let mut props = splits[last].props.0.clone();
            props.insert("attachments".into(), serde_json::to_value(&candidate).unwrap());
            let n = props_len(&props);
            if n <= PROPS_MAX_RUNES {
                typed[last] = candidate;
                splits[last].props.0 = props;
                break;
            }
            if !typed[last].is_empty() {
                splits.push(base.clone());
                typed.push(vec![]);
                continue;
            }
            let need = n - PROPS_MAX_RUNES;
            let len = rune_len(&a.text);
            if len < need {
                return Err(split_too_large());
            }
            a.text = a.text.chars().take(len - need).collect();
            let mut props = splits[last].props.0.clone();
            props.insert("attachments".into(), serde_json::to_value(vec![a.clone()]).unwrap());
            typed[last] = vec![a.clone()];
            splits[last].props.0 = props;
            break;
        }
    }
    for key in ["mm_blocks", "blocks", "cards"] {
        let Some(Value::Array(items)) = post.props.0.get(key) else { continue };
        for item in items {
            loop {
                let last = splits.len() - 1;
                let mut props = splits[last].props.0.clone();
                let mut arr = props.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
                let had = !arr.is_empty();
                arr.push(item.clone());
                props.insert(key.into(), Value::Array(arr));
                if props_len(&props) <= PROPS_MAX_RUNES {
                    splits[last].props.0 = props;
                    break;
                }
                if had {
                    splits.push(base.clone());
                    continue;
                }
                return Err(split_too_large());
            }
        }
    }
    Ok(splits)
}

// ---------- /api/v4/hooks/incoming ----------

/// `createIncomingHook`.
async fn create_hook(St(app): St<State>, Auth(s): Auth, Body(mut hook): Body<IncomingWebhook>) -> ApiResult<Response> {
    let channel = app.channel_or_404(&hook.channel_id).await?;
    crate::audit::log(&app, Some(&s), "attempt").await;
    app.require_team(&s, &channel.team_id, MANAGE_OWN).await?;
    if !app.can_read_channel(&s, &channel).await?.0 {
        crate::audit::log(&app, Some(&s), "fail - bad channel permissions").await;
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    let mut owner = s.user_id.clone();
    if !hook.user_id.is_empty() && hook.user_id != s.user_id {
        if !app.has_team_perm(&s, &channel.team_id, MANAGE_OTHERS).await {
            crate::audit::log(&app, Some(&s), "fail - inappropriate permissions").await;
            return Err(AppError::forbidden_perm(MANAGE_OTHERS));
        }
        let u = app.user(&hook.user_id).await?.ok_or_else(crate::users::missing_account)?;
        if let Err(e) = validate_hook_user(&app, &s, &u.id, u.is_system_admin(), &channel).await {
            crate::audit::log(&app, Some(&s), "fail - invalid webhook user").await;
            return Err(e);
        }
        owner = hook.user_id.clone();
    }
    if !app.has_team_perm(&s, &channel.team_id, BYPASS_LOCK).await {
        hook.channel_locked = true;
        hook.channel_id = channel.id.clone();
    }
    // CreateIncomingWebhookForChannel
    if !app.hooks_on() {
        return Err(disabled());
    }
    hook.user_id = owner;
    hook.team_id = channel.team_id.clone();
    if !app.cfg.bool("ServiceSettings.EnablePostUsernameOverride") {
        hook.username.clear();
    }
    if !app.cfg.bool("ServiceSettings.EnablePostIconOverride") {
        hook.icon_url.clear();
    }
    if !hook.username.is_empty() && !crate::user_edit::valid_username(&hook.username) {
        return Err(AppError::bad_request("api.incoming_webhook.invalid_username.app_error", ""));
    }
    // SaveIncoming
    if !hook.id.is_empty() {
        return Err(AppError::bad_request("app.webhooks.save_incoming.existing.app_error", ""));
    }
    hook.id = crate::app::new_id();
    hook.create_at = now_ms();
    hook.update_at = hook.create_at;
    hook.validate()?;
    db::insert(&app.db, &hook).await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok((StatusCode::CREATED, Json(hook)).into_response())
}

/// `ValidateIncomingWebhookUser`.
async fn validate_hook_user(
    app: &App,
    s: &Session,
    user_id: &str,
    user_admin: bool,
    channel: &Channel,
) -> ApiResult<()> {
    if user_admin && !app.has_perm(s, perm::MANAGE_SYSTEM).await {
        return Err(AppError::new(StatusCode::FORBIDDEN, "api.webhook.incoming.user_role.app_error", ""));
    }
    validate_hook_channel_access(app, user_id, channel).await
}

async fn validate_hook_channel_access(app: &App, user_id: &str, channel: &Channel) -> ApiResult<()> {
    if !app.user_channel_perm(user_id, &channel.id, perm::READ_CHANNEL_CONTENT).await.0 {
        return Err(AppError::new(StatusCode::FORBIDDEN, "api.webhook.incoming.user_membership.app_error", ""));
    }
    Ok(())
}

/// `updateIncomingHook`.
async fn update_hook(
    St(app): St<State>,
    Auth(s): Auth,
    Path(hook_id): Path<String>,
    Body(mut updated): Body<IncomingWebhook>,
) -> ApiResult<Response> {
    if !is_valid_id(&hook_id) {
        return Err(AppError::invalid_url_param("hook_id"));
    }
    if updated.id != hook_id {
        return Err(AppError::invalid_param("hook_id"));
    }
    crate::audit::log(&app, Some(&s), "attempt").await;
    let old = app.get_incoming_webhook(&hook_id).await?;
    if updated.team_id.is_empty() {
        updated.team_id = old.team_id.clone();
    }
    if updated.team_id != old.team_id {
        return Err(AppError::bad_request("api.webhook.team_mismatch.app_error", ""));
    }
    let channel = app.channel_or_404(&updated.channel_id).await?;
    if channel.team_id != updated.team_id {
        return Err(AppError::invalid_param("channel_id"));
    }
    app.require_team(&s, &channel.team_id, MANAGE_OWN).await?;
    if s.user_id != old.user_id && !app.has_team_perm(&s, &channel.team_id, MANAGE_OTHERS).await {
        crate::audit::log(&app, Some(&s), "fail - inappropriate permissions").await;
        return Err(AppError::forbidden_perm(MANAGE_OTHERS));
    }
    if channel.r#type != channel::OPEN && !app.can_read_channel(&s, &channel).await?.0 {
        crate::audit::log(&app, Some(&s), "fail - bad channel permissions").await;
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    if updated.channel_id != old.channel_id
        && let Err(e) = validate_hook_channel_access(&app, &old.user_id, &channel).await
    {
        crate::audit::log(&app, Some(&s), "fail - invalid webhook user").await;
        return Err(e);
    }
    if !app.has_team_perm(&s, &channel.team_id, BYPASS_LOCK).await {
        updated.channel_locked = true;
        updated.channel_id = channel.id.clone();
    }
    // UpdateIncomingWebhook
    if !app.hooks_on() {
        return Err(disabled());
    }
    if !app.cfg.bool("ServiceSettings.EnablePostUsernameOverride") {
        updated.username = old.username.clone();
    }
    if !app.cfg.bool("ServiceSettings.EnablePostIconOverride") {
        updated.icon_url = old.icon_url.clone();
    }
    if !updated.username.is_empty() && !crate::user_edit::valid_username(&updated.username) {
        return Err(AppError::bad_request("api.incoming_webhook.invalid_username.app_error", ""));
    }
    updated.id = old.id.clone();
    updated.user_id = old.user_id.clone();
    updated.create_at = old.create_at;
    updated.team_id = old.team_id.clone();
    updated.delete_at = old.delete_at;
    updated.last_used = old.last_used;
    updated.update_at = now_ms();
    sqlx::query(
        "UPDATE IncomingWebhooks SET CreateAt=$1, UpdateAt=$2, DeleteAt=$3, ChannelId=$4, TeamId=$5, DisplayName=$6,
             Description=$7, Username=$8, IconURL=$9, ChannelLocked=$10 WHERE Id=$11",
    )
    .bind(updated.create_at)
    .bind(updated.update_at)
    .bind(updated.delete_at)
    .bind(&updated.channel_id)
    .bind(&updated.team_id)
    .bind(&updated.display_name)
    .bind(&updated.description)
    .bind(&updated.username)
    .bind(&updated.icon_url)
    .bind(updated.channel_locked)
    .bind(&updated.id)
    .execute(&app.db)
    .await
    .map_err(|e| AppError::internal("app.webhooks.update_incoming.app_error", e.to_string()))?;
    app.caches.webhook.remove(&old.id);
    crate::audit::log(&app, Some(&s), "success").await;
    Ok((StatusCode::CREATED, Json(updated)).into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ListQuery {
    team_id: String,
    page: Option<String>,
    per_page: Option<String>,
    include_total_count: Option<String>,
}

/// `getIncomingHooks`.
async fn list_hooks(St(app): St<State>, Auth(s): Auth, Query(q): Query<ListQuery>) -> ApiResult<Json<Value>> {
    let mut user_id = s.user_id.clone();
    if !q.team_id.is_empty() {
        app.require_team(&s, &q.team_id, MANAGE_OWN).await?;
        if app.has_team_perm(&s, &q.team_id, MANAGE_OTHERS).await {
            user_id.clear();
        }
    } else {
        app.require(&s, MANAGE_OWN).await?;
        if app.has_perm(&s, MANAGE_OTHERS).await {
            user_id.clear();
        }
    }
    if !app.hooks_on() {
        return Err(disabled());
    }
    let (page, per_page) = crate::posts::page_params(q.page.as_deref(), q.per_page.as_deref());
    let mut cond = vec!["DeleteAt = 0".to_string()];
    let mut args: Vec<String> = vec![];
    if !q.team_id.is_empty() {
        args.push(q.team_id.clone());
        cond.push(format!("TeamId = ${}", args.len()));
    }
    if !user_id.is_empty() {
        args.push(user_id.clone());
        cond.push(format!("UserId = ${}", args.len()));
    }
    let sql = format!(
        "{} WHERE {} ORDER BY DisplayName, Id LIMIT {per_page} OFFSET {}",
        IncomingWebhook::SELECT,
        cond.join(" AND "),
        page * per_page
    );
    let mut query = sqlx::query_as::<_, IncomingWebhook>(sqlx::AssertSqlSafe(sql));
    for a in &args {
        query = query.bind(a);
    }
    let hooks = query
        .fetch_all(&app.db)
        .await
        .map_err(|e| AppError::internal("app.webhooks.get_incoming_by_user.app_error", e.to_string()))?;
    if q.include_total_count.as_deref() == Some("true") {
        let sql = format!("SELECT COUNT(*) FROM IncomingWebhooks WHERE {}", cond.join(" AND "));
        let mut c = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql));
        for a in &args {
            c = c.bind(a);
        }
        let total = c.fetch_one(&app.db).await?;
        #[derive(Serialize)]
        struct WithCount {
            incoming_webhooks: Vec<IncomingWebhook>,
            total_count: i64,
        }
        return Ok(Json(serde_json::to_value(WithCount { incoming_webhooks: hooks, total_count: total }).unwrap()));
    }
    Ok(Json(serde_json::to_value(hooks).unwrap()))
}

/// `getIncomingHook`.
async fn get_hook(St(app): St<State>, Auth(s): Auth, Path(hook_id): Path<String>) -> ApiResult<Json<IncomingWebhook>> {
    let hook = checked_hook(&app, &s, &hook_id, true).await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(Json((*hook).clone()))
}

/// The hook the session may manage, with Go's audit rows (`attempt` only for a read).
async fn checked_hook(app: &App, s: &Session, hook_id: &str, attempt: bool) -> ApiResult<Arc<IncomingWebhook>> {
    if !is_valid_id(hook_id) {
        return Err(AppError::invalid_url_param("hook_id"));
    }
    let hook = app.get_incoming_webhook(hook_id).await?;
    if attempt {
        crate::audit::log(app, Some(s), "attempt").await;
    }
    let channel = app.channel_or_404(&hook.channel_id).await?;
    let restricted = channel.r#type != channel::OPEN && !app.can_read_channel(s, &channel).await?.0;
    if !app.has_team_perm(s, &hook.team_id, MANAGE_OWN).await || restricted {
        crate::audit::log(app, Some(s), "fail - bad permissions").await;
        return Err(AppError::forbidden_perm(MANAGE_OWN));
    }
    if s.user_id != hook.user_id && !app.has_team_perm(s, &hook.team_id, MANAGE_OTHERS).await {
        crate::audit::log(app, Some(s), "fail - inappropriate permissions").await;
        return Err(AppError::forbidden_perm(MANAGE_OTHERS));
    }
    Ok(hook)
}

/// `deleteIncomingHook`.
async fn delete_hook(St(app): St<State>, Auth(s): Auth, Path(hook_id): Path<String>) -> ApiResult<Json<Value>> {
    let hook = checked_hook(&app, &s, &hook_id, false).await?;
    if !app.hooks_on() {
        return Err(disabled());
    }
    let now = now_ms();
    sqlx::query("UPDATE IncomingWebhooks SET DeleteAt = $1, UpdateAt = $1 WHERE Id = $2")
        .bind(now)
        .bind(&hook.id)
        .execute(&app.db)
        .await
        .map_err(|e| AppError::internal("app.webhooks.delete_incoming.app_error", e.to_string()))?;
    app.caches.webhook.remove(&hook.id);
    Ok(crate::users::ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_sprint_numbers() {
        assert_eq!(go_sprint(&serde_json::json!(5)), "5");
        assert_eq!(go_sprint(&serde_json::json!(3.5)), "3.5");
        assert_eq!(go_sprint(&serde_json::json!(true)), "true");
    }

    #[test]
    fn splits_long_messages() {
        let p = Post { message: "a".repeat(25), ..Default::default() };
        let s = split_webhook_post(&p, 10, None).unwrap();
        assert_eq!(s.iter().map(|p| p.message.len()).collect::<Vec<_>>(), vec![10, 10, 5]);
    }

    #[test]
    fn escapes_raw_newlines() {
        let r = decode_request(b"{\"text\": \"a\nb\"}").unwrap();
        assert_eq!(r.text, "a\nb");
    }
}
