//! Outgoing webhooks: their routes (create, list, read, update, delete, a new token), and firing them on new
//! posts in public channels (`handleWebhookEvents`): the matching hooks' callback URLs are called with the post,
//! and a response with text or attachments is posted back as the hook's creator.

// Based on Mattermost server/channels/api4/webhook.go, app/webhook.go (outgoing webhooks), app/post.go
// (handlePostEvents), public/model/outgoing_webhook.go and store/sqlstore/webhook_store.go, Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use crate::app::Query;
use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ebachat_macros::model;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::app::{ApiResult, App, AppError, Auth, State, is_valid_id, new_id, now_ms};
use crate::channels_api::paging;
use crate::command_admin::valid_http_url;
use crate::db::{self, Table};
use crate::model::{Channel, Post, PostPriority, Session, StringArray, User};
use crate::team_admin::decode;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/hooks/outgoing", post(create_hook).get(list_hooks))
        .route("/api/v4/hooks/outgoing/{hook_id}", get(get_hook).put(update_hook).delete(delete_hook))
        .route("/api/v4/hooks/outgoing/{hook_id}/regen_token", post(regen_token))
}

/// `model.OutgoingWebhook`.
#[model(table = "outgoingwebhooks")]
#[derive(Debug, Clone, PartialEq)]
pub struct OutgoingWebhook {
    pub id: String,
    pub token: String,
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
    pub creator_id: String,
    pub channel_id: String,
    pub team_id: String,
    pub trigger_words: StringArray,
    pub trigger_when: i32,
    #[mm(column = "callbackurls")]
    pub callback_urls: StringArray,
    pub display_name: String,
    pub description: String,
    pub content_type: String,
    pub username: String,
    #[mm(column = "iconurl")]
    pub icon_url: String,
}

/// `MaxIntegrationResponseSize`.
const MAX_INTEGRATION_RESPONSE: usize = 1024 * 1024;

const MANAGE_OWN: &str = "manage_own_outgoing_webhooks";
const MANAGE_OTHERS: &str = "manage_others_outgoing_webhooks";

fn bad(id: &str) -> AppError {
    AppError::bad_request(id, "")
}

fn forbidden(id: &str) -> AppError {
    AppError::new(StatusCode::FORBIDDEN, id, "")
}

fn disabled() -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, "api.outgoing_webhook.disabled.app_error", "")
}

/// `fmt.Sprintf("%s", list)`: `[a b c]`.
fn go_list_len(l: &[String]) -> usize {
    2 + l.iter().map(String::len).sum::<usize>() + l.len().saturating_sub(1)
}

impl OutgoingWebhook {
    /// `OutgoingWebhook.IsValid`.
    fn validate(&self) -> ApiResult<()> {
        let e = |f: &str| bad(&format!("model.outgoing_hook.is_valid.{f}.app_error"));
        if !is_valid_id(&self.id) {
            return Err(e("id"));
        }
        if self.token.len() != 26 {
            return Err(e("token"));
        }
        if self.create_at == 0 {
            return Err(e("create_at").detail(format!("id={}", self.id)));
        }
        if self.update_at == 0 {
            return Err(e("update_at").detail(format!("id={}", self.id)));
        }
        if !is_valid_id(&self.creator_id) {
            return Err(e("user_id"));
        }
        if !self.channel_id.is_empty() && !is_valid_id(&self.channel_id) {
            return Err(e("channel_id"));
        }
        if !is_valid_id(&self.team_id) {
            return Err(e("team_id"));
        }
        if go_list_len(&self.trigger_words.0) > 1024 {
            return Err(e("words"));
        }
        if self.trigger_words.0.iter().any(String::is_empty) {
            return Err(e("trigger_words"));
        }
        if self.callback_urls.0.is_empty() || go_list_len(&self.callback_urls.0) > 1024 {
            return Err(e("callback"));
        }
        if !self.callback_urls.0.iter().all(|u| valid_http_url(u)) {
            return Err(e("url"));
        }
        if self.display_name.len() > 64 {
            return Err(e("display_name"));
        }
        if self.description.len() > 500 {
            return Err(e("description"));
        }
        if self.content_type.len() > 128 || self.trigger_when > 1 {
            return Err(e("content_type"));
        }
        if self.username.len() > 64 {
            return Err(bad("model.outgoing_hook.username.app_error"));
        }
        if self.icon_url.len() > 1024 {
            return Err(bad("model.outgoing_hook.icon_url.app_error"));
        }
        Ok(())
    }

