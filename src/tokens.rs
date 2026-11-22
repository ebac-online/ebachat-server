//! Personal access tokens: create, list, search, enable/disable/revoke/rotate, and the lifetime policy.

// Based on Mattermost server/channels/api4/user.go (the token handlers), app/session.go and
// store/sqlstore/user_access_token_store.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;

use crate::app::Query;
use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, State, decode_json, is_valid_id, map_from_json, new_id, now_ms};
use crate::channels_api::paging;
use crate::model::Session;
use crate::perm::MANAGE_SYSTEM;
use crate::preferences::user_param;
use crate::user_edit::user_or_bot_perm;
use crate::users::{missing_account, ok};

const CREATE: &str = "create_user_access_token";
const READ: &str = "read_user_access_token";
const REVOKE: &str = "revoke_user_access_token";

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/users/{user_id}/tokens", post(create).get(for_user))
        .route("/api/v4/users/tokens", get(all))
        .route("/api/v4/users/tokens/search", post(search))
        .route("/api/v4/users/tokens/non_compliant/count", get(count_non_compliant))
        .route("/api/v4/users/tokens/non_compliant/revoke", post(revoke_non_compliant))
        .route("/api/v4/users/tokens/{token_id}", get(get_one))
        .route("/api/v4/users/tokens/revoke", post(revoke))
        .route("/api/v4/users/tokens/disable", post(disable))
        .route("/api/v4/users/tokens/enable", post(enable))
        .route("/api/v4/users/tokens/rotate", post(rotate))
}

/// `model.UserAccessToken`.
#[derive(Serialize, Deserialize, Default, Clone, sqlx::FromRow)]
#[serde(default)]
pub struct UserAccessToken {
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub token: String,
    pub user_id: String,
    pub description: String,
    pub is_active: bool,
    pub expires_at: i64,
}

const SELECT: &str = "SELECT t.Id AS id, COALESCE(t.Token, '') AS token, COALESCE(t.UserId, '') AS user_id,
    COALESCE(t.Description, '') AS description, COALESCE(t.IsActive, false) AS is_active, t.ExpiresAt AS expires_at
    FROM UserAccessTokens t";

fn not_found() -> AppError {
    AppError::not_found("app.user_access_token.get_by_user.app_error", "Unable to get user access tokens by user.")
}

fn oauth_denied(p: &str) -> AppError {
    AppError::forbidden_perm(p).detail(format!("userId=, permission={p}, attempted access by oauth app"))
}

