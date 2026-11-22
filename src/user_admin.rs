//! Managing users: update, deactivate, roles, auth, MFA, sessions, audits, email verification and password reset
//! tokens, guests, terms of service, account-type switches and the endpoints a server without a license answers.

// Based on Mattermost server/channels/api4/user.go, api4/notify_admin.go, api4/custom_profile_attributes.go,
// api4/data_retention.go, api4/group.go, app/user.go, app/session.go, app/platform/session.go, app/bot.go
// (disableUserBots, notifySysadminsBotOwnerDeactivated), app/notify_admin.go, app/upload.go,
// platform/shared/mfa/mfa.go and store/sqlstore/user_store.go, Copyright (c) 2015-present Mattermost, Inc.
// Licensed under AGPL-3.0.

use std::collections::{BTreeMap, HashMap};

use crate::app::Query;
use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::{ApiResult, App, AppError, Auth, MaybeAuth, State, decode_json, map_from_json, now_ms};
use crate::channels_api::paging;
use crate::hub::Broadcast;
use crate::model::{Session, User};
use crate::perm::{EDIT_OTHER_USERS, MANAGE_SYSTEM};
use crate::preferences::user_param;
use crate::user_edit::user_or_bot_perm;
use crate::users::{missing_account, ok};

const WRITE_USERS: &str = "sysconsole_write_user_management_users";
const READ_USERS: &str = "sysconsole_read_user_management_users";
/// `model.TokenSize`.
const TOKEN_SIZE: usize = 64;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/users/{user_id}", put(update_user).delete(delete_user))
        .route("/api/v4/users/{user_id}/roles", put(update_roles))
        .route("/api/v4/users/{user_id}/active", put(update_active))
        .route("/api/v4/users/{user_id}/auth", put(update_auth))
        .route("/api/v4/users/{user_id}/mfa", put(update_mfa))
        .route("/api/v4/users/{user_id}/mfa/generate", post(generate_mfa))
        .route("/api/v4/users/{user_id}/sessions/revoke", post(revoke_session))
        .route("/api/v4/users/{user_id}/sessions/revoke/all", post(revoke_all_for_user))
        .route("/api/v4/users/sessions/revoke/all", post(revoke_all_users))
        .route("/api/v4/users/sessions/attributes/manifest", get(attributes_manifest))
        .route("/api/v4/users/{user_id}/audits", get(audits))
        .route("/api/v4/users/password/reset", post(reset_password))
        .route("/api/v4/users/email/verify", post(verify_email))
        .route("/api/v4/users/{user_id}/email/verify/member", post(verify_member))
        .route("/api/v4/users/{user_id}/reset_failed_attempts", post(reset_failed_attempts))
        .route("/api/v4/users/{user_id}/promote", post(promote))
        .route("/api/v4/users/{user_id}/demote", post(demote))
        .route("/api/v4/users/{user_id}/convert_to_bot", post(convert_to_bot))
        .route("/api/v4/users/{user_id}/terms_of_service", post(save_user_terms).get(get_user_terms))
        .route("/api/v4/users/{user_id}/uploads", get(uploads))
        .route("/api/v4/users/{user_id}/data_retention/channel_policies", get(data_retention))
        .route("/api/v4/users/{user_id}/data_retention/team_policies", get(data_retention))
        .route("/api/v4/users/{user_id}/groups", get(groups))
        .route("/api/v4/users/group_channels", post(group_channels))
        .route("/api/v4/users/invalid_emails", get(invalid_emails))
        .route("/api/v4/users/auth_data", get(by_auth_data))
        .route("/api/v4/users/login/sso/code-exchange", post(sso_code_exchange))
        .route("/api/v4/users/login/desktop_token", post(desktop_token))
        .route("/api/v4/users/login/cws", post(login_cws))
        .route("/api/v4/users/login/type", post(login_type))
        .route("/api/v4/users/login/switch", post(switch_account))
        .route("/api/v4/users/migrate_auth/ldap", post(migrate_ldap))
        .route("/api/v4/users/migrate_auth/saml", post(migrate_saml))
        .route("/api/v4/users/notify-admin", post(notify_admin))
        .route("/api/v4/users/trigger-notify-admin-posts", post(trigger_notify_admin))
}

async fn get_user(app: &App, id: &str) -> ApiResult<User> {
    Ok((*app.user(id).await?.ok_or_else(missing_account)?).clone())
}

fn oauth_denied(e: AppError) -> AppError {
    let d = format!("{}, attempted access by oauth app", e.detailed_error);
    e.detail(d)
}

/// `model.StringInterfaceFromJSON`: a JSON object, anything else empty.
fn object_from_json(b: &[u8]) -> Map<String, Value> {
    serde_json::Deserializer::from_slice(b)
        .into_iter::<Map<String, Value>>()
        .next()
        .and_then(Result::ok)
        .unwrap_or_default()
}

// ---------- sessions ----------

impl App {
    /// `RevokeAllSessions`: every session of the user (OAuth ones with their access data), the cache, websockets.
    pub async fn revoke_all_sessions(&self, user_id: &str) -> ApiResult<()> {
        let sessions: Vec<(String, String, bool)> =
            sqlx::query_as("SELECT Id, Token, COALESCE(IsOAuth, false) FROM Sessions WHERE UserId = $1")
                .bind(user_id)
                .fetch_all(&self.db)
                .await
                .map_err(|e| AppError::internal("app.session.get_sessions.app_error", e.to_string()))?;
        for (id, token, oauth) in sessions {
            if oauth {
                sqlx::query("DELETE FROM OAuthAccessData WHERE Token = $1").bind(&token).execute(&self.db).await?;
            }
            sqlx::query("DELETE FROM Sessions WHERE Id = $1 OR Token = $2")
                .bind(&id)
                .bind(&token)
                .execute(&self.db)
                .await
                .map_err(|e| AppError::internal("app.session.remove.app_error", e.to_string()))?;
            self.caches.session.remove(&token);
            self.hub.close_session(&token);
        }
        self.clear_sessions_for_user(user_id);
        Ok(())
    }
}

/// `revokeSession`.
async fn revoke_session(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let user_id = user_param(&s, &user_id)?;
    if !user_or_bot_perm(&app, &s, &user_id).await? {
        return Err(AppError::forbidden_perm(EDIT_OTHER_USERS));
    }
    let session_id = map_from_json(&body).remove("session_id").unwrap_or_default();
    if session_id.is_empty() {
        return Err(AppError::invalid_param("session_id"));
    }
    let found: Option<Session> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!("{} WHERE Id = $1", <Session as crate::db::Table>::SELECT)))
            .bind(&session_id)
            .fetch_optional(&app.db)
            .await?;
    let found = found.ok_or_else(|| AppError::bad_request("app.session.get.app_error", ""))?;
    if found.user_id != user_id {
        return Err(AppError::invalid_url_param("user_id"));
    }
    if found.is_oauth {
        sqlx::query("DELETE FROM OAuthAccessData WHERE Token = $1").bind(&found.token).execute(&app.db).await?;
        sqlx::query("DELETE FROM Sessions WHERE Id = $1 OR Token = $1").bind(&found.token).execute(&app.db).await?;
    }
    app.revoke_session(&found).await?;
    app.clear_sessions_for_user(&found.user_id);
    crate::audit::log(&app, Some(&s), "").await;
    Ok(ok())
}