    /// `GetTriggerWord`: the first trigger word equal to the word, or (`starts_with`) the word starts with.
    fn trigger_word(&self, word: &str, starts_with: bool) -> Option<&str> {
        if word.is_empty() {
            return None;
        }
        self.trigger_words
            .0
            .iter()
            .find(|t| if starts_with { word.starts_with(t.as_str()) } else { t.as_str() == word })
            .map(String::as_str)
    }
}

impl App {
    fn outgoing_on(&self) -> ApiResult<()> {
        if self.cfg.bool("ServiceSettings.EnableOutgoingWebhooks") { Ok(()) } else { Err(disabled()) }
    }

    /// `GetOutgoingWebhook`.
    async fn outgoing_webhook(&self, id: &str) -> ApiResult<OutgoingWebhook> {
        self.outgoing_on()?;
        let sql = format!("{} WHERE Id = $1 AND DeleteAt = 0", OutgoingWebhook::SELECT);
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&self.db)
            .await
            .map_err(|e| AppError::internal("app.webhooks.get_outgoing.app_error", e.to_string()))?
            .ok_or_else(|| AppError::not_found("app.webhooks.get_outgoing.app_error", ""))
    }

    /// `GetOutgoingByTeam` (`...ByUser` with a creator), by display name.
    async fn outgoing_webhooks(
        &self,
        cond: &str,
        arg: &str,
        user: &str,
        page: Option<(i64, i64)>,
    ) -> sqlx::Result<Vec<OutgoingWebhook>> {
        let mut sql = format!("{} WHERE {cond} AND DeleteAt = 0", OutgoingWebhook::SELECT);
        if !user.is_empty() {
            sql += " AND CreatorId = $2";
        } else {
            sql += " AND $2::text IS NOT NULL";
        }
        sql += " ORDER BY DisplayName, Id";
        if let Some((page, per_page)) = page {
            sql += &format!(" LIMIT {per_page} OFFSET {}", page * per_page);
        }
        sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(arg).bind(user).fetch_all(&self.db).await
    }

    /// `ValidateOutgoingWebhookUser`: a system admin's hook only from a system admin; the hook's user must read
    /// the channel, or see the team.
    async fn validate_hook_user(
        &self,
        s: &Session,
        user: &User,
        team_id: &str,
        channel: Option<&Channel>,
    ) -> ApiResult<()> {
        if user.is_system_admin() && !self.has_perm(s, crate::perm::MANAGE_SYSTEM).await {
            return Err(forbidden("api.webhook.incoming.user_role.app_error").detail(format!("user_id={}", user.id)));
        }
        let ok = match channel {
            Some(c) => self.user_channel_perm(&user.id, &c.id, crate::perm::READ_CHANNEL_CONTENT).await.0,
            None => team_id.is_empty() || self.user_team_perm(&user.id, team_id, crate::perm::VIEW_TEAM).await,
        };
        if !ok {
            return Err(forbidden("api.webhook.incoming.user_membership.app_error"));
        }
        Ok(())
    }

    /// The trigger and callback overlap checks of `CreateOutgoingWebhook` / `UpdateOutgoingWebhook`.
    async fn check_overlap(&self, team_id: &str, h: &OutgoingWebhook, update: bool) -> ApiResult<()> {
        let all = self
            .outgoing_webhooks("TeamId = $1", team_id, "", None)
            .await
            .map_err(|e| AppError::internal("app.webhooks.get_outgoing_by_team.app_error", e.to_string()))?;
        for o in all {
            let urls = o.callback_urls.0.iter().any(|u| h.callback_urls.0.contains(u));
            let words = o.trigger_words.0.iter().any(|w| h.trigger_words.0.contains(w));
            if o.channel_id == h.channel_id && urls && words && (!update || o.id != h.id) {
                return Err(if update {
                    bad("api.webhook.update_outgoing.intersect.app_error")
                } else {
                    AppError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "api.webhook.create_outgoing.intersect.app_error",
                        "",
                    )
                });
            }
        }
        Ok(())
    }

    /// `SqlWebhookStore.UpdateOutgoing` (no validation, as Go's).
    async fn update_outgoing_row(&self, h: &mut OutgoingWebhook) -> ApiResult<()> {
        h.update_at = now_ms();
        db::update(&self.db, &*h)
            .await
            .map_err(|e| AppError::internal("app.webhooks.update_outgoing.app_error", e.to_string()))?;
        Ok(())
    }

    /// The hook and the checks the read, delete and regenerate routes share.
    async fn own_hook(&self, s: &Session, id: &str) -> ApiResult<OutgoingWebhook> {
        let h = self.outgoing_webhook(id).await?;
        crate::audit::log(self, Some(s), "attempt").await;
        if !self.has_team_perm(s, &h.team_id, MANAGE_OWN).await {
            return Err(AppError::forbidden_perm(MANAGE_OWN));
        }
        if s.user_id != h.creator_id && !self.has_team_perm(s, &h.team_id, MANAGE_OTHERS).await {
            crate::audit::log(self, Some(s), "fail - inappropriate permissions").await;
            return Err(AppError::forbidden_perm(MANAGE_OTHERS));
        }
        Ok(h)
    }
}