impl App {
    /// `UserAccessTokenStore.Get`.
    pub async fn access_token(&self, id: &str) -> ApiResult<UserAccessToken> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} WHERE t.Id = $1")))
            .bind(id)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(not_found)
    }

    /// `MaximumPersonalAccessTokenLifetimeDays` as `maxUserAccessTokenExpiry`: the latest compliant expiry.
    fn max_token_expiry(&self) -> Option<i64> {
        let days = self.cfg.i64("ServiceSettings.MaximumPersonalAccessTokenLifetimeDays");
        (days > 0).then(|| now_ms() + days * 86_400_000)
    }

    /// `enforceUserAccessTokenExpiryPolicy`: plugin-owned bots are exempt.
    async fn check_token_expiry(&self, user: &crate::model::User, expires_at: i64) -> ApiResult<()> {
        let days = self.cfg.i64("ServiceSettings.MaximumPersonalAccessTokenLifetimeDays").max(0);
        let err = if expires_at == 0 {
            (days > 0).then(|| AppError::bad_request("app.user_access_token.expires_at_required.app_error", ""))
        } else if expires_at <= now_ms() {
            Some(AppError::bad_request("app.user_access_token.expires_at_in_past.app_error", ""))
        } else if days > 0 && expires_at > now_ms() + days * 86_400_000 {
            Some(
                AppError::bad_request("app.user_access_token.expires_at_too_far.app_error", "")
                    .with_param("Days", &days.to_string()),
            )
        } else {
            None
        };
        let Some(err) = err else { return Ok(()) };
        if !user.is_bot {
            return Err(err);
        }
        // userAccessTokenExpiryPolicyApplies: an owner that exists, or one that looks like a user id.
        let owner: Option<(String,)> = sqlx::query_as("SELECT COALESCE(OwnerId, '') FROM Bots WHERE UserId = $1")
            .bind(&user.id)
            .fetch_optional(&self.db)
            .await?;
        let Some((owner,)) = owner else { return Err(crate::bots::bot_not_found()) };
        if is_valid_id(&owner) { Err(err) } else { Ok(()) }
    }

    /// `CreateUserAccessToken`.
    pub async fn create_access_token(
        &self,
        user_id: &str,
        description: &str,
        expires_at: i64,
    ) -> ApiResult<UserAccessToken> {
        let user = self.user(user_id).await?.ok_or_else(missing_account)?;
        if !self.cfg.bool("ServiceSettings.EnableUserAccessTokens") && !user.is_bot {
            return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "app.user_access_token.disabled", ""));
        }
        self.check_token_expiry(&user, expires_at).await?;
        let t = UserAccessToken {
            id: new_id(),
            token: new_id(),
            user_id: user_id.into(),
            description: description.into(),
            is_active: true,
            expires_at,
        };
        // UserAccessToken.IsValid
        if description.chars().count() > 255 {
            return Err(AppError::bad_request("model.user_access_token.is_valid.description.app_error", ""));
        }
        if expires_at < 0 {
            return Err(AppError::bad_request("model.user_access_token.is_valid.expires_at.app_error", ""));
        }
        sqlx::query(
            "INSERT INTO UserAccessTokens (Id, Token, UserId, Description, IsActive, ExpiresAt) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&t.id)
        .bind(&t.token)
        .bind(&t.user_id)
        .bind(&t.description)
        .bind(t.is_active)
        .bind(t.expires_at)
        .execute(&self.db)
        .await
        .map_err(|e| AppError::internal("app.user_access_token.save.app_error", e.to_string()))?;
        if !user.is_bot
            && let Err(e) = crate::email::send_access_token_added(self, &user.email, &user.locale).await
        {
            eprintln!("Unable to send user access token added email: {e}");
        }
        Ok(t)
    }

    /// After a token's sessions were deleted: `RevokeSession` of the one made from its secret (the user's cached
    /// sessions cleared, its websockets closed).
    fn forget_token_session(&self, t: &UserAccessToken) {
        self.clear_sessions_for_user(&t.user_id);
        self.caches.session.remove(&t.token);
        self.hub.close_session(&t.token);
    }

    /// `RevokeUserAccessToken`: the token and its sessions deleted.
    pub async fn revoke_access_token(&self, t: &UserAccessToken) -> ApiResult<()> {
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM Sessions s USING UserAccessTokens o WHERE o.Token = s.Token AND o.Id = $1")
            .bind(&t.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM UserAccessTokens WHERE Id = $1").bind(&t.id).execute(&mut *tx).await?;
        tx.commit().await?;
        self.forget_token_session(t);
        Ok(())
    }

    /// `DeleteAllForUser`: every token of the user and the sessions made from them.
    pub async fn delete_access_tokens_for_user(&self, user_id: &str) -> ApiResult<()> {
        let tokens: Vec<(String,)> =
            sqlx::query_as("SELECT COALESCE(Token, '') FROM UserAccessTokens WHERE UserId = $1")
                .bind(user_id)
                .fetch_all(&self.db)
                .await?;
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM Sessions s USING UserAccessTokens o WHERE o.Token = s.Token AND o.UserId = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM UserAccessTokens WHERE UserId = $1").bind(user_id).execute(&mut *tx).await?;
        tx.commit().await?;
        for (t,) in tokens {
            self.caches.session.remove(&t);
            self.hub.close_session(&t);
        }
        Ok(())
    }
}