/// `revokeAllSessionsForUser`.
async fn revoke_all_for_user(St(app): St<State>, Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Json<Value>> {
    let user_id = user_param(&s, &user_id)?;
    if !user_or_bot_perm(&app, &s, &user_id).await? {
        return Err(AppError::forbidden_perm(EDIT_OTHER_USERS));
    }
    app.revoke_all_sessions(&user_id).await?;
    crate::audit::log(&app, Some(&s), "").await;
    Ok(ok())
}

/// `revokeAllSessionsAllUsers` (`RevokeSessionsFromAllUsers`): every session and OAuth access, every cache.
async fn revoke_all_users(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    app.require(&s, MANAGE_SYSTEM).await?;
    let tokens: Vec<(String,)> = sqlx::query_as("SELECT Token FROM Sessions").fetch_all(&app.db).await?;
    sqlx::query("DELETE FROM Sessions")
        .execute(&app.db)
        .await
        .map_err(|e| AppError::internal("app.session.remove_all_sessions_for_team.app_error", e.to_string()))?;
    sqlx::query("DELETE FROM OAuthAccessData")
        .execute(&app.db)
        .await
        .map_err(|e| AppError::internal("app.oauth.remove_access_data.app_error", e.to_string()))?;
    app.caches.session.clear();
    for (t,) in tokens {
        app.hub.close_session(&t);
    }
    crate::audit::log(&app, Some(&s), "").await;
    Ok(ok())
}

/// `getSessionAttributesManifest`: session attributes are a licensed feature.
async fn attributes_manifest() -> AppError {
    AppError::new(
        StatusCode::NOT_IMPLEMENTED,
        "api.user.session_attributes.disabled.app_error",
        "Session attributes are not enabled.",
    )
}

// ---------- audits ----------

#[derive(Serialize, sqlx::FromRow)]
struct Audit {
    id: String,
    create_at: i64,
    user_id: String,
    action: String,
    extra_info: String,
    ip_address: String,
    session_id: String,
}

type Q = Query<HashMap<String, String>>;

/// `getUserAudits`: the Audits rows Go writes (this server writes none), newest first; `null` when none.
async fn audits(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    Query(q): Q,
) -> ApiResult<Json<Option<Vec<Audit>>>> {
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    let (page, per_page) = paging(&q);
    if per_page > 1000 {
        return Err(AppError::bad_request("app.audit.get.limit.app_error", ""));
    }
    let v: Vec<Audit> = sqlx::query_as(
        "SELECT Id AS id, COALESCE(CreateAt, 0) AS create_at, COALESCE(UserId, '') AS user_id,
                COALESCE(Action, '') AS action, COALESCE(ExtraInfo, '') AS extra_info,
                COALESCE(IpAddress, '') AS ip_address, COALESCE(SessionId, '') AS session_id
         FROM Audits WHERE UserId = $1 ORDER BY CreateAt DESC LIMIT $2 OFFSET $3",
    )
    .bind(&user_id)
    .bind(per_page)
    .bind(page * per_page)
    .fetch_all(&app.db)
    .await
    .map_err(|e| AppError::internal("app.audit.get.finding.app_error", e.to_string()))?;
    Ok(Json((!v.is_empty()).then_some(v)))
}

// ---------- update / delete ----------

/// `updateUser` (PUT): the whole user, the store keeping what it never takes from a client.
async fn update_user(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<User>> {
    let user_id = user_param(&s, &user_id)?;
    let user: User = decode_json(&body, "user")?;
    if user.id != user_id {
        return Err(AppError::invalid_param("user_id"));
    }
    if !user_or_bot_perm(&app, &s, &user.id).await? {
        return Err(AppError::forbidden_perm(EDIT_OTHER_USERS));
    }
    let old = get_user(&app, &user.id).await?;
    if old.is_system_admin() {
        app.require(&s, MANAGE_SYSTEM).await?;
    }
    if s.is_oauth && old.email != user.email {
        return Err(oauth_denied(AppError::forbidden_perm(EDIT_OTHER_USERS)));
    }
    // CheckProviderAttributes: a user of any login provider keeps its username (LDAP and SAML have no further
    // checks without a license).
    if !old.auth_service.is_empty() && user.username != old.username {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "api.user.update_user.login_provider_attribute_set.app_error",
            "",
        )
        .with_param("Field", "username"));
    }
    if !user.email.is_empty() && old.email != user.email && s.user_id == user_id {
        app.double_check_password(&old, &user.password).await.map_err(|_| AppError::invalid_param("password"))?;
    }
    let r = app.update_user(user, true, false).await?;
    crate::audit::log(&app, Some(&s), "").await;
    Ok(Json(r))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DeleteQuery {
    permanent: String,
}

/// `deleteUser`: deactivation (permanent deletion only with `EnableAPIUserDeletion`).
async fn delete_user(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<Json<Value>> {
    let user_id = user_param(&s, &user_id)?;
    let permanent = crate::channels_api::parse_bool(Some(&q.permanent));
    if !user_or_bot_perm(&app, &s, &user_id).await? {
        return Err(AppError::forbidden_perm(EDIT_OTHER_USERS));
    }
    if user_id == s.user_id
        && !app.cfg.bool("TeamSettings.EnableUserDeactivation")
        && !app.has_perm(&s, MANAGE_SYSTEM).await
    {
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "api.user.update_active.not_enable.app_error", ""));
    }
    let user = get_user(&app, &user_id).await?;
    if user.is_system_admin() {
        app.require(&s, MANAGE_SYSTEM).await?;
    }
    if permanent {
        if app.cfg.bool("ServiceSettings.EnableAPIUserDeletion") {
            app.permanent_delete_user(user).await?;
            return Ok(ok());
        }
        let admin = app.user(&s.user_id).await?.is_some_and(|u| u.is_system_admin());
        let id = if admin {
            "api.user.delete_user.not_enabled.for_admin.app_error"
        } else {
            "api.user.delete_user.not_enabled.app_error"
        };
        return Err(AppError::new(StatusCode::UNAUTHORIZED, id, ""));
    }
    app.update_active(user, false).await?;
    Ok(ok())
}

impl App {
    /// `UpdateActive`: `DeleteAt` set or cleared through the store, then on deactivation the sessions revoked and
    /// `userDeactivated`; channel member caches, `user_updated`. (Go's 250-active-user cap is not kept.)
    pub async fn update_active(&self, mut user: User, active: bool) -> ApiResult<User> {
        let now = now_ms();
        user.delete_at = if active { 0 } else { now };
        let ruser = self.update_user(user, false, true).await?;
        if !active {
            self.revoke_all_sessions(&ruser.id).await?;
            self.user_deactivated(&ruser.id).await?;
        }
        self.invalidate_user_channel_members_caches(&ruser.id).await;
        if let Some(u) = self.user(&ruser.id).await? {
            self.send_updated_user_event(&u);
        }
        Ok(ruser)
    }

    /// `invalidateUserChannelMembersCaches`: the member caches of every channel of the user.
    pub async fn invalidate_user_channel_members_caches(&self, user_id: &str) {
        let ids: Vec<(String,)> = sqlx::query_as(
            "SELECT cm.ChannelId FROM ChannelMembers cm JOIN Channels c ON c.Id = cm.ChannelId
             WHERE cm.UserId = $1 AND c.DeleteAt = 0",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        for (id,) in ids {
            self.invalidate_channel_members_cache(&id);
        }
    }

    /// `userDeactivated`: offline, admins told about the user's bots (and the bots disabled with
    /// `DisableBotsWhenOwnerIsDeactivated`), OAuth grants removed.
    pub(crate) async fn user_deactivated(&self, user_id: &str) -> ApiResult<()> {
        self.set_status_offline(user_id, false, true).await;
        let user = get_user(self, user_id).await?;
        if !user.is_bot
            && let Err(e) = self.notify_admins_bot_owner_deactivated(&user).await
        {
            eprintln!("notifying admins about the bots of {user_id}: {}", e.detailed_error);
        }
        // disableUserBots. A bot made from a user owns itself: Go disables it from within its own
        // deactivation, again and again, until the stack gives out; it is skipped here.
        if self.cfg.bool("ServiceSettings.DisableBotsWhenOwnerIsDeactivated") {
            let bots: Vec<(String,)> =
                sqlx::query_as("SELECT UserId FROM Bots WHERE OwnerId = $1 AND UserId <> $1 AND DeleteAt = 0")
                    .bind(user_id)
                    .fetch_all(&self.db)
                    .await?;
            for (bot,) in bots {
                if let Err(e) = Box::pin(self.update_bot_active(&bot, false)).await {
                    eprintln!("disabling bot {bot}: {}", e.detailed_error);
                }
            }
        }
        let _ = sqlx::query("DELETE FROM OAuthAuthData WHERE UserId = $1").bind(user_id).execute(&self.db).await;
        let _ = sqlx::query("DELETE FROM OAuthAccessData WHERE UserId = $1").bind(user_id).execute(&self.db).await;
        Ok(())
    }

    /// `notifySysadminsBotOwnerDeactivated`: a message in each active system admin's own DM.
    async fn notify_admins_bot_owner_deactivated(&self, user: &User) -> ApiResult<()> {
        let bots: Vec<(String,)> = sqlx::query_as(
            "SELECT u.Username FROM Bots b JOIN Users u ON u.Id = b.UserId WHERE b.OwnerId = $1 AND b.DeleteAt = 0
             ORDER BY b.CreateAt ASC, u.Username ASC",
        )
        .bind(&user.id)
        .fetch_all(&self.db)
        .await?;
        if bots.is_empty() {
            return Ok(());
        }
        let admins: Vec<(String,)> = sqlx::query_as(
            "SELECT Id FROM Users WHERE Roles LIKE '%system_admin%' AND DeleteAt = 0 ORDER BY Username ASC",
        )
        .fetch_all(&self.db)
        .await?;
        let all = bots.len() <= 10;
        let names: String = bots.iter().take(10).map(|(n,)| format!("* {n}\n")).collect();
        let (who, n) = (&user.username, bots.len());
        let doc = "For more information, see our [documentation](https://docs.mattermost.com/developer/bot-accounts.html#what-happens-when-a-user-who-owns-bot-accounts-is-disabled).";
        let message = if self.cfg.bool("ServiceSettings.DisableBotsWhenOwnerIsDeactivated") {
            let head = if all {
                format!(
                    "{who} was deactivated. They managed the following bot accounts which have now been disabled.\n\n{names}"
                )
            } else {
                format!(
                    "{who} was deactivated. They managed {n} bot accounts which have now been disabled, including the following:\n\n{names}"
                )
            };
            format!(
                "{head}You can take ownership of each bot by enabling it at **Integrations > Bot Accounts** and creating new tokens for the bot.\n\n{doc}"
            )
        } else {
            let head = if all {
                format!(
                    "{who} was deactivated. They managed the following bot accounts which are still enabled.\n\n{names}\n"
                )
            } else {
                format!(
                    "{who} was deactivated. They managed {n} bot accounts which are still enabled, including the following:\n\n{names}"
                )
            };
            format!(
                "{head}We strongly recommend you to take ownership of each bot by re-enabling it at **Integrations > Bot Accounts** and creating new tokens for the bot.\n\n{doc}\n\nIf you want bot accounts to disable automatically after owner deactivation, set “Disable bot accounts when owner is deactivated” in **System Console > Integrations > Bot Accounts** to true."
            )
        };
        for (admin,) in admins {
            let s = Session { user_id: admin.clone(), roles: "system_admin system_user".into(), ..Default::default() };
            let c = self.get_or_create_direct(&s, &admin, &admin).await?;
            let post = crate::model::Post {
                user_id: admin.clone(),
                channel_id: c.id.clone(),
                message: message.clone(),
                r#type: "system_generic".into(),
                ..Default::default()
            };
            self.create_post(post, true).await?;
        }
        Ok(())
    }