fn require_hook_id(id: &str) -> ApiResult<()> {
    if is_valid_id(id) { Ok(()) } else { Err(AppError::invalid_url_param("hook_id")) }
}

/// `createOutgoingHook`.
async fn create_hook(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Response> {
    let mut h: OutgoingWebhook = decode(&body, "outgoing_webhook")?;
    crate::audit::log(&app, Some(&s), "attempt").await;
    if !app.has_team_perm(&s, &h.team_id, MANAGE_OWN).await {
        return Err(AppError::forbidden_perm(MANAGE_OWN));
    }
    if h.creator_id.is_empty() {
        h.creator_id = s.user_id.clone();
    } else {
        if !app.has_team_perm(&s, &h.team_id, MANAGE_OTHERS).await {
            crate::audit::log(&app, Some(&s), "fail - inappropriate permissions").await;
            return Err(AppError::forbidden_perm(MANAGE_OTHERS));
        }
        let user = app.user(&h.creator_id).await?.ok_or_else(crate::users::missing_account)?;
        let channel = if h.channel_id.is_empty() { None } else { Some(app.get_channel(&h.channel_id).await?) };
        if let Err(e) = app.validate_hook_user(&s, &user, &h.team_id, channel.as_deref()).await {
            crate::audit::log(&app, Some(&s), "fail - invalid webhook user").await;
            return Err(e);
        }
    }
    match create_outgoing(&app, h).await {
        Ok(h) => {
            crate::audit::log(&app, Some(&s), "success").await;
            Ok((StatusCode::CREATED, Json(h)).into_response())
        }
        Err(e) => {
            crate::audit::log(&app, Some(&s), "fail").await;
            Err(e)
        }
    }
}

/// `CreateOutgoingWebhook`.
async fn create_outgoing(app: &App, mut h: OutgoingWebhook) -> ApiResult<OutgoingWebhook> {
    app.outgoing_on()?;
    if !h.channel_id.is_empty() {
        let c: Option<Channel> = db::get(&app.db, &h.channel_id).await?;
        let c = c.ok_or_else(|| AppError::not_found("app.channel.get.existing.app_error", ""))?;
        if c.r#type != "O" {
            return Err(forbidden("api.outgoing_webhook.disabled.app_error"));
        }
        if c.team_id != h.team_id {
            return Err(forbidden("api.webhook.create_outgoing.permissions.app_error"));
        }
    } else if h.trigger_words.0.is_empty() {
        return Err(bad("api.webhook.create_outgoing.triggers.app_error"));
    }
    app.check_overlap(&h.team_id.clone(), &h, false).await?;
    if !h.id.is_empty() {
        return Err(bad("app.webhooks.save_outgoing.override.app_error"));
    }
    h.id = new_id();
    if h.token.is_empty() {
        h.token = new_id();
    }
    h.create_at = now_ms();
    h.update_at = h.create_at;
    h.validate()?;
    db::insert(&app.db, &h)
        .await
        .map_err(|e| AppError::internal("app.webhooks.save_outgoing.app_error", e.to_string()))?;
    Ok(h)
}

