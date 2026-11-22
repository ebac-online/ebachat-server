//! Bot accounts: create, patch, list, enable/disable, assign, convert to a user.

// Based on Mattermost server/channels/api4/bot.go, app/bot.go, app/authorization.go
// (SessionHasPermissionToManageBot), app/user.go (ConvertBotToUser), model/bot.go and
// store/sqlstore/bot_store.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;

use crate::app::Query;
use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::app::{ApiResult, App, AppError, Auth, State, decode_json, is_valid_id, new_id, now_ms};
use crate::channels_api::{paging, parse_bool};
use crate::model::{Session, User};
use crate::perm::MANAGE_SYSTEM;
use crate::users::missing_account;

const VERSION: &str = crate::SERVER_VERSION;

const CREATE_BOT: &str = "create_bot";
const READ_BOTS: &str = "read_bots";
const READ_OTHERS_BOTS: &str = "read_others_bots";
const MANAGE_BOTS: &str = "manage_bots";
const MANAGE_OTHERS_BOTS: &str = "manage_others_bots";
const ASSIGN_BOT: &str = "assign_bot";

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/bots", post(create).get(list))
        .route("/api/v4/bots/{bot_user_id}", get(get_one).put(patch))
        .route("/api/v4/bots/{bot_user_id}/disable", post(disable))
        .route("/api/v4/bots/{bot_user_id}/enable", post(enable))
        .route("/api/v4/bots/{bot_user_id}/assign/{user_id}", post(assign))
        .route("/api/v4/bots/{bot_user_id}/convert_to_user", post(convert_to_user))
}

/// `model.Bot` with its `system_owned` (`MarshalJSON`).
#[derive(Serialize, Clone, Default, sqlx::FromRow)]
pub struct Bot {
    #[sqlx(default)]
    pub system_owned: bool,
    pub user_id: String,
    pub username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub owner_id: String,
    #[serde(skip_serializing_if = "crate::model::is_zero")]
    pub last_icon_update: i64,
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
}

/// `ProtectedBotUsernames`.
fn is_protected(username: &str) -> bool {
    matches!(username.to_lowercase().as_str(), "system-bot" | "content-review")
}

impl Bot {
    fn finish(mut self) -> Bot {
        self.system_owned = is_protected(&self.username);
        self
    }
    fn etag(&self) -> String {
        format!("{}.{}.{}", VERSION, self.user_id, self.update_at)
    }
}

/// `model.MakeBotNotFoundError`.
pub fn bot_not_found() -> AppError {
    AppError::not_found("store.sql_bot.get.missing.app_error", "Unable to find the existing bot.")
}