    /// `UpdateUserRolesWithUser`: the roles must exist and the last system admin stays one; sessions follow.
    pub async fn update_user_roles(&self, mut user: User, new_roles: &str, send_event: bool) -> ApiResult<User> {
        let names: Vec<&str> = new_roles.split_whitespace().collect();
        let found = self.roles(&names).await?;
        if let Some(missing) = names.iter().find(|n| !found.iter().any(|r| r.name == **n)) {
            return Err(AppError::bad_request("app.role.check_roles_exist.role_not_found", "")
                .detail(format!("role={missing}")));
        }
        if user.is_system_admin() && !new_roles.contains("system_admin") {
            let (admins,): (i64,) = sqlx::query_as(
                "SELECT COUNT(DISTINCT u.Id) FROM Users u LEFT JOIN Bots b ON b.UserId = u.Id
                 WHERE b.UserId IS NULL AND u.DeleteAt = 0 AND u.Roles LIKE '%system_admin%'",
            )
            .fetch_one(&self.db)
            .await
            .map_err(|e| AppError::bad_request("app.user.update.countAdmins.app_error", "").detail(e.to_string()))?;
            if admins <= 1 {
                return Err(AppError::bad_request("app.user.update.lastAdmin.app_error", ""));
            }
        }
        user.roles = new_roles.to_string();
        let ruser = self.update_user(user, false, true).await?;
        if let Err(e) = sqlx::query("UPDATE Sessions SET Roles = $1 WHERE UserId = $2")
            .bind(new_roles)
            .bind(&ruser.id)
            .execute(&self.db)
            .await
        {
            eprintln!("updating the roles of the sessions of {}: {e}", ruser.id);
        }
        self.invalidate_user(&ruser.id);
        self.clear_sessions_for_user(&ruser.id);
        if send_event {
            self.hub.publish(
                "user_role_updated",
                json!({"user_id": ruser.id, "roles": new_roles}),
                Broadcast { user_id: ruser.id.clone(), ..Default::default() },
            );
        }
        Ok(ruser)
    }
}

/// `model.IsValidUserRoles`.
fn valid_user_roles(roles: &str) -> bool {
    let r: Vec<&str> = roles.split_whitespace().collect();
    r.iter().all(|n| {
        !n.is_empty() && n.len() <= 64 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }) && !(r.len() == 1 && r[0] == "system_admin")
}

/// `updateUserRoles`: the newer system roles need a license.
async fn update_roles(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let user_id = user_param(&s, &user_id)?;
    let roles = map_from_json(&body).remove("roles").unwrap_or_default();
    if !valid_user_roles(&roles) {
        return Err(AppError::invalid_param("roles"));
    }
    const LICENSED: &[&str] =
        &["system_user_manager", "system_read_only_admin", "system_manager", "shared_channel_manager"];
    if roles.split_whitespace().any(|r| LICENSED.contains(&r)) {
        return Err(AppError::bad_request("api.user.update_user_roles.license.app_error", ""));
    }
    app.require(&s, "manage_roles").await?;
    // UpdateUserRoles: a missing user is a 400 there.
    let user = get_user(&app, &user_id).await.map_err(|mut e| {
        e.status_code = 400;
        e
    })?;
    app.update_user_roles(user, &roles, true).await?;
    crate::audit::log(&app, Some(&s), &format!("user={user_id} roles={roles}")).await;
    Ok(ok())
}

/// `updateUserActive`.
async fn update_active(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let user_id = user_param(&s, &user_id)?;
    let Some(active) = object_from_json(&body).get("active").and_then(Value::as_bool) else {
        return Err(AppError::invalid_param("active"));
    };
    let self_deactivate = !active && user_id == s.user_id;
    if !self_deactivate && !app.has_perm(&s, WRITE_USERS).await {
        return Err(AppError::forbidden().with_id("api.user.update_active.permissions.app_error"));
    }
    if self_deactivate && !app.cfg.bool("TeamSettings.EnableUserDeactivation") {
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "api.user.update_active.not_enable.app_error", ""));
    }
    let user = get_user(&app, &user_id).await?;
    if user.is_system_admin() {
        app.require(&s, MANAGE_SYSTEM).await?;
    }
    if user.is_bot {
        app.require_manage_bot(&s, &user_id).await?;
    }
    if active && user.is_guest() && !app.cfg.bool("GuestAccountsSettings.Enable") {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "api.user.update_active.cannot_enable_guest_when_guest_feature_is_disabled.app_error",
            "",
        ));
    }
    if user.auth_service == "ldap" {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.user.update_active.cannot_modify_status_when_user_is_managed_by_ldap.app_error",
            "",
        ));
    }
    let (email, locale) = (user.email.clone(), user.locale.clone());
    let uid = user.id.clone();
    app.update_active(user, active).await?;
    crate::audit::log(&app, Some(&s), &format!("user_id={uid} active={active}")).await;
    if self_deactivate {
        crate::email::go(&app, "deactivate email", move |app| async move {
            crate::email::send_deactivate(&app, &email, &locale).await
        });
    }
    app.hub.publish("user_activation_status_change", json!({}), Broadcast::default());
    Ok(ok())
}

#[derive(Deserialize, Serialize, Default)]
#[serde(default)]
struct UserAuth {
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_data: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    auth_service: String,
}

impl UserAuth {
    fn is_valid(&self) -> bool {
        const SERVICES: &[&str] = &["email", "gitlab", "ldap", "saml", "google", "office365", "openid"];
        if !SERVICES.contains(&self.auth_service.as_str()) {
            return false;
        }
        if self.auth_service == "email" {
            return self.auth_data.is_none();
        }
        self.auth_data.as_deref().is_some_and(|d| !d.is_empty())
    }
}

impl App {
    /// `UpdateUserAuth` (`SqlUserStore.UpdateAuthData`): the password goes, the sessions too.
    async fn update_user_auth(&self, user_id: &str, service: &str, data: Option<&str>) -> ApiResult<()> {
        let now = now_ms();
        let r = sqlx::query(
            "UPDATE Users SET Password = '', LastPasswordUpdate = $1, UpdateAt = $1, FailedAttempts = 0,
                 AuthService = $2, AuthData = $3 WHERE Id = $4",
        )
        .bind(now)
        .bind(service)
        .bind(data)
        .bind(user_id)
        .execute(&self.db)
        .await;
        if let Err(e) = r {
            let m = e.to_string();
            let id = if m.contains("email") || m.contains("authdata") {
                "app.user.update_auth_data.email_exists.app_error"
            } else {
                "app.user.update_auth_data.app_error"
            };
            return Err(if id.contains("exists") { AppError::bad_request(id, "") } else { AppError::internal(id, m) });
        }
        self.invalidate_user(user_id);
        self.revoke_all_sessions(user_id).await
    }
}

/// `updateUserAuth`: system admins only.
async fn update_auth(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<UserAuth>> {
    if !app.has_perm(&s, MANAGE_SYSTEM).await {
        return Err(AppError::forbidden_perm(EDIT_OTHER_USERS));
    }
    let user_id = user_param(&s, &user_id)?;
    let mut ua: UserAuth = decode_json(&body, "user")?;
    if !ua.is_valid() {
        return Err(AppError::bad_request("api.user.update_user_auth.invalid_request", ""));
    }
    if ua.auth_service == "email" {
        ua.auth_service.clear();
    }
    app.update_user_auth(&user_id, &ua.auth_service, ua.auth_data.as_deref()).await?;
    crate::audit::log(&app, Some(&s), &format!("updated user {user_id} auth to service={}", ua.auth_service)).await;
    Ok(Json(ua))
}

// ---------- MFA ----------

/// `updateUserMfa`.
async fn update_mfa(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let user_id = user_param(&s, &user_id)?;
    if s.is_oauth {
        return Err(oauth_denied(AppError::forbidden_perm(EDIT_OTHER_USERS)));
    }
    app.require_user(&s, &user_id).await?;
    let props = object_from_json(&body);
    let Some(activate) = props.get("activate").and_then(Value::as_bool) else {
        return Err(AppError::invalid_param("activate"));
    };
    let code = props.get("code").and_then(Value::as_str).unwrap_or("");
    if activate && code.is_empty() {
        return Err(AppError::invalid_param("code"));
    }
    crate::audit::log(&app, Some(&s), "attempt").await;
    let user = get_user(&app, &user_id).await?;
    if activate {
        // ActivateMfa
        if !user.auth_service.is_empty() && user.auth_service != "ldap" {
            return Err(AppError::bad_request("api.user.activate_mfa.email_and_ldap_only.app_error", ""));
        }
        if !app.cfg.bool("ServiceSettings.EnableMultifactorAuthentication") {
            return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "mfa.mfa_disabled.app_error", ""));
        }
        let used =
            mfa_used_timestamps(&app, &user_id).await.map_err(|e| AppError::internal("mfa.activate.app_error", e))?;
        let Ok(Some(used)) = mfa_authenticate(&user.mfa_secret, code, now_ms() / 1000, used) else {
            // Go answers a code that is not six digits with a 500; both are a bad token here.
            return Err(AppError::new(StatusCode::UNAUTHORIZED, "mfa.activate.bad_token.app_error", ""));
        };
        // UpdateMfaActive, then StoreMfaUsedTimestamps (strings, as Go's StringArray).
        let used = sqlx::types::Json(used.iter().map(|t| Value::String(t.to_string())).collect::<Vec<_>>());
        sqlx::query("UPDATE Users SET MfaActive = true, MfaUsedTimestamps = $1, UpdateAt = $2 WHERE Id = $3")
            .bind(used)
            .bind(now_ms())
            .bind(&user_id)
            .execute(&app.db)
            .await
            .map_err(|e| AppError::internal("mfa.activate.app_error", e.to_string()))?;
    } else {
        // UpdateMfaActive, then UpdateMfaSecret (which empties the used time steps).
        sqlx::query(
            "UPDATE Users SET MfaActive = false, MfaSecret = '', MfaUsedTimestamps = '[]', UpdateAt = $2 WHERE Id = $1",
        )
        .bind(&user_id)
        .bind(now_ms())
        .execute(&app.db)
        .await
        .map_err(|e| AppError::internal("mfa.deactivate.app_error", e.to_string()))?;
    }
    app.invalidate_user(&user_id);
    if let Some(u) = app.user(&user_id).await? {
        let (email, locale) = (u.email.clone(), u.locale.clone());
        crate::email::go(&app, "mfa change email", move |app| async move {
            crate::email::send_mfa_change(&app, &email, activate, &locale).await
        });
    }
    crate::audit::log(&app, Some(&s), "success - mfa updated").await;
    Ok(ok())
}