/// `getOutgoingHooks`: of a channel, a team, or everywhere; only the session's own without `manage_others`.
async fn list_hooks(
    St(app): St<State>,
    Auth(s): Auth,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<OutgoingWebhook>>> {
    let channel_id = q.get("channel_id").map(String::as_str).unwrap_or("");
    let team_id = q.get("team_id").map(String::as_str).unwrap_or("");
    let paged = Some(paging(&q));
    let mut user = s.user_id.clone();
    let err = |e: sqlx::Error, id: &str| AppError::internal(id, e.to_string());
    let hooks = if !channel_id.is_empty() {
        if !app.channel_perm(&s, channel_id, MANAGE_OWN).await.map(|p| p.0).unwrap_or(false) {
            return Err(AppError::forbidden_perm(MANAGE_OWN));
        }
        if app.channel_perm(&s, channel_id, MANAGE_OTHERS).await.map(|p| p.0).unwrap_or(false) {
            user.clear();
        }
        app.outgoing_on()?;
        app.outgoing_webhooks("ChannelId = $1", channel_id, &user, paged)
            .await
            .map_err(|e| err(e, "app.webhooks.get_outgoing_by_channel.app_error"))?
    } else if !team_id.is_empty() {
        if !app.has_team_perm(&s, team_id, MANAGE_OWN).await {
            return Err(AppError::forbidden_perm(MANAGE_OWN));
        }
        if app.has_team_perm(&s, team_id, MANAGE_OTHERS).await {
            user.clear();
        }
        app.outgoing_on()?;
        app.outgoing_webhooks("TeamId = $1", team_id, &user, paged)
            .await
            .map_err(|e| err(e, "app.webhooks.get_outgoing_by_team.app_error"))?
    } else {
        if !app.has_perm(&s, MANAGE_OWN).await {
            return Err(AppError::forbidden_perm(MANAGE_OWN));
        }
        if app.has_perm(&s, MANAGE_OTHERS).await {
            user.clear();
        }
        app.outgoing_on()?;
        app.outgoing_webhooks("$1::text IS NOT NULL", "", &user, paged)
            .await
            .map_err(|e| err(e, "app.webhooks.get_outgoing_by_channel.app_error"))?
    };
    Ok(Json(hooks))
}

async fn get_hook(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<OutgoingWebhook>> {
    require_hook_id(&id)?;
    let h = app.own_hook(&s, &id).await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(Json(h))
}

/// `updateOutgoingHook`: the body over the hook; its creator, team and times kept.
async fn update_hook(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<OutgoingWebhook>> {
    require_hook_id(&id)?;
    let mut h: OutgoingWebhook = decode(&body, "outgoing_webhook")?;
    if h.id != id {
        return Err(AppError::invalid_param("hook_id"));
    }
    crate::audit::log(&app, Some(&s), "attempt").await;
    let old = app.outgoing_webhook(&id).await?;
    if h.team_id.is_empty() {
        h.team_id = old.team_id.clone();
    }
    if h.team_id != old.team_id {
        return Err(bad("api.webhook.team_mismatch.app_error").detail(format!("user_id={}", s.user_id)));
    }
    if !app.has_team_perm(&s, &h.team_id, MANAGE_OWN).await {
        return Err(AppError::forbidden_perm(MANAGE_OWN));
    }
    if s.user_id != old.creator_id && !app.has_team_perm(&s, &h.team_id, MANAGE_OTHERS).await {
        crate::audit::log(&app, Some(&s), "fail - inappropriate permissions").await;
        return Err(AppError::forbidden_perm(MANAGE_OTHERS));
    }
    if !h.channel_id.is_empty() && h.channel_id != old.channel_id {
        let c = app.get_channel(&h.channel_id).await?;
        let user = app.user(&old.creator_id).await?.ok_or_else(crate::users::missing_account)?;
        if let Err(e) = app.validate_hook_user(&s, &user, &h.team_id, Some(&c)).await {
            crate::audit::log(&app, Some(&s), "fail - invalid webhook user").await;
            return Err(e);
        }
    }
    // UpdateOutgoingWebhook
    app.outgoing_on()?;
    if !h.channel_id.is_empty() {
        let c = app.get_channel(&h.channel_id).await?;
        if c.r#type != "O" {
            return Err(forbidden("api.webhook.create_outgoing.not_open.app_error"));
        }
        if c.team_id != old.team_id {
            return Err(forbidden("api.webhook.create_outgoing.permissions.app_error"));
        }
    } else if h.trigger_words.0.is_empty() {
        return Err(AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api.webhook.create_outgoing.triggers.app_error",
            "",
        ));
    }
    app.check_overlap(&old.team_id, &h, true).await?;
    h.creator_id = old.creator_id;
    h.create_at = old.create_at;
    h.delete_at = old.delete_at;
    h.team_id = old.team_id;
    app.update_outgoing_row(&mut h).await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(Json(h))
}

async fn regen_token(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<OutgoingWebhook>> {
    require_hook_id(&id)?;
    let mut h = app.own_hook(&s, &id).await?;
    app.outgoing_on()?;
    h.token = new_id();
    app.update_outgoing_row(&mut h).await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(Json(h))
}

async fn delete_hook(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    require_hook_id(&id)?;
    let h = app.own_hook(&s, &id).await?;
    let deleted = match app.outgoing_on() {
        Err(e) => Err(e),
        Ok(()) => sqlx::query("UPDATE OutgoingWebhooks SET DeleteAt = $1, UpdateAt = $1 WHERE Id = $2")
            .bind(now_ms())
            .bind(&h.id)
            .execute(&app.db)
            .await
            .map(|_| ())
            .map_err(|e| AppError::internal("app.webhooks.delete_outgoing.app_error", e.to_string())),
    };
    if let Err(e) = deleted {
        crate::audit::log(&app, Some(&s), "fail").await;
        return Err(e);
    }
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(Json(json!({"status": "OK"})))
}

// ---------- firing ----------

/// A new post to check against the team's outgoing webhooks.
struct Event {
    post: Post,
    channel: Channel,
    username: String,
}

static QUEUE: OnceLock<mpsc::UnboundedSender<Event>> = OnceLock::new();

/// Starts the worker that fires outgoing webhooks (Go runs `handleWebhookEvents` in a goroutine per post).
pub fn spawn_worker(app: State) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    if QUEUE.set(tx).is_err() {
        return;
    }
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = app.handle_webhook_events(&ev).await {
                    eprintln!("outgoing webhooks for post {}: {} {}", ev.post.id, e.id, e.detailed_error);
                }
            });
        }
    });
}