/// `SqlStore` LIKE escaping of `sanitizeSearchTerm(term, "\\")`.
pub fn sanitize_search_term(term: &str) -> String {
    term.replace('\\', "").replace('%', "\\%").replace('_', "\\_")
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NewToken {
    description: String,
    expires_at: i64,
}

/// `createUserAccessToken`.
async fn create(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<UserAccessToken>> {
    let user_id = user_param(&s, &user_id)?;
    let user = app.user(&user_id).await?.ok_or_else(missing_account)?;
    if user.remote_id.as_deref().is_some_and(|r| !r.is_empty()) {
        return Err(AppError::forbidden_perm(CREATE));
    }
    if s.is_oauth {
        return Err(oauth_denied(CREATE));
    }
    let t: NewToken = decode_json(&body, "user_access_token")?;
    if t.description.is_empty() {
        return Err(AppError::invalid_param("description"));
    }
    crate::audit::log(&app, Some(&s), "").await;
    app.require(&s, CREATE).await?;
    if !user_or_bot_perm(&app, &s, &user_id).await? {
        return Err(AppError::forbidden_perm(crate::perm::EDIT_OTHER_USERS));
    }
    if user.is_system_admin() {
        app.require(&s, MANAGE_SYSTEM).await?;
    }
    let t = app.create_access_token(&user_id, &t.description, t.expires_at).await?;
    crate::audit::log(&app, Some(&s), &format!("success - token_id={}", t.id)).await;
    Ok(Json(t))
}

type Q = Query<HashMap<String, String>>;

/// `getUserAccessTokens`: every token, without the secrets.
async fn all(St(app): St<State>, Auth(s): Auth, Query(q): Q) -> ApiResult<Json<Vec<UserAccessToken>>> {
    app.require(&s, MANAGE_SYSTEM).await?;
    let (page, per_page) = paging(&q);
    let mut v: Vec<UserAccessToken> = sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} LIMIT $1 OFFSET $2")))
        .bind(per_page)
        .bind(page * per_page)
        .fetch_all(&app.db)
        .await?;
    v.iter_mut().for_each(|t| t.token.clear());
    Ok(Json(v))
}

/// `getUserAccessTokensForUser`.
async fn for_user(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    Query(q): Q,
) -> ApiResult<Json<Vec<UserAccessToken>>> {
    let user_id = user_param(&s, &user_id)?;
    app.require(&s, READ).await?;
    if !user_or_bot_perm(&app, &s, &user_id).await? {
        return Err(AppError::forbidden_perm(crate::perm::EDIT_OTHER_USERS));
    }
    let (page, per_page) = paging(&q);
    let mut v: Vec<UserAccessToken> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} WHERE t.UserId = $1 LIMIT $2 OFFSET $3")))
            .bind(&user_id)
            .bind(per_page)
            .bind(page * per_page)
            .fetch_all(&app.db)
            .await?;
    v.iter_mut().for_each(|t| t.token.clear());
    Ok(Json(v))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SearchBody {
    term: String,
}

/// `searchUserAccessTokens`: the id, the user id or the username, as LIKE patterns without wildcards.
async fn search(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Vec<UserAccessToken>>> {
    app.require(&s, MANAGE_SYSTEM).await?;
    let b: SearchBody = decode_json(&body, "user_access_token_search")?;
    if b.term.is_empty() {
        return Err(AppError::invalid_param("term"));
    }
    let term = sanitize_search_term(&b.term);
    let mut v: Vec<UserAccessToken> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} INNER JOIN Users u ON t.UserId = u.Id WHERE t.Id LIKE $1 OR t.UserId LIKE $1 OR u.Username LIKE $1"
    )))
    .bind(&term)
    .fetch_all(&app.db)
    .await?;
    v.iter_mut().for_each(|t| t.token.clear());
    Ok(Json(v))
}

const NON_COMPLIANT: &str = "FROM UserAccessTokens t LEFT JOIN Bots b ON b.UserId = t.UserId
    WHERE (t.ExpiresAt = 0 OR t.ExpiresAt > $1) AND t.IsActive = true
      AND (b.UserId IS NULL OR b.OwnerId ~ '^[0-9A-Za-z]{26}$')";