/// `generateMfaSecret`: a new secret stored for the user and its `otpauth://` QR code.
async fn generate_mfa(St(app): St<State>, Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Response> {
    let user_id = user_param(&s, &user_id)?;
    if s.is_oauth {
        return Err(oauth_denied(AppError::forbidden_perm(EDIT_OTHER_USERS)));
    }
    app.require_user(&s, &user_id).await?;
    let user = get_user(&app, &user_id).await?;
    if !app.cfg.bool("ServiceSettings.EnableMultifactorAuthentication") {
        return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "mfa.mfa_disabled.app_error", ""));
    }
    let fail = |e: String| AppError::internal("mfa.generate_qr_code.create_code.app_error", e);
    let secret = base32_std(&rand::random::<[u8; 20]>());
    let site = app.cfg.str("ServiceSettings.SiteURL").trim();
    let issuer = if site.is_empty() {
        crate::brand::name().to_string()
    } else {
        let s = site.trim_start_matches("https://").trim_start_matches("http://");
        s.strip_prefix("www.").unwrap_or(s).to_string()
    };
    let issuer = query_escape(&issuer);
    let link = format!("otpauth://totp/{issuer}:{}?secret={secret}&issuer={issuer}", user.email);
    let png = qr_png(&link).map_err(fail)?;
    // UpdateMfaSecret: the used time steps emptied, UpdateAt.
    sqlx::query("UPDATE Users SET MfaSecret = $1, MfaUsedTimestamps = '[]', UpdateAt = $3 WHERE Id = $2")
        .bind(&secret)
        .bind(&user_id)
        .bind(now_ms())
        .execute(&app.db)
        .await
        .map_err(|e| fail(e.to_string()))?;
    app.invalidate_user(&user_id);
    use base64::Engine;
    let body = json!({"secret": secret, "qr_code": base64::engine::general_purpose::STANDARD.encode(png)});
    let mut r = Json(body).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    h.insert(header::EXPIRES, HeaderValue::from_static("0"));
    Ok(r)
}