/// `handlePostEvents`' webhook step, for a post created with `TriggerWebhooks`.
pub fn post_created(post: &Post, channel: &Channel, user: &User) {
    if channel.r#type != "O" || post.r#type == "burn_on_read" {
        return;
    }
    if let Some(q) = QUEUE.get() {
        let _ = q.send(Event { post: post.clone(), channel: channel.clone(), username: user.username.clone() });
    }
}

/// `model.OutgoingWebhookPayload`.
#[derive(serde::Serialize)]
struct Payload<'a> {
    token: &'a str,
    team_id: &'a str,
    team_domain: &'a str,
    channel_id: &'a str,
    channel_name: &'a str,
    timestamp: i64,
    user_id: &'a str,
    user_name: &'a str,
    post_id: &'a str,
    text: &'a str,
    trigger_word: &'a str,
    file_ids: String,
}

impl Payload<'_> {
    /// `ToFormValues`: keys sorted, as Go's `url.Values.Encode`, the time in seconds.
    fn form(&self) -> String {
        let mut v = vec![
            ("channel_id", self.channel_id.to_string()),
            ("channel_name", self.channel_name.to_string()),
            ("file_ids", self.file_ids.clone()),
            ("post_id", self.post_id.to_string()),
            ("team_domain", self.team_domain.to_string()),
            ("team_id", self.team_id.to_string()),
            ("text", self.text.to_string()),
            ("timestamp", (self.timestamp / 1000).to_string()),
            ("token", self.token.to_string()),
            ("trigger_word", self.trigger_word.to_string()),
            ("user_id", self.user_id.to_string()),
            ("user_name", self.user_name.to_string()),
        ];
        v.sort_by(|a, b| a.0.cmp(b.0));
        v.iter().map(|(k, val)| format!("{k}={}", query_escape(val))).collect::<Vec<_>>().join("&")
    }
}