/// `countNonCompliantUserAccessTokens`.
async fn count_non_compliant(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    app.require(&s, MANAGE_SYSTEM).await?;
    let Some(max) = app.max_token_expiry() else { return Ok(Json(json!({ "count": 0 }))) };
    let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) {NON_COMPLIANT}")))
        .bind(max)
        .fetch_one(&app.db)
        .await
        .map_err(|e| AppError::internal("app.user_access_token.count_non_compliant.app_error", e.to_string()))?;
    Ok(Json(json!({ "count": n })))
}

/// `revokeNonCompliantUserAccessTokens`: in batches of 1000, sessions too.
async fn revoke_non_compliant(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    app.require(&s, MANAGE_SYSTEM).await?;
    let Some(max) = app.max_token_expiry() else {
        return Err(AppError::bad_request("app.user_access_token.revoke_non_compliant.no_policy.app_error", ""));
    };
    let mut total = 0i64;
    for _ in 0..1000 {
        let rows: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "WITH to_delete AS (SELECT t.Id, t.Token, t.UserId {NON_COMPLIANT} LIMIT 1000),
             deleted_sessions AS (DELETE FROM Sessions WHERE Token IN (SELECT Token FROM to_delete)),
             deleted_tokens AS (DELETE FROM UserAccessTokens WHERE Id IN (SELECT Id FROM to_delete) RETURNING UserId, Token)
             SELECT COALESCE(UserId, ''), COALESCE(Token, '') FROM deleted_tokens"
        )))
        .bind(max)
        .fetch_all(&app.db)
        .await
        .map_err(|e| AppError::internal("app.user_access_token.delete.app_error", e.to_string()))?;
        total += rows.len() as i64;
        for (user_id, token) in &rows {
            app.clear_sessions_for_user(user_id);
            app.hub.close_session(token);
        }
        if rows.len() < 1000 {
            return Ok(Json(json!({ "count": total })));
        }
    }
    Err(AppError::internal("app.user_access_token.revoke_non_compliant.partial.app_error", ""))
}

/// `getUserAccessToken`.
async fn get_one(St(app): St<State>, Auth(s): Auth, Path(token_id): Path<String>) -> ApiResult<Json<UserAccessToken>> {
    if !is_valid_id(&token_id) {
        return Err(AppError::invalid_url_param("token_id"));
    }
    app.require(&s, READ).await?;
    let mut t = app.access_token(&token_id).await?;
    t.token.clear();
    if !user_or_bot_perm(&app, &s, &t.user_id).await? {
        return Err(AppError::forbidden_perm(crate::perm::EDIT_OTHER_USERS));
    }
    Ok(Json(t))
}

/// The common start of revoke/disable/enable: the token of `token_id` the session may manage. Go carries on
/// after a missing `token_id` (and answers with a later error); this answers the missing parameter.
async fn managed_token(app: &App, s: &Session, body: &[u8], perm: &str) -> ApiResult<UserAccessToken> {
    let token_id = map_from_json(body).remove("token_id").unwrap_or_default();
    // Go logs its audit row even for a missing `token_id`, then carries on.
    crate::audit::log(app, Some(s), "").await;
    if token_id.is_empty() {
        return Err(AppError::invalid_param("token_id"));
    }
    if s.is_oauth {
        return Err(oauth_denied(perm));
    }
    app.require(s, perm).await?;
    let t = app.access_token(&token_id).await?;
    if !user_or_bot_perm(app, s, &t.user_id).await? {
        return Err(AppError::forbidden_perm(crate::perm::EDIT_OTHER_USERS));
    }
    Ok(t)
}

/// `revokeUserAccessToken`.
async fn revoke(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    let t = managed_token(&app, &s, &body, REVOKE).await?;
    app.revoke_access_token(&t).await?;
    crate::audit::log(&app, Some(&s), &format!("success - token_id={}", t.id)).await;
    Ok(ok())
}