const SELECT: &str = "SELECT b.UserId AS user_id, u.Username AS username, u.FirstName AS display_name,
    COALESCE(b.Description, '') AS description, COALESCE(b.OwnerId, '') AS owner_id,
    COALESCE(b.LastIconUpdate, 0) AS last_icon_update, COALESCE(b.CreateAt, 0) AS create_at,
    COALESCE(b.UpdateAt, 0) AS update_at, COALESCE(b.DeleteAt, 0) AS delete_at
    FROM Bots b JOIN Users u ON u.Id = b.UserId";

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct BotPatch {
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

impl BotPatch {
    fn would_patch(&self, b: &Bot) -> bool {
        self.username.as_ref().is_some_and(|v| *v != b.username)
            || self.display_name.as_ref().is_some_and(|v| *v != b.display_name)
            || self.description.as_ref().is_some_and(|v| *v != b.description)
    }
    fn apply(self, b: &mut Bot) {
        if let Some(v) = self.username {
            b.username = v;
        }
        if let Some(v) = self.display_name {
            b.display_name = v;
        }
        if let Some(v) = self.description {
            b.description = v;
        }
    }
}

/// `Bot.IsValidCreate`.
fn check_bot(b: &Bot) -> ApiResult<()> {
    let bad = |f: &str| AppError::bad_request(&format!("model.bot.is_valid.{f}.app_error"), "Invalid bot.");
    if !crate::user_edit::valid_username(&b.username) {
        return Err(bad("username"));
    }
    if b.display_name.chars().count() > 64 {
        return Err(bad("user_id"));
    }
    if b.description.chars().count() > 1024 {
        return Err(bad("description"));
    }
    if b.owner_id.is_empty() || b.owner_id.chars().count() > 190 {
        return Err(bad("creator_id"));
    }
    Ok(())
}

impl App {
    /// `BotStore.Get`.
    pub async fn bot(&self, id: &str, include_deleted: bool) -> ApiResult<Bot> {
        let cond = if include_deleted { "" } else { " AND b.DeleteAt = 0" };
        let b: Option<Bot> = sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} WHERE b.UserId = $1{cond}")))
            .bind(id)
            .fetch_optional(&self.db)
            .await
            .map_err(|e| AppError::internal("app.bot.getbot.internal_error", e.to_string()))?;
        b.map(Bot::finish).ok_or_else(bot_not_found)
    }

    /// `SessionHasPermissionToManageBot`: one's own bots with `manage_bots`, others' with
    /// `manage_others_bots`; a bot the session may not even read is not found.
    pub async fn require_manage_bot(&self, s: &Session, bot_id: &str) -> ApiResult<Bot> {
        let b = self.bot(bot_id, true).await?;
        let (manage, read) =
            if b.owner_id == s.user_id { (MANAGE_BOTS, READ_BOTS) } else { (MANAGE_OTHERS_BOTS, READ_OTHERS_BOTS) };
        if !self.has_perm(s, manage).await {
            if !self.has_perm(s, read).await {
                return Err(bot_not_found());
            }
            return Err(AppError::forbidden_perm(manage));
        }
        Ok(b)
    }

    /// `BotStore.Save` (`PreSave`, `IsValid`) for an existing user row.
    pub async fn save_bot(&self, user_id: &str, username: &str, owner_id: &str, description: &str) -> ApiResult<Bot> {
        let now = now_ms();
        let b = Bot {
            user_id: user_id.into(),
            username: username.to_lowercase(),
            description: description.into(),
            owner_id: owner_id.into(),
            create_at: now,
            update_at: now,
            ..Default::default()
        };
        if !is_valid_id(&b.user_id) {
            return Err(AppError::bad_request("model.bot.is_valid.user_id.app_error", ""));
        }
        check_bot(&b)?;
        sqlx::query(
            "INSERT INTO Bots (UserId, Description, OwnerId, LastIconUpdate, CreateAt, UpdateAt, DeleteAt)
             VALUES ($1, $2, $3, 0, $4, $4, 0)",
        )
        .bind(&b.user_id)
        .bind(&b.description)
        .bind(&b.owner_id)
        .bind(now)
        .execute(&self.db)
        .await
        .map_err(|e| AppError::internal("app.bot.createbot.internal_error", e.to_string()))?;
        Ok(b.finish())
    }

    /// `BotStore.Update`: description, owner, icon and the times; the user fields stay the store's.
    pub(crate) async fn store_bot(&self, b: &Bot) -> ApiResult<Bot> {
        let mut b = b.clone();
        b.update_at = now_ms();
        check_bot(&b)?;
        let old = self.bot(&b.user_id, true).await?;
        sqlx::query(
            "UPDATE Bots SET Description = $1, OwnerId = $2, LastIconUpdate = $3, UpdateAt = $4, DeleteAt = $5
             WHERE UserId = $6",
        )
        .bind(&b.description)
        .bind(&b.owner_id)
        .bind(b.last_icon_update)
        .bind(b.update_at)
        .bind(b.delete_at)
        .bind(&b.user_id)
        .execute(&self.db)
        .await
        .map_err(|e| AppError::internal("app.bot.patchbot.internal_error", e.to_string()))?;
        self.invalidate_user(&b.user_id);
        Ok(Bot {
            description: b.description,
            owner_id: b.owner_id,
            last_icon_update: b.last_icon_update,
            update_at: b.update_at,
            delete_at: b.delete_at,
            ..old
        })
    }

    /// `UpdateBotActive`: the bot user (de)activated, then the bot row.
    pub async fn update_bot_active(&self, bot_id: &str, active: bool) -> ApiResult<Bot> {
        let mut b = self.bot(bot_id, true).await?;
        if b.system_owned && !active {
            return Err(AppError::forbidden().with_id("app.bot.update_bot_active.protected_bot.app_error"));
        }
        let user = self.user(bot_id).await?.ok_or_else(missing_account)?;
        self.update_active((*user).clone(), active).await?;
        let changed = if active && b.delete_at != 0 {
            b.delete_at = 0;
            true
        } else if !active && b.delete_at == 0 {
            b.delete_at = now_ms();
            true
        } else {
            false
        };
        if changed {
            b = self.store_bot(&b).await?;
        }
        Ok(b)
    }

    /// `CreateBot`: the bot's user row as `UserFromBot` makes it, the bot row, and the bot's DM with its owner
    /// carrying the "add to teams and channels" post.
    pub(crate) async fn create_bot(&self, s: &Session, mut b: Bot) -> ApiResult<Bot> {
        check_bot(&b)?;
        if is_protected(&b.username) {
            return Err(AppError::bad_request("app.bot.createbot.reserved_username.app_error", ""));
        }
        let now = now_ms();
        let username = crate::user_edit::sanitize_unicode(&b.username).to_lowercase();
        let u = User {
            id: new_id(),
            username: username.clone(),
            email: format!("{username}@localhost").to_lowercase(),
            first_name: crate::user_edit::sanitize_unicode(&b.display_name),
            roles: "system_user".into(),
            create_at: now,
            update_at: now,
            last_password_update: now,
            locale: "en".into(),
            // UserStore.Save straight from UserFromBot: no remote id (NULL), MfaUsedTimestamps null.
            remote_id: None,
            notify_props: sqlx::types::Json(crate::user_edit::default_notify_props()),
            timezone: sqlx::types::Json(
                [("automaticTimezone", ""), ("manualTimezone", ""), ("useAutomaticTimezone", "true")]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            ..Default::default()
        };
        crate::user_edit::check_user(&u)?;
        if let Err(e) = crate::db::insert(&self.db, &u).await {
            let m = e.to_string();
            let id = if m.contains("email") {
                "app.user.save.email_exists.app_error"
            } else if m.contains("username") {
                "app.user.save.username_exists.app_error"
            } else {
                return Err(AppError::internal("app.user.save.app_error", m));
            };
            return Err(AppError::bad_request(id, ""));
        }
        self.invalidate_user(&u.id);
        b.user_id = u.id.clone();
        let saved = match self.save_bot(&u.id, &b.username, &b.owner_id, &b.description).await {
            Ok(saved) => Bot { display_name: b.display_name.clone(), ..saved },
            Err(e) => {
                let _ = sqlx::query("DELETE FROM Users WHERE Id = $1").bind(&u.id).execute(&self.db).await;
                self.invalidate_user(&u.id);
                return Err(e);
            }
        };
        if let Some(owner) = self.user(&b.owner_id).await? {
            let c = self.get_or_create_direct(s, &u.id, &owner.id).await?;
            let post = crate::model::Post {
                user_id: saved.user_id.clone(),
                channel_id: c.id.clone(),
                message: crate::i18n::t("api.bot.teams_channels.add_message_mobile").unwrap_or_default().into(),
                r#type: "add_bot_teams_channels".into(),
                ..Default::default()
            };
            // CreatePostAsUser: a bot's post (`from_bot`) leaves its last view alone.
            self.create_post(post, true).await?;
        }
        Ok(saved)
    }
}

/// `createBot`.
async fn create(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Response> {
    let patch: Option<BotPatch> = serde_json::Deserializer::from_slice(&body)
        .into_iter::<Option<BotPatch>>()
        .next()
        .ok_or_else(|| AppError::invalid_param("bot").detail("EOF"))?
        .map_err(|e| AppError::invalid_param("bot").detail(e.to_string()))?;
    let patch = patch.ok_or_else(|| AppError::invalid_param("bot"))?;
    let mut b = Bot { owner_id: s.user_id.clone(), ..Default::default() };
    patch.apply(&mut b);
    app.require(&s, CREATE_BOT).await?;
    if app.user(&s.user_id).await?.is_some_and(|u| u.is_bot) {
        return Err(AppError::forbidden_perm(CREATE_BOT));
    }
    if !app.cfg.bool("ServiceSettings.EnableBotAccountCreation") {
        return Err(AppError::forbidden().with_id("api.bot.create_disabled"));
    }
    let b = app.create_bot(&s, b).await?;
    Ok((StatusCode::CREATED, Json(b)).into_response())
}

/// `patchBot`.
async fn patch(St(app): St<State>, Auth(s): Auth, Path(bot_id): Path<String>, body: Bytes) -> ApiResult<Json<Bot>> {
    if !is_valid_id(&bot_id) {
        return Err(AppError::invalid_url_param("bot_user_id"));
    }
    let patch: Option<BotPatch> = serde_json::Deserializer::from_slice(&body)
        .into_iter::<Option<BotPatch>>()
        .next()
        .ok_or_else(|| AppError::invalid_param("bot").detail("EOF"))?
        .map_err(|e| AppError::invalid_param("bot").detail(e.to_string()))?;
    let patch = patch.ok_or_else(|| AppError::invalid_param("bot"))?;
    app.require_manage_bot(&s, &bot_id).await?;
    Ok(Json(app.patch_bot(&bot_id, patch).await?))
}

impl App {
    /// A bot and its user patched.
    pub(crate) async fn patch_bot(&self, bot_id: &str, patch: BotPatch) -> ApiResult<Bot> {
        let app = self;
        let mut b = app.bot(bot_id, true).await?;
        if !patch.would_patch(&b) {
            return Ok(b);
        }
        let was_system = b.system_owned;
        patch.apply(&mut b);
        if !was_system && is_protected(&b.username) {
            return Err(AppError::bad_request("app.bot.patchbot.reserved_username.app_error", ""));
        }
        // UserStore.Update of the bot's user: name, email and display name only (no default picture redrawn, unlike
        // App.UpdateUser).
        let user = app.user(bot_id).await?.ok_or_else(missing_account)?;
        let mut u = (*user).clone();
        u.username = crate::user_edit::sanitize_unicode(&b.username).to_lowercase();
        u.email = format!("{}@localhost", b.username).to_lowercase();
        u.first_name = crate::user_edit::sanitize_unicode(&b.display_name);
        crate::user_edit::check_user(&u)?;
        let r = sqlx::query("UPDATE Users SET Username = $1, Email = $2, FirstName = $3, UpdateAt = $4 WHERE Id = $5")
            .bind(&u.username)
            .bind(&u.email)
            .bind(&u.first_name)
            .bind(now_ms())
            .bind(&u.id)
            .execute(&app.db)
            .await;
        if let Err(e) = r {
            let m = e.to_string();
            let id = if m.contains("username") {
                "app.user.save.username_exists.app_error"
            } else if m.contains("email") {
                "app.user.save.email_exists.app_error"
            } else {
                return Err(AppError::internal("app.user.update.finding.app_error", m));
            };
            return Err(AppError::bad_request(id, ""));
        }
        app.invalidate_user(&u.id);
        if let Some(fresh) = app.user(&u.id).await? {
            app.send_updated_user_event(&fresh);
        }
        app.store_bot(&b).await
    }
}

/// `getBot`: others' bots with `read_others_bots`, one's own with `read_bots`; otherwise not found.
async fn get_one(
    St(app): St<State>,
    Auth(s): Auth,
    headers: HeaderMap,
    Path(bot_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    if !is_valid_id(&bot_id) {
        return Err(AppError::invalid_url_param("bot_user_id"));
    }
    let b = app.bot(&bot_id, parse_bool(q.get("include_deleted"))).await?;
    if !app.has_perm(&s, READ_OTHERS_BOTS).await && !(b.owner_id == s.user_id && app.has_perm(&s, READ_BOTS).await) {
        return Err(bot_not_found());
    }
    let etag = b.etag();
    if let Some(r) = crate::teams::not_modified(&headers, &etag) {
        return Ok(r);
    }
    let mut r = Json(b).into_response();
    r.headers_mut().insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    Ok(r)
}

/// `getBots`: all bots with `read_others_bots`, one's own with `read_bots`.
async fn list(
    St(app): St<State>,
    Auth(s): Auth,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let owner = if app.has_perm(&s, READ_OTHERS_BOTS).await {
        String::new()
    } else if app.has_perm(&s, READ_BOTS).await {
        s.user_id.clone()
    } else {
        return Err(AppError::forbidden_perm(READ_BOTS));
    };
    let (page, per_page) = paging(&q);
    let mut conds: Vec<String> = vec![];
    let mut join = "";
    if !parse_bool(q.get("include_deleted")) {
        conds.push("b.DeleteAt = 0".into());
    }
    if !owner.is_empty() {
        conds.push("b.OwnerId = $3".into());
    }
    if parse_bool(q.get("only_orphaned")) {
        join = " JOIN Users o ON o.Id = b.OwnerId";
        conds.push("o.DeleteAt != 0".into());
    }
    let cond = if conds.is_empty() { String::new() } else { format!(" WHERE {}", conds.join(" AND ")) };
    let sql = format!("{SELECT}{join}{cond} ORDER BY b.CreateAt ASC, u.Username ASC LIMIT $1 OFFSET $2");
    let mut query = sqlx::query_as::<_, Bot>(sqlx::AssertSqlSafe(sql)).bind(per_page).bind(page * per_page);
    if !owner.is_empty() {
        query = query.bind(&owner);
    }
    let bots: Vec<Bot> = query
        .fetch_all(&app.db)
        .await
        .map_err(|e| AppError::internal("app.bot.getbots.internal_error", e.to_string()))?
        .into_iter()
        .map(Bot::finish)
        .collect();
    // BotList.Etag
    let (mut id, mut t) = ("0".to_string(), 0i64);
    for b in &bots {
        if b.update_at > t {
            t = b.update_at;
            id = b.user_id.clone();
        }
    }
    let etag = format!("{}.{id}.{t}.0.{}", VERSION, bots.len());
    if let Some(r) = crate::teams::not_modified(&headers, &etag) {
        return Ok(r);
    }
    let mut r = Json(bots).into_response();
    r.headers_mut().insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    Ok(r)
}

async fn set_active(app: &App, s: &Session, bot_id: &str, active: bool) -> ApiResult<Json<Bot>> {
    if !is_valid_id(bot_id) {
        return Err(AppError::invalid_url_param("bot_user_id"));
    }
    app.require_manage_bot(s, bot_id).await?;
    Ok(Json(app.update_bot_active(bot_id, active).await?))
}

/// `disableBot`.
async fn disable(St(app): St<State>, Auth(s): Auth, Path(bot_id): Path<String>) -> ApiResult<Json<Bot>> {
    set_active(&app, &s, &bot_id, false).await
}

/// `enableBot`.
async fn enable(St(app): St<State>, Auth(s): Auth, Path(bot_id): Path<String>) -> ApiResult<Json<Bot>> {
    set_active(&app, &s, &bot_id, true).await
}

/// `assignBot`: to another (human) owner.
async fn assign(
    St(app): St<State>,
    Auth(s): Auth,
    Path((bot_id, user_id)): Path<(String, String)>,
) -> ApiResult<Json<Bot>> {
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    if !is_valid_id(&bot_id) {
        return Err(AppError::invalid_url_param("bot_user_id"));
    }
    app.require_manage_bot(&s, &bot_id).await?;
    let user = app.user(&user_id).await?.ok_or_else(missing_account)?;
    if user.is_bot {
        return Err(AppError::forbidden_perm(ASSIGN_BOT));
    }
    let mut b = app.bot(&bot_id, true).await?;
    b.owner_id = user_id;
    Ok(Json(app.store_bot(&b).await?))
}

/// `convertBotToUser` (`ConvertBotToUser`): a password, optionally system admin, then the bot row goes. (Go
/// keeps the user cached as a bot; it is invalidated here.)
async fn convert_to_user(
    St(app): St<State>,
    Auth(s): Auth,
    Path(bot_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult<Json<User>> {
    if !is_valid_id(&bot_id) {
        return Err(AppError::invalid_url_param("bot_user_id"));
    }
    let b = app.bot(&bot_id, false).await?;
    let patch: crate::user_edit::UserPatch = decode_json(&body, "userPatch")?;
    let Some(password) = patch.password.clone().filter(|p| !p.is_empty()) else {
        return Err(AppError::invalid_param("userPatch"));
    };
    let admin = parse_bool(q.get("set_system_admin"));
    app.require(&s, MANAGE_SYSTEM).await?;
    let user = app.user(&b.user_id).await?.ok_or_else(missing_account)?;
    let mut user = (*user).clone();
    if admin && !user.is_system_admin() {
        let roles = format!("{} system_admin", user.roles);
        user = app.update_user_roles(user, &roles, false).await?;
        user = (*app.user(&user.id).await?.ok_or_else(missing_account)?).clone();
    }
    patch.apply(&mut user);
    let user = app.update_user(user, false, false).await?;
    let fresh = app.user(&user.id).await?.ok_or_else(missing_account)?;
    app.set_password(&fresh, &password, None).await?;
    sqlx::query("DELETE FROM Bots WHERE UserId = $1")
        .bind(&b.user_id)
        .execute(&app.db)
        .await
        .map_err(|e| AppError::internal("app.user.convert_bot_to_user.app_error", e.to_string()))?;
    app.invalidate_user(&b.user_id);
    let mut out = (*app.user(&b.user_id).await?.ok_or_else(missing_account)?).clone();
    out.sanitize_own();
    Ok(Json(out))
}