/// Go's `url.QueryEscape`.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `model.OutgoingWebhookResponse`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct HookResponse {
    text: Option<String>,
    username: Option<String>,
    icon_url: Option<String>,
    props: Option<Map<String, Value>>,
    attachments: Option<Vec<Value>>,
    #[serde(rename = "type")]
    kind: Option<String>,
    response_type: Option<String>,
    priority: Option<PostPriority>,
}

impl App {
    /// `handleWebhookEvents`: the hooks of the channel (any message, or with a trigger word) and of the team
    /// (trigger words), each called with its own matching trigger word (Go sends every hook the last matching
    /// hook's word; fixed here).
    async fn handle_webhook_events(&self, ev: &Event) -> ApiResult<()> {
        if !self.cfg.bool("ServiceSettings.EnableOutgoingWebhooks") {
            return Ok(());
        }
        let team = self.team(&ev.channel.team_id).await?;
        let hooks = self
            .outgoing_webhooks("TeamId = $1", &team.id, "", None)
            .await
            .map_err(|e| AppError::internal("app.webhooks.get_outgoing_by_team.app_error", e.to_string()))?;
        let first = ev.post.message.split_whitespace().next().unwrap_or("");
        let mut calls = vec![];
        for h in &hooks {
            if h.channel_id != ev.post.channel_id && !h.channel_id.is_empty() {
                continue;
            }
            let word = if h.channel_id == ev.post.channel_id && h.trigger_words.0.is_empty() {
                Some("")
            } else if h.trigger_when == 0 {
                h.trigger_word(first, false)
            } else if h.trigger_when == 1 {
                h.trigger_word(first, true)
            } else {
                None
            };
            let Some(word) = word else { continue };
            let payload = Payload {
                token: &h.token,
                team_id: &h.team_id,
                team_domain: &team.name,
                channel_id: &ev.post.channel_id,
                channel_name: &ev.channel.name,
                timestamp: ev.post.create_at,
                user_id: &ev.post.user_id,
                user_name: &ev.username,
                post_id: &ev.post.id,
                text: &ev.post.message,
                trigger_word: word,
                file_ids: ev.post.file_ids.0.join(","),
            };
            let (body, content_type) = if h.content_type == "application/json" {
                (serde_json::to_vec(&payload).unwrap_or_default(), "application/json")
            } else {
                (payload.form().into_bytes(), "application/x-www-form-urlencoded")
            };
            for url in &h.callback_urls.0 {
                calls.push(self.trigger_webhook(h, url, body.clone(), content_type, ev));
            }
        }
        futures_util::future::join_all(calls).await;
        Ok(())
    }