/// `disableUserAccessToken`: inactive, its sessions deleted.
async fn disable(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    let t = managed_token(&app, &s, &body, REVOKE).await?;
    let mut tx = app.db.begin().await?;
    sqlx::query("DELETE FROM Sessions s USING UserAccessTokens o WHERE o.Token = s.Token AND o.Id = $1")
        .bind(&t.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE UserAccessTokens SET IsActive = FALSE WHERE Id = $1").bind(&t.id).execute(&mut *tx).await?;
    tx.commit()
        .await
        .map_err(|e| AppError::internal("app.user_access_token.update_token_disable.app_error", e.to_string()))?;
    app.forget_token_session(&t);
    crate::audit::log(&app, Some(&s), &format!("success - token_id={}", t.id)).await;
    Ok(ok())
}

/// `enableUserAccessToken`.
async fn enable(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    let t = managed_token(&app, &s, &body, CREATE).await?;
    sqlx::query("UPDATE UserAccessTokens SET IsActive = TRUE WHERE Id = $1")
        .bind(&t.id)
        .execute(&app.db)
        .await
        .map_err(|e| AppError::internal("app.user_access_token.update_token_enable.app_error", e.to_string()))?;
    crate::audit::log(&app, Some(&s), &format!("success - token_id={}", t.id)).await;
    Ok(ok())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RotateBody {
    token_id: String,
    expires_at: i64,
}

/// `rotateUserAccessToken`: a new secret and expiry, the old secret's sessions gone.
async fn rotate(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<UserAccessToken>> {
    let b: RotateBody = decode_json(&body, "rotate_user_access_token")?;
    if b.token_id.is_empty() {
        return Err(AppError::invalid_param("token_id"));
    }
    crate::audit::log(&app, Some(&s), "").await;
    if s.is_oauth {
        return Err(oauth_denied(CREATE));
    }
    app.require(&s, CREATE).await?;
    let mut t = app.access_token(&b.token_id).await?;
    let user = app.user(&t.user_id).await?.ok_or_else(missing_account)?;
    if !user_or_bot_perm(&app, &s, &t.user_id).await? {
        return Err(AppError::forbidden_perm(crate::perm::EDIT_OTHER_USERS));
    }
    if user.is_system_admin() {
        app.require(&s, MANAGE_SYSTEM).await?;
    }
    if user.remote_id.as_deref().is_some_and(|r| !r.is_empty()) {
        return Err(AppError::forbidden_perm(CREATE));
    }
    if !t.is_active {
        return Err(AppError::bad_request("api.user.rotate_user_access_token.disabled_token.app_error", ""));
    }
    if !app.cfg.bool("ServiceSettings.EnableUserAccessTokens") && !user.is_bot {
        return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "app.user_access_token.disabled", ""));
    }
    app.check_token_expiry(&user, b.expires_at).await?;
    let secret = new_id();
    let mut tx = app.db.begin().await?;
    sqlx::query("DELETE FROM Sessions s USING UserAccessTokens o WHERE o.Token = s.Token AND o.Id = $1")
        .bind(&t.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE UserAccessTokens SET Token = $1, ExpiresAt = $2 WHERE Id = $3")
        .bind(&secret)
        .bind(b.expires_at)
        .bind(&t.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await.map_err(|e| AppError::internal("app.user_access_token.rotate.app_error", e.to_string()))?;
    app.forget_token_session(&t);
    t.token = secret;
    t.expires_at = b.expires_at;
    if !user.is_bot
        && let Err(e) = crate::email::send_access_token_rotated(&app, &user.email, &user.locale).await
    {
        eprintln!("Unable to send user access token rotated email: {e}");
    }
    crate::audit::log(&app, Some(&s), &format!("success - token_id={}", t.id)).await;
    Ok(Json(t))
}

#[cfg(test)]
mod tests {
    #[test]
    fn search_term() {
        assert_eq!(super::sanitize_search_term(r"a_b%c\d"), r"a\_b\%cd");
    }
}