/// `url.QueryEscape`.
fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A QR code (level H) as rsc/qr draws it: 8 pixels a module, a 4-module border.
fn qr_png(text: &str) -> Result<Vec<u8>, String> {
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::H).map_err(|e| e.to_string())?;
    let n = code.width() as u32;
    let (scale, border) = (8u32, 4u32);
    let size = (n + 2 * border) * scale;
    let colors = code.to_colors();
    let img = image::GrayImage::from_fn(size, size, |x, y| {
        let (mx, my) = (x / scale, y / scale);
        let dark = mx >= border
            && my >= border
            && mx < n + border
            && my < n + border
            && colors[((my - border) * n + (mx - border)) as usize] == qrcode::Color::Dark;
        image::Luma([if dark { 0 } else { 255 }])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

// ---------- password reset and email verification tokens ----------

#[derive(sqlx::FromRow)]
struct Token {
    token: String,
    create_at: i64,
    r#type: String,
    extra: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct TokenExtra {
    user_id: String,
    email: String,
}

async fn token_row(app: &App, token: &str) -> ApiResult<Option<Token>> {
    Ok(sqlx::query_as(
        "SELECT Token AS token, COALESCE(CreateAt, 0) AS create_at, COALESCE(Type, '') AS type, COALESCE(Extra, '') AS extra
         FROM Tokens WHERE Token = $1",
    )
    .bind(token)
    .fetch_optional(&app.db)
    .await?)
}

async fn delete_token(app: &App, token: &str) {
    if let Err(e) = sqlx::query("DELETE FROM Tokens WHERE Token = $1").bind(token).execute(&app.db).await {
        eprintln!("deleting a token: {e}");
    }
}

/// `resetPassword` (`ResetPasswordFromToken`): a password recovery token of the last hour.
async fn reset_password(St(app): St<State>, body: Bytes) -> ApiResult<Json<Value>> {
    let mut props = map_from_json(&body);
    let token = props.remove("token").unwrap_or_default();
    if token.len() != TOKEN_SIZE {
        return Err(AppError::invalid_param("token"));
    }
    let new_password = props.remove("new_password").unwrap_or_default();
    let prefix = &token[..5];
    crate::audit::log(&app, None, &format!("attempt - token_prefix={prefix}")).await;
    match reset_password_from_token(&app, &token, &new_password).await {
        Ok(()) => {
            crate::audit::log(&app, None, &format!("success - token_prefix={prefix}")).await;
            Ok(ok())
        }
        Err(e) => {
            crate::audit::log(&app, None, &format!("fail - token_prefix={prefix}")).await;
            Err(e)
        }
    }
}

/// `ResetPasswordFromToken`.
async fn reset_password_from_token(app: &App, token: &str, new_password: &str) -> ApiResult<()> {
    let t = token_row(app, token)
        .await?
        .ok_or_else(|| AppError::bad_request("api.user.reset_password.invalid_link.app_error", ""))?;
    if t.r#type != "password_recovery" {
        return Err(AppError::bad_request("api.user.reset_password.broken_token.app_error", ""));
    }
    if now_ms() > t.create_at + 3_600_000 {
        return Err(AppError::bad_request("api.user.reset_password.link_expired.app_error", ""));
    }
    let extra: TokenExtra = serde_json::from_str(&t.extra)
        .map_err(|_| AppError::internal("api.user.reset_password.token_parse.error", ""))?;
    let user = get_user(app, &extra.user_id).await?;
    if user.email != extra.email {
        return Err(AppError::bad_request("api.user.reset_password.link_expired.app_error", ""));
    }
    if !user.auth_service.is_empty() && user.auth_service != "email" {
        return Err(AppError::bad_request("api.user.reset_password.sso.app_error", ""));
    }
    if user.auth_service == "magic_link" && user.is_guest() {
        return Err(AppError::bad_request("api.user.send_password_reset.guest_magic_link.app_error", ""));
    }
    if user.remote_id.as_deref().is_some_and(|r| !r.is_empty()) {
        return Err(AppError::bad_request("api.user.reset_password.broken_token.app_error", ""));
    }
    app.set_password(&user, new_password, None).await?;
    crate::user_edit::password_change_email(app, &user, "api.user.reset_password.method");
    delete_token(app, &t.token).await;
    Ok(())
}

/// `verifyUserEmail` (`VerifyEmailFromToken`): any failure is a bad link.
async fn verify_email(St(app): St<State>, body: Bytes) -> ApiResult<Json<Value>> {
    let token = map_from_json(&body).remove("token").unwrap_or_default();
    if token.len() != TOKEN_SIZE {
        return Err(AppError::invalid_param("token"));
    }
    let bad = || AppError::bad_request("api.user.verify_email.bad_link.app_error", "");
    let t = token_row(&app, &token).await?.ok_or_else(bad)?;
    // Token.IsExpired: 48 hours.
    if t.r#type != "verify_email" || now_ms() > t.create_at + 48 * 3_600_000 {
        return Err(bad());
    }
    let extra: TokenExtra = serde_json::from_str(&t.extra).map_err(|_| bad())?;
    let user = get_user(&app, &extra.user_id).await.map_err(|_| bad())?;
    let new_email = extra.email.to_lowercase();
    app.verify_user_email(&extra.user_id, &new_email).await.map_err(|_| bad())?;
    if user.email != new_email {
        let (old, locale) = (user.email.clone(), user.locale.clone());
        crate::email::go(&app, "email change email", move |app| async move {
            crate::email::send_email_change(&app, &old, &new_email, &locale).await
        });
    }
    delete_token(&app, &t.token).await;
    crate::audit::log(&app, None, "Email Verified").await;
    Ok(ok())
}

impl App {
    /// `VerifyUserEmail`.
    async fn verify_user_email(&self, user_id: &str, email: &str) -> ApiResult<()> {
        sqlx::query("UPDATE Users SET Email = lower($1), EmailVerified = true, UpdateAt = $2 WHERE Id = $3")
            .bind(email)
            .bind(now_ms())
            .bind(user_id)
            .execute(&self.db)
            .await
            .map_err(|e| AppError::internal("app.user.verify_email.app_error", e.to_string()))?;
        self.invalidate_user(user_id);
        let u = get_user(self, user_id).await?;
        self.send_updated_user_event(&u);
        Ok(())
    }
}

/// `verifyUserEmailWithoutToken`.
async fn verify_member(St(app): St<State>, Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Json<User>> {
    let user_id = user_param(&s, &user_id)?;
    let user = get_user(&app, &user_id).await?;
    app.require(&s, MANAGE_SYSTEM).await?;
    app.verify_user_email(&user.id, &user.email).await?;
    crate::audit::log(&app, Some(&s), "user verified").await;
    let mut u = user;
    u.sanitize_profile(app.show(&s).await);
    Ok(Json(u))
}

/// `resetPasswordFailedAttempts` (the user cache is invalidated too, which Go misses).
async fn reset_failed_attempts(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let user_id = user_param(&s, &user_id)?;
    if !app.has_perm(&s, WRITE_USERS).await {
        return Err(AppError::forbidden().with_id("api.user.reset_password_failed_attempts.permissions.app_error"));
    }
    let user = get_user(&app, &user_id).await?;
    if user.is_system_admin() {
        app.require(&s, MANAGE_SYSTEM).await?;
    }
    if user.auth_service != "ldap" && !user.auth_service.is_empty() {
        return Err(AppError::bad_request("api.user.reset_password_failed_attempts.ldap_and_email_only.app_error", ""));
    }
    sqlx::query("UPDATE Users SET FailedAttempts = 0 WHERE Id = $1")
        .bind(&user_id)
        .execute(&app.db)
        .await
        .map_err(|e| AppError::internal("app.user.reset_password_failed_attempts.app_error", e.to_string()))?;
    app.invalidate_user(&user_id);
    Ok(ok())
}

// ---------- guests ----------

/// `promoteGuestToUser`.
async fn promote(St(app): St<State>, Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Json<Value>> {
    let user_id = user_param(&s, &user_id)?;
    app.require(&s, "promote_guest").await?;
    let user = get_user(&app, &user_id).await?;
    if !user.is_guest() {
        return Err(AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "api.user.promote_guest_to_user.no_guest.app_error",
            "",
        ));
    }
    if user.auth_service == "magic_link" {
        return Err(AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "api.user.promote_guest_to_user.magic_link_enabled.app_error",
            "",
        ));
    }
    app.promote_guest(&user, &s.user_id).await?;
    Ok(ok())
}

impl App {
    /// `PromoteGuestToUser`: user, team and channel roles; the default channels of the user's teams; events.
    async fn promote_guest(&self, user: &User, requestor: &str) -> ApiResult<()> {
        let roles: Vec<&str> =
            user.roles.split_whitespace().map(|r| if r == "system_guest" { "system_user" } else { r }).collect();
        let fail = |e: sqlx::Error| AppError::internal("app.user.promote_guest.user_update.app_error", e.to_string());
        let mut tx = self.db.begin().await.map_err(fail)?;
        sqlx::query("UPDATE Users SET Roles = $1, UpdateAt = $2 WHERE Id = $3")
            .bind(roles.join(" "))
            .bind(now_ms())
            .bind(&user.id)
            .execute(&mut *tx)
            .await
            .map_err(fail)?;
        sqlx::query("UPDATE ChannelMembers SET SchemeUser = true, SchemeGuest = false WHERE UserId = $1")
            .bind(&user.id)
            .execute(&mut *tx)
            .await
            .map_err(fail)?;
        sqlx::query("UPDATE TeamMembers SET SchemeUser = true, SchemeGuest = false WHERE UserId = $1")
            .bind(&user.id)
            .execute(&mut *tx)
            .await
            .map_err(fail)?;
        tx.commit().await.map_err(fail)?;
        self.invalidate_user(&user.id);
        let promoted = get_user(self, &user.id).await?;
        for team in self.teams_of_user(&user.id).await? {
            if let Err(e) = self.join_default_channels(&team.id, &promoted, false, requestor).await {
                eprintln!("joining default channels of {}: {}", team.id, e.detailed_error);
            }
        }
        self.send_updated_user_event(&promoted);
        // `UpdateSessionsIsGuest`: the sessions' roles (`Session().UpdateRoles`), then their `is_guest` prop.
        sqlx::query("UPDATE Sessions SET Roles = $1 WHERE UserId = $2")
            .bind(&promoted.roles)
            .bind(&user.id)
            .execute(&self.db)
            .await
            .ok();
        sqlx::query("UPDATE Sessions SET Props = jsonb_set(Props::jsonb, '{is_guest}', to_jsonb($1::text))::text::jsonb WHERE UserId = $2")
            .bind(promoted.is_guest().to_string())
            .bind(&user.id)
            .execute(&self.db)
            .await
            .ok();
        let members = crate::perm::team_members(self, "tm.userid = $1 AND tm.deleteat = 0", &[&user.id]).await?;
        for m in members {
            let mut sm = m.clone();
            crate::teams::sanitize_member(&mut sm, &user.id);
            self.hub.publish(
                "memberrole_updated",
                json!({"member": serde_json::to_string(&sm).unwrap_or_default()}),
                Broadcast { user_id: m.user_id.clone(), ..Default::default() },
            );
            // `GetChannelMembersForUser`: the team's channels (archived too) and the DMs and GMs, once per team.
            let channel_members = self
                .channel_members_where(
                    "cm.userid = $1 AND (t.id = ANY($2) OR t.id = '' OR t.id IS NULL) AND c.type <> 'S'",
                    &user.id,
                    std::slice::from_ref(&m.team_id),
                )
                .await
                .unwrap_or_default();
            for cm in channel_members {
                self.invalidate_channel_members_cache(&cm.channel_id);
                self.hub.publish(
                    "channel_member_updated",
                    json!({"channelMember": serde_json::to_string(&cm).unwrap_or_default()}),
                    Broadcast { user_id: user.id.clone(), ..Default::default() },
                );
            }
        }
        self.invalidate_user_teams(&user.id).await;
        self.clear_sessions_for_user(&user.id);
        Ok(())
    }
}

/// `demoteUserToGuest`: guest accounts need a license.
async fn demote(Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Json<Value>> {
    user_param(&s, &user_id)?;
    Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.team.demote_user_to_guest.license.error", ""))
}

/// `convertUserToBot`.
async fn convert_to_bot(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
) -> ApiResult<Json<crate::bots::Bot>> {
    let user_id = user_param(&s, &user_id)?;
    let mut user = get_user(&app, &user_id).await?;
    app.require(&s, MANAGE_SYSTEM).await?;
    if !user.auth_service.is_empty() {
        app.update_user_auth(&user.id, "", Some("")).await?;
        user = get_user(&app, &user.id).await?;
    }
    let bot = app.save_bot(&user.id, &user.username, &user.id, "").await?;
    app.revoke_all_sessions(&user.id).await?;
    app.invalidate_user(&user.id);
    Ok(Json(crate::bots::Bot { display_name: user.username.clone(), ..bot }))
}

// ---------- terms of service ----------

#[derive(Serialize, sqlx::FromRow)]
struct UserTerms {
    user_id: String,
    terms_of_service_id: String,
    create_at: i64,
}

/// `saveUserTermsOfService`: for the session's user, whatever the path says.
async fn save_user_terms(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    user_param(&s, &user_id)?;
    let props = object_from_json(&body);
    let Some(terms_id) = props.get("termsOfServiceId").and_then(Value::as_str) else {
        return Err(AppError::invalid_param("termsOfServiceId"));
    };
    let Some(accepted) = props.get("accepted").and_then(Value::as_bool) else {
        return Err(AppError::invalid_param("accepted"));
    };
    app.terms_of_service(terms_id).await?;
    if accepted {
        sqlx::query(
            "INSERT INTO UserTermsOfService (UserId, TermsOfServiceId, CreateAt) VALUES ($1, $2, $3)
             ON CONFLICT (UserId) DO UPDATE SET TermsOfServiceId = $2, CreateAt = $3",
        )
        .bind(&s.user_id)
        .bind(terms_id)
        .bind(now_ms())
        .execute(&app.db)
        .await
        .map_err(|e| AppError::internal("app.user_terms_of_service.save.app_error", e.to_string()))?;
    } else {
        sqlx::query("DELETE FROM UserTermsOfService WHERE UserId = $1 AND TermsOfServiceId = $2")
            .bind(&s.user_id)
            .bind(terms_id)
            .execute(&app.db)
            .await
            .map_err(|e| AppError::internal("app.user_terms_of_service.delete.app_error", e.to_string()))?;
    }
    crate::audit::log(&app, Some(&s), &format!("TermsOfServiceId={terms_id}, accepted={accepted}")).await;
    Ok(ok())
}

/// `getUserTermsOfService`: the session's user's.
async fn get_user_terms(St(app): St<State>, Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Json<UserTerms>> {
    user_param(&s, &user_id)?;
    let t: Option<UserTerms> = sqlx::query_as(
        "SELECT UserId AS user_id, COALESCE(TermsOfServiceId, '') AS terms_of_service_id, COALESCE(CreateAt, 0) AS create_at
         FROM UserTermsOfService WHERE UserId = $1",
    )
    .bind(&s.user_id)
    .fetch_optional(&app.db)
    .await
    .map_err(|e| AppError::internal("app.user_terms_of_service.get_by_user.app_error", e.to_string()))?;
    t.map(Json).ok_or_else(|| AppError::not_found("app.user_terms_of_service.get_by_user.no_rows.app_error", ""))
}

// ---------- lists ----------

#[derive(Serialize, sqlx::FromRow)]
struct UploadSession {
    id: String,
    r#type: String,
    create_at: i64,
    user_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    channel_id: String,
    filename: String,
    file_size: i64,
    file_offset: i64,
    remote_id: String,
    req_file_id: String,
}

/// `getUploadsForUser`: one's own resumable uploads.
async fn uploads(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Vec<UploadSession>>> {
    let user_id = user_param(&s, &user_id)?;
    if user_id != s.user_id {
        return Err(AppError::forbidden().with_id("api.user.get_uploads_for_user.forbidden.app_error"));
    }
    let v = sqlx::query_as(
        "SELECT Id AS id, COALESCE(Type::text, '') AS type, COALESCE(CreateAt, 0) AS create_at, COALESCE(UserId, '') AS user_id,
                COALESCE(ChannelId, '') AS channel_id, COALESCE(Filename, '') AS filename, COALESCE(FileSize, 0) AS file_size,
                COALESCE(FileOffset, 0) AS file_offset, COALESCE(RemoteId, '') AS remote_id, COALESCE(ReqFileId, '') AS req_file_id
         FROM UploadSessions WHERE UserId = $1 ORDER BY CreateAt ASC",
    )
    .bind(&user_id)
    .fetch_all(&app.db)
    .await
    .map_err(|e| AppError::internal("app.upload.get_for_user.app_error", e.to_string()))?;
    Ok(Json(v))
}

/// `getChannelPoliciesForUser` / `getTeamPoliciesForUser`: data retention needs a license.
async fn data_retention(Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Json<Value>> {
    user_param(&s, &user_id)?;
    Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "ent.data_retention.generic.license.error", ""))
}

/// `getGroupsByUserId`: groups need a license.
async fn groups(Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Json<Value>> {
    user_param(&s, &user_id)?;
    Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.license_error", "api endpoint requires a license"))
}

/// `getUsersByGroupChannelIds`: the other members of the caller's group channels (at most 50 channels).
async fn group_channels(
    St(app): St<State>,
    Auth(s): Auth,
    crate::app::Body(ids): crate::app::Body<Vec<String>>,
) -> ApiResult<Json<BTreeMap<String, Vec<User>>>> {
    let mut ids = ids;
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err(AppError::bad_request("api.payload.parse.error", "Error parsing the payload."));
    }
    ids.truncate(50);
    let pairs: Vec<(String, String)> = sqlx::query_as(
        "SELECT cm.UserId, cm.ChannelId FROM ChannelMembers cm JOIN Channels c ON cm.ChannelId = c.Id JOIN Users u ON u.Id = cm.UserId
         WHERE c.Type = 'G' AND cm.ChannelId = ANY($1) AND cm.UserId <> $2
           AND EXISTS (SELECT 1 FROM ChannelMembers m WHERE m.UserId = $2 AND m.ChannelId = cm.ChannelId)
         ORDER BY u.Username ASC",
    )
    .bind(&ids)
    .bind(&s.user_id)
    .fetch_all(&app.db)
    .await
    .map_err(|e| AppError::internal("app.user.get_profile_by_group_channel_ids_for_user.app_error", e.to_string()))?;
    let user_ids: Vec<String> = pairs.iter().map(|p| p.0.clone()).collect();
    let users = crate::users::load_users(&app, "u.id = ANY($1)", &user_ids).await?;
    let show = app.show(&s).await;
    let mut out: BTreeMap<String, Vec<User>> = BTreeMap::new();
    for (uid, cid) in pairs {
        if let Some(u) = users.iter().find(|u| u.id == uid) {
            out.entry(cid).or_default().push(crate::users::sanitized(u, show));
        }
    }
    Ok(Json(out))
}

/// `getUsersWithInvalidEmails`: users outside `RestrictCreationToDomains`, when the server is not open.
async fn invalid_emails(St(app): St<State>, Auth(s): Auth, Query(q): Q) -> ApiResult<Json<Vec<User>>> {
    if app.cfg.bool("TeamSettings.EnableOpenServer") {
        return Err(AppError::bad_request("<untranslated>", "<untranslated>"));
    }
    app.require(&s, READ_USERS).await?;
    let (page, per_page) = paging(&q);
    let domains: Vec<String> = app
        .cfg
        .str("TeamSettings.RestrictCreationToDomains")
        .split(',')
        .filter(|d| !d.is_empty())
        .map(|d| format!("%{}%", d.to_lowercase()))
        .collect();
    let users = crate::users::query_users(
        &app,
        "",
        "b.UserId IS NULL AND u.Roles != 'system_guest' AND u.DeleteAt = 0 AND (u.AuthService = '' OR u.AuthService IS NULL)
         AND NOT (u.Email LIKE ANY($1))",
        "LIMIT $2 OFFSET $3",
        vec![crate::users::Arg::V(domains), crate::users::Arg::I(per_page), crate::users::Arg::I(page * per_page)],
    )
    .await
    .map_err(|e| AppError::internal("app.user.get_profiles.app_error", e.detailed_error))?;
    Ok(Json(
        users
            .into_iter()
            .map(|mut u| {
                u.sanitize_own();
                u
            })
            .collect(),
    ))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AuthDataQuery {
    value: String,
}

/// `getUserByAuthData`: system admins only.
async fn by_auth_data(St(app): St<State>, Auth(s): Auth, Query(q): Query<AuthDataQuery>) -> ApiResult<Json<User>> {
    app.require(&s, MANAGE_SYSTEM).await?;
    if q.value.is_empty() || q.value.len() > 128 {
        return Err(AppError::invalid_param("value"));
    }
    let users =
        crate::users::query_users(&app, "", "u.AuthData = $1", "", vec![crate::users::Arg::S(q.value.clone())]).await?;
    let mut u = match users.len() {
        0 => return Err(missing_account()),
        1 => users.into_iter().next().unwrap(),
        _ => {
            let mut e = missing_account().detail("multiple users");
            e.status_code = 500;
            return Err(e);
        }
    };
    if !app.can_see_user(&s, &u.id).await? {
        return Err(AppError::forbidden_perm(crate::perm::VIEW_MEMBERS));
    }
    let t: Option<(String, i64)> =
        sqlx::query_as("SELECT TermsOfServiceId, COALESCE(CreateAt, 0) FROM UserTermsOfService WHERE UserId = $1")
            .bind(&u.id)
            .fetch_optional(&app.db)
            .await?;
    if let Some((id, at)) = t {
        (u.terms_of_service_id, u.terms_of_service_create_at) = (id, at);
    }
    u.sanitize_profile(app.show(&s).await);
    Ok(Json(u))
}

// ---------- logins this server answers as an unlicensed one ----------

/// `loginSSOCodeExchange`: off unless the `MobileSSOCodeExchange` feature flag is on, which needs SAML.
async fn sso_code_exchange() -> Response {
    let mut r =
        AppError::new(StatusCode::GONE, "api.user.login_sso_code_exchange.deprecated.app_error", "").into_response();
    r.headers_mut().insert("Deprecation", HeaderValue::from_static("true"));
    r
}

/// `loginWithDesktopToken`: the desktop app's session after an SSO sign-in in the browser (`src/sso.rs`).
async fn desktop_token(St(app): St<State>, headers: axum::http::HeaderMap, body: Bytes) -> ApiResult<Response> {
    crate::sso::login_with_desktop_token(&app, &headers, &body).await
}

/// `loginCWS`: Cloud only.
async fn login_cws() -> AppError {
    AppError::new(StatusCode::UNAUTHORIZED, "api.user.login_cws.license.error", "")
}

/// `getLoginType`: guest magic links need a license, so a bare 404.
async fn login_type() -> StatusCode {
    StatusCode::NOT_FOUND
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SwitchRequest {
    current_service: String,
    new_service: String,
    email: String,
    password: String,
    new_password: String,
    mfa_code: String,
}

/// `switchAccountType`: email to GitLab (or SAML) answers the provider's sign-in link (`src/sso.rs`); LDAP is
/// not available on an unlicensed server. (Go revokes every session of the user before finding out LDAP is
/// unavailable for email to LDAP; this server does not.)
async fn switch_account(
    St(app): St<State>,
    MaybeAuth(s): MaybeAuth,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let r: SwitchRequest = decode_json(&body, "switch_request")?;
    const OAUTH: &[&str] = &["saml", "gitlab", "google", "office365", "openid"];
    let email_to_oauth = r.current_service == "email" && OAUTH.contains(&r.new_service.as_str());
    let oauth_to_email = OAUTH.contains(&r.current_service.as_str()) && r.new_service == "email";
    let email_to_ldap = r.current_service == "email" && r.new_service == "ldap";
    let ldap_to_email = r.current_service == "ldap" && r.new_service == "email";
    let by_email = |e: &str| user_by_email(&app, e.to_lowercase());
    if email_to_oauth || email_to_ldap {
        let user = by_email(&r.email).await?;
        // SwitchEmailToOAuth / SwitchEmailToLdap: the login's checks, lockout and MFA included.
        app.check_password_and_all_criteria(&user.id, &r.password, &r.mfa_code).await?;
        if email_to_ldap {
            return Err(AppError::new(
                StatusCode::NOT_IMPLEMENTED,
                "api.user.email_to_ldap.not_available.app_error",
                "",
            ));
        }
        let (link, cookie) = crate::sso::switch_email_to_oauth(&app, &headers, &r.email, &r.new_service).await?;
        crate::audit::log(&app, s.as_deref(), "success").await;
        let mut resp = crate::sso::go_map_json(&BTreeMap::from([("follow_link".to_string(), link)])).into_response();
        resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(c) = cookie.and_then(|c| HeaderValue::from_str(&c).ok()) {
            resp.headers_mut().append(header::SET_COOKIE, c);
        }
        return Ok(resp);
    }
    if oauth_to_email {
        let s = s.ok_or_else(crate::app::invalid_token)?;
        if s.is_oauth {
            return Err(oauth_denied(AppError::forbidden_perm(EDIT_OTHER_USERS)));
        }
        email_login_available(&app)?;
        let user = by_email(&r.email).await?;
        if user.auth_service == "magic_link" && user.is_guest() {
            return Err(AppError::bad_request("api.user.oauth_to_email.magic_link.app_error", ""));
        }
        if user.id != s.user_id {
            return Err(AppError::forbidden().with_id("api.user.oauth_to_email.context.app_error"));
        }
        if !OAUTH.contains(&user.auth_service.as_str()) {
            return Err(AppError::bad_request("api.user.oauth_to_email.not_oauth_user.app_error", ""));
        }
        app.set_password(&user, &r.new_password, None).await?;
        let (email, locale) = (user.email.clone(), user.locale.clone());
        crate::email::go(&app, "signin change email", move |app| async move {
            let method = crate::i18n::tl(&locale, "api.templates.signin_change_email.body.method_email", &[]);
            crate::email::send_sign_in_change(&app, &email, &method, &locale).await
        });
        app.revoke_all_sessions(&s.user_id).await?;
        crate::audit::log(&app, Some(&s), "success").await;
        return Ok(Json(json!({"follow_link": "/login?extra=signin_change"})).into_response());
    }
    if ldap_to_email {
        email_login_available(&app)?;
        let user = by_email(&r.email).await?;
        if user.auth_service != "ldap" {
            return Err(AppError::bad_request("api.user.ldap_to_email.not_ldap_account.app_error", ""));
        }
        return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.user.ldap_to_email.not_available.app_error", ""));
    }
    Err(AppError::invalid_param("switch_request"))
}

/// `GetUserByEmail`.
async fn user_by_email(app: &App, email: String) -> ApiResult<User> {
    let users = crate::users::query_users(app, "", "u.Email = $1", "", vec![crate::users::Arg::S(email)]).await?;
    users.into_iter().next().ok_or_else(missing_account)
}

fn email_login_available(app: &App) -> ApiResult<()> {
    if !app.cfg.bool("EmailSettings.EnableSignUpWithEmail") {
        return Err(AppError::forbidden().with_id("api.user.auth_switch.not_available.email_signup_disabled.app_error"));
    }
    if !app.cfg.bool("EmailSettings.EnableSignInWithEmail") && !app.cfg.bool("EmailSettings.EnableSignInWithUsername") {
        return Err(AppError::forbidden().with_id("api.user.auth_switch.not_available.login_disabled.app_error"));
    }
    Ok(())
}

fn too_many_attempts() -> AppError {
    AppError::new(StatusCode::UNAUTHORIZED, "api.user.check_user_login_attempts.too_many.app_error", "")
}

fn attempts_error(e: impl ToString) -> AppError {
    AppError::internal("app.user.update_failed_pwd_attempts.app_error", e.to_string())
}

impl App {
    /// `CheckPasswordAndAllCriteria` (login, and the email to SSO/LDAP switches). The attempt takes a lockout slot
    /// first (`TryIncrementFailedPasswordAttempts`, atomic, so parallel guesses cannot pass the limit); a wrong
    /// password or a wrong MFA code keeps it, a missing MFA code (the client asking whether MFA is on) gives it
    /// back; the count goes to 0 only once both password and code are good.
    pub async fn check_password_and_all_criteria(&self, user_id: &str, password: &str, mfa: &str) -> ApiResult<()> {
        let user = match self.user(user_id).await {
            Ok(Some(u)) => u,
            Ok(None) => {
                let mut e = crate::users::missing_account();
                e.status_code = 400;
                return Err(e);
            }
            Err(mut e) => {
                e.status_code = 500;
                return Err(e);
            }
        };
        // CheckUserPreflightAuthenticationCriteria
        if user.delete_at > 0 {
            return Err(AppError::new(StatusCode::UNAUTHORIZED, "api.user.login.inactive.app_error", ""));
        }
        if user.is_bot {
            return Err(AppError::new(StatusCode::UNAUTHORIZED, "api.user.login.bot_login_forbidden.app_error", ""));
        }
        let max = self.cfg.i64("ServiceSettings.MaximumLoginAttempts");
        if user.failed_attempts as i64 >= max {
            return Err(too_many_attempts());
        }
        let claimed =
            sqlx::query("UPDATE Users SET FailedAttempts = FailedAttempts + 1 WHERE Id = $1 AND FailedAttempts < $2")
                .bind(&user.id)
                .bind(max as i32)
                .execute(&self.db)
                .await
                .map_err(attempts_error)?
                .rows_affected()
                == 1;
        self.invalidate_user(&user.id);
        if !claimed {
            return Err(too_many_attempts());
        }
        // checkUserPassword: a wrong (or empty) password keeps the slot.
        match crate::password::check(&user.password, password).await {
            crate::password::Check::Mismatch => {
                return Err(AppError::new(
                    StatusCode::UNAUTHORIZED,
                    "api.user.check_user_password.invalid.app_error",
                    "",
                ));
            }
            crate::password::Check::MatchOld => {
                if let Err(e) = self.store_password(&user.id, &crate::password::hash(password).await).await {
                    self.refund_attempt(&user.id).await;
                    return Err(AppError::internal("app.user.check_user_password.failed_update", e.message));
                }
            }
            crate::password::Check::Match => {}
        }
        if let Err(e) = self.check_user_mfa(&user, mfa).await {
            if mfa.is_empty() {
                self.refund_attempt(&user.id).await;
            }
            return Err(e);
        }
        sqlx::query("UPDATE Users SET FailedAttempts = 0 WHERE Id = $1")
            .bind(&user.id)
            .execute(&self.db)
            .await
            .map_err(attempts_error)?;
        self.invalidate_user(&user.id);
        // CheckUserPostflightAuthenticationCriteria
        if !user.email_verified && self.cfg.bool("EmailSettings.RequireEmailVerification") {
            return Err(AppError::new(StatusCode::UNAUTHORIZED, "api.user.login.not_verified.app_error", ""));
        }
        Ok(())
    }

    /// `DecrementFailedPasswordAttempts`: a slot given back (Go only logs a failure).
    async fn refund_attempt(&self, user_id: &str) {
        let _ =
            sqlx::query("UPDATE Users SET FailedAttempts = FailedAttempts - 1 WHERE Id = $1 AND FailedAttempts > 0")
                .bind(user_id)
                .execute(&self.db)
                .await;
        self.invalidate_user(user_id);
    }

    /// `CheckUserMfa` with `mfa.ValidateToken`: a code of the user's secret, one step either side, never one of the
    /// time steps already used (`MfaUsedTimestamps`, stored again with this one).
    pub async fn check_user_mfa(&self, user: &User, token: &str) -> ApiResult<()> {
        if !user.mfa_active || !self.cfg.bool("ServiceSettings.EnableMultifactorAuthentication") {
            return Ok(());
        }
        let bad_token = |e: String| AppError::bad_request("mfa.validate_token.authenticate.app_error", "").detail(e);
        let used = mfa_used_timestamps(self, &user.id).await.map_err(bad_token)?;
        match mfa_authenticate(&user.mfa_secret, token, now_ms() / 1000, used) {
            Err(e) => Err(bad_token(format!("unable to parse the token: {e}"))),
            Ok(None) => Err(AppError::new(StatusCode::UNAUTHORIZED, "api.user.check_user_mfa.bad_code.app_error", "")),
            Ok(Some(used)) => store_mfa_used_timestamps(self, &user.id, &used)
                .await
                .map_err(|e| bad_token(format!("unable to store the DisallowReuse slice: {e}"))),
        }
    }
}

/// `GetMfaUsedTimestamps`: Go stores them as a JSON array of strings; NULL is none. (Numbers, which older builds
/// of this server wrote, are read too.)
async fn mfa_used_timestamps(app: &App, user_id: &str) -> Result<Vec<i64>, String> {
    let v: Option<sqlx::types::Json<Value>> = sqlx::query_scalar("SELECT MfaUsedTimestamps FROM Users WHERE Id = $1")
        .bind(user_id)
        .fetch_optional(&app.db)
        .await
        .map_err(|e| format!("unable to retrieve the DisallowReuse slice: {e}"))?
        .flatten();
    let Some(Value::Array(a)) = v.map(|j| j.0) else { return Ok(vec![]) };
    a.iter()
        .map(|t| match t {
            Value::String(s) => {
                s.parse::<i64>().map_err(|e| format!("unable to retrieve the DisallowReuse slice: {e}"))
            }
            Value::Number(n) => n.as_i64().ok_or_else(|| "unable to retrieve the DisallowReuse slice".to_string()),
            _ => Err("unable to retrieve the DisallowReuse slice".to_string()),
        })
        .collect()
}

/// `StoreMfaUsedTimestamps`: the time steps as strings, and `UpdateAt`.
async fn store_mfa_used_timestamps(app: &App, user_id: &str, used: &[i64]) -> Result<(), sqlx::Error> {
    let ts: Vec<Value> = used.iter().map(|t| Value::String(t.to_string())).collect();
    sqlx::query("UPDATE Users SET MfaUsedTimestamps = $1, UpdateAt = $2 WHERE Id = $3")
        .bind(sqlx::types::Json(ts))
        .bind(now_ms())
        .bind(user_id)
        .execute(&app.db)
        .await?;
    app.invalidate_user(user_id);
    Ok(())
}

/// `migrateAuthToLDAP`: LDAP needs a license.
async fn migrate_ldap(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    let p = object_from_json(&body);
    let from = p.get("from").and_then(Value::as_str);
    if !from.is_some_and(|f| ["email", "gitlab", "saml", "google", "office365"].contains(&f)) {
        return Err(AppError::invalid_param("from"));
    }
    if !p.get("force").is_some_and(Value::is_boolean) {
        return Err(AppError::invalid_param("force"));
    }
    if !p.get("match_field").is_some_and(Value::is_string) {
        return Err(AppError::invalid_param("match_field"));
    }
    app.require(&s, MANAGE_SYSTEM).await?;
    Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.admin.ldap.not_available.app_error", ""))
}

/// `migrateAuthToSaml`: SAML needs a license.
async fn migrate_saml(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    let p = object_from_json(&body);
    let from = p.get("from").and_then(Value::as_str);
    if !from.is_some_and(|f| ["email", "gitlab", "ldap", "google", "office365"].contains(&f)) {
        return Err(AppError::invalid_param("from"));
    }
    if !p.get("auto").is_some_and(Value::is_boolean) {
        return Err(AppError::invalid_param("auto"));
    }
    if !p.get("matches").is_some_and(Value::is_object) {
        return Err(AppError::invalid_param("matches"));
    }
    app.require(&s, MANAGE_SYSTEM).await?;
    Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.admin.saml.not_available.app_error", ""))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NotifyAdminRequest {
    trial_notification: bool,
    required_plan: String,
    required_feature: String,
}

/// `handleNotifyAdmin` (`SaveAdminNotification`): a user asking admins for a paid feature, once per feature.
async fn notify_admin(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    let r: Option<NotifyAdminRequest> = serde_json::Deserializer::from_slice(&body)
        .into_iter::<Option<NotifyAdminRequest>>()
        .next()
        .ok_or_else(|| AppError::invalid_param("notifyAdminRequest").detail("EOF"))?
        .map_err(|e| AppError::invalid_param("notifyAdminRequest").detail(e.to_string()))?;
    let r = r.ok_or_else(|| AppError::invalid_param("notifyAdminRequest"))?;
    let (already,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM NotifyAdmin WHERE UserId = $1 AND RequiredFeature = $2)")
            .bind(&s.user_id)
            .bind(&r.required_feature)
            .fetch_one(&app.db)
            .await?;
    if already {
        return Err(AppError::forbidden().with_id("api.cloud.notify_admin_to_upgrade_error.already_notified"));
    }
    const FEATURES: &[&str] = &[
        "mattermost.feature.guest_accounts",
        "mattermost.feature.custom_user_groups",
        "mattermost.feature.create_multiple_teams",
        "mattermost.feature.start_call",
        "mattermost.feature.playbooks_retro",
        "mattermost.feature.unlimited_messages",
        "mattermost.feature.unlimited_file_storage",
        "mattermost.feature.all_professional",
        "mattermost.feature.all_enterprise",
        "mattermost.feature.upgrade_downgraded_workspace",
        "mattermost.feature.highlight_without_notification",
    ];
    let save_err = |d: String| AppError::internal("app.notify_admin.save.app_error", d);
    if !r.required_feature.starts_with("mattermost.feature.plugin") {
        if !["professional", "enterprise"].contains(&r.required_plan.as_str()) {
            return Err(save_err(format!("Invalid plan, {} provided", r.required_plan)));
        }
        if !FEATURES.contains(&r.required_feature.as_str()) {
            return Err(save_err(format!("Invalid feature, {} provided", r.required_feature)));
        }
    }
    sqlx::query(
        "INSERT INTO NotifyAdmin (UserId, CreateAt, RequiredPlan, RequiredFeature, Trial) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&s.user_id)
    .bind(now_ms())
    .bind(&r.required_plan)
    .bind(&r.required_feature)
    .bind(r.trial_notification)
    .execute(&app.db)
    .await
    .map_err(|e| save_err(e.to_string()))?;
    Ok(ok())
}

/// `handleTriggerNotifyAdminPosts`: only with `EnableAPITriggerAdminNotifications`, which this server does not
/// act on (the admin notification job is Cloud's).
async fn trigger_notify_admin(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    if !app.cfg.bool("ServiceSettings.EnableAPITriggerAdminNotifications") {
        return Err(AppError::forbidden().with_id("api.cloud.app_error"));
    }
    let r: Option<NotifyAdminRequest> = serde_json::from_slice(&body)
        .map_err(|e| AppError::invalid_param("notifyAdminRequest").detail(e.to_string()))?;
    r.ok_or_else(|| AppError::invalid_param("notifyAdminRequest"))?;
    app.require(&s, MANAGE_SYSTEM).await?;
    Err(AppError::not_implemented("Admin notification posts"))
}

// ---------- TOTP as dgoogauth checks it ----------

/// `base32.StdEncoding.EncodeToString`.
fn base32_std(data: &[u8]) -> String {
    const A: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    for chunk in data.chunks(5) {
        let mut buf = [0u8; 5];
        buf[..chunk.len()].copy_from_slice(chunk);
        let v = u64::from_be_bytes([0, 0, 0, buf[0], buf[1], buf[2], buf[3], buf[4]]);
        let chars = (chunk.len() * 8).div_ceil(5);
        for i in 0..8 {
            if i < chars {
                out.push(A[((v >> (35 - i * 5)) & 31) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let (mut out, mut acc, mut bits) = (vec![], 0u64, 0);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        } as u64;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn totp_code(key: &[u8], t: i64) -> u32 {
    use hmac::{KeyInit, Mac};
    let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(key).expect("any key length");
    mac.update(&(t as u64).to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 0xf) as usize;
    u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]]) % 1_000_000
}

/// `mfa.authenticate`: dgoogauth's `OTPConfig.Authenticate` of the trimmed code, with a window of 3 and
/// `DisallowReuse` (always on: Go reads the used steps into a non-nil slice). `Err` for a code that is not six
/// digits (or eight, a scratch code, which never matches), `Ok(None)` for a wrong or reused code, `Ok(Some)` with
/// the used time steps to store for a good one.
fn mfa_authenticate(secret: &str, token: &str, now_secs: i64, mut used: Vec<i64>) -> Result<Option<Vec<i64>>, String> {
    const INVALID: &str = "invalid code";
    let token = token.trim();
    let b = token.as_bytes();
    let scratch = match b.len() {
        6 if b[0].is_ascii_digit() => false,
        8 if (b'1'..=b'9').contains(&b[0]) => true,
        _ => return Err(INVALID.into()),
    };
    if !b.iter().all(u8::is_ascii_digit) {
        return Err(INVALID.into());
    }
    if scratch {
        return Ok(None);
    }
    let code: u32 = token.parse().map_err(|_| INVALID.to_string())?;
    // ComputeCode is -1, never a match, for a secret that is not base32.
    let Some(key) = base32_decode(secret) else { return Ok(None) };
    let t0 = now_secs / 30;
    let Some(t) = (t0 - 1..=t0 + 1).find(|t| totp_code(&key, *t) == code) else { return Ok(None) };
    if used.contains(&t) {
        return Ok(None);
    }
    used.push(t);
    used.sort();
    used.retain(|u| *u >= t0 - 1);
    Ok(Some(used))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_sha1() {
        // RFC 6238 test secret "12345678901234567890", T = 59 s → 94287082 (8 digits) → 287082.
        assert_eq!(totp_code(b"12345678901234567890", 59 / 30), 287082);
    }

    /// dgoogauth's `Authenticate` with Mattermost's window and reuse list.
    #[test]
    fn mfa_codes_like_dgoogauth() {
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"; // "12345678901234567890"
        let key = base32_decode(secret).unwrap();
        let now = 1_700_000_000i64;
        let t0 = now / 30;
        let code = |t: i64| format!("{:06}", totp_code(&key, t));
        // Good code: its step is stored, steps before the window dropped.
        assert_eq!(mfa_authenticate(secret, &code(t0), now, vec![t0 - 5, t0 - 1]), Ok(Some(vec![t0 - 1, t0])));
        // One step either side is good; two is not.
        assert_eq!(mfa_authenticate(secret, &code(t0 + 1), now, vec![]), Ok(Some(vec![t0 + 1])));
        assert_eq!(mfa_authenticate(secret, &code(t0 - 1), now, vec![]), Ok(Some(vec![t0 - 1])));
        assert_eq!(mfa_authenticate(secret, &code(t0 + 2), now, vec![]), Ok(None));
        // A code already used is refused (replay).
        assert_eq!(mfa_authenticate(secret, &code(t0), now, vec![t0]), Ok(None));
        // Spaces around are trimmed.
        assert_eq!(mfa_authenticate(secret, &format!(" {} ", code(t0)), now, vec![]), Ok(Some(vec![t0])));
        // Not a code: an error (Go's 400), not a wrong code (401); eight digits are a scratch code, never good.
        for bad in ["", "12345", "1234567", "12345a", "abcdef", "-12345", "01234567"] {
            assert_eq!(mfa_authenticate(secret, bad, now, vec![]), Err("invalid code".into()), "{bad:?}");
        }
        assert_eq!(mfa_authenticate(secret, "12345678", now, vec![]), Ok(None));
        // A secret that is not base32 never matches.
        assert_eq!(mfa_authenticate("not base32!", &code(t0), now, vec![]), Ok(None));
    }

    #[test]
    fn roles() {
        assert!(valid_user_roles("system_user"));
        assert!(valid_user_roles("system_admin system_user"));
        assert!(!valid_user_roles("system_admin"));
        assert!(!valid_user_roles("System_user"));
    }
    #[test]
    fn escape() {
        assert_eq!(query_escape("localhost:8065/a b"), "localhost%3A8065%2Fa+b");
    }
    #[test]
    fn qr() {
        let png = qr_png("otpauth://totp/x:y?secret=ABC&issuer=x").unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }
}