    /// One callback of `TriggerWebhook`: the response's text and attachments posted as the hook's creator, as a
    /// reply when `response_type` is `comment`.
    async fn trigger_webhook(&self, h: &OutgoingWebhook, url: &str, body: Vec<u8>, content_type: &str, ev: &Event) {
        let resp = match self.outgoing_request(url, body, content_type).await {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                eprintln!("outgoing webhook {} to {url}: {e}", h.id);
                return;
            }
        };
        let attachments = resp.attachments.unwrap_or_default();
        if resp.text.is_none() && attachments.is_empty() {
            return;
        }
        let root = if resp.response_type.as_deref() == Some("comment") { ev.post.id.clone() } else { String::new() };
        let mut props = resp.props.unwrap_or_default();
        props.insert("webhook_display_name".into(), Value::String(h.display_name.clone()));
        let text = match &resp.text {
            Some(t) => self.process_slack_text(t).await,
            None => String::new(),
        };
        let attachments = self.process_message_attachments(attachments).await;
        let typed: Option<Vec<crate::integrations::MessageAttachment>> =
            if attachments.is_empty() { None } else { serde_json::from_value(Value::Array(attachments)).ok() };
        let mut username = resp.username.unwrap_or_default();
        if self.cfg.bool("ServiceSettings.EnablePostUsernameOverride") && !h.username.is_empty() && username.is_empty()
        {
            username = h.username.clone();
        }
        let mut icon_url = resp.icon_url.unwrap_or_default();
        if self.cfg.bool("ServiceSettings.EnablePostIconOverride") && !h.icon_url.is_empty() && icon_url.is_empty() {
            icon_url = h.icon_url.clone();
        }
        let hp = crate::webhooks::HookPost {
            user_id: h.creator_id.clone(),
            text,
            username,
            icon_url,
            icon_emoji: String::new(),
            props,
            attachments: typed,
            kind: resp.kind.unwrap_or_default(),
            root,
            priority: resp.priority,
            silent: false,
        };
        if let Err(e) = self.create_webhook_post(&ev.channel, hp).await {
            eprintln!("outgoing webhook {} response post: {} {}", h.id, e.id, e.detailed_error);
        }
    }

    /// `doOutgoingWebhookRequest`: POST, at most `OutgoingIntegrationRequestsTimeout`; an empty answer is none.
    async fn outgoing_request(
        &self,
        url: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<Option<HookResponse>, String> {
        let u = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
        if !self.untrusted_url_allowed(&u) {
            return Err("address forbidden, you may need to set AllowedUntrustedInternalConnections to allow an integration access to your internal network".into());
        }
        let timeout =
            Duration::from_secs(self.cfg.i64("ServiceSettings.OutgoingIntegrationRequestsTimeout").max(1) as u64);
        let resp = self
            .integration_client()
            .post(u)
            .timeout(timeout)
            .header("Content-Type", content_type)
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let mut data = vec![];
        let mut stream = resp;
        while let Some(chunk) = stream.chunk().await.map_err(|e| e.to_string())? {
            data.extend_from_slice(&chunk);
            if data.len() >= MAX_INTEGRATION_RESPONSE {
                data.truncate(MAX_INTEGRATION_RESPONSE);
                break;
            }
        }
        let mut it = serde_json::Deserializer::from_slice(&data).into_iter::<HookResponse>();
        match it.next() {
            None => Ok(None),
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(format!("api.unmarshal_error: {e}")),
        }
    }
}
