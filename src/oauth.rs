//! OAuth 2.0 apps this server is the provider for: register, list, update, delete, regenerate the secret, and the
//! apps a user authorized. Dynamic client registration answers as a server with it disabled.

// Based on Mattermost server/channels/api4/oauth.go, app/oauth.go, model/oauth.go and
// store/sqlstore/oauth_store.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;

use crate::app::Query;
use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, State, decode_json, is_valid_id, new_id, now_ms};
use crate::channels_api::paging;
use crate::model::JsonText;
use crate::perm::{MANAGE_OAUTH, MANAGE_SYSTEM};
use crate::preferences::user_param;
use crate::users::ok;

const MANAGE_SYSTEM_WIDE_OAUTH: &str = "manage_system_wide_oauth";

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/oauth/apps", post(create).get(list))
        .route("/api/v4/oauth/apps/register", post(register))
        .route("/api/v4/oauth/apps/{app_id}", get(get_one).put(update).delete(delete))
        .route("/api/v4/oauth/apps/{app_id}/info", get(info))
        .route("/api/v4/oauth/apps/{app_id}/regen_secret", post(regen_secret))
        .route("/api/v4/users/{user_id}/oauth/apps/authorized", get(authorized))
}

/// `model.OAuthApp`.
#[derive(Serialize, Deserialize, Default, Clone, sqlx::FromRow)]
#[serde(default)]
pub struct OAuthApp {
    pub id: String,
    pub creator_id: String,
    pub create_at: i64,
    pub update_at: i64,
    pub client_secret: String,
    pub name: String,
    pub description: String,
    pub icon_url: String,
    pub callback_urls: JsonText<Vec<String>>,
    pub homepage: String,
    pub is_trusted: bool,
    pub mattermost_app_id: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_dynamically_registered: bool,
}

pub(crate) const SELECT: &str =
    "SELECT o.Id AS id, COALESCE(o.CreatorId, '') AS creator_id, COALESCE(o.CreateAt, 0) AS create_at,
    COALESCE(o.UpdateAt, 0) AS update_at, COALESCE(o.ClientSecret, '') AS client_secret, COALESCE(o.Name, '') AS name,
    COALESCE(o.Description, '') AS description, COALESCE(o.IconURL, '') AS icon_url,
    COALESCE(o.CallbackUrls, '') AS callback_urls, COALESCE(o.Homepage, '') AS homepage,
    COALESCE(o.IsTrusted, false) AS is_trusted, o.MattermostAppID AS mattermost_app_id,
    COALESCE(o.IsDynamicallyRegistered, false) AS is_dynamically_registered
    FROM OAuthApps o";

/// `IsValidHTTPURL`.
pub fn valid_http_url(u: &str) -> bool {
    (u.starts_with("http://") || u.starts_with("https://")) && !u.chars().any(|c| c.is_whitespace() || c.is_control())
}

impl OAuthApp {
    /// `OAuthApp.IsValid`.
    fn check(&self) -> ApiResult<()> {
        let bad = |f: &str| AppError::bad_request(&format!("model.oauth.is_valid.{f}.app_error"), "");
        if !is_valid_id(&self.id) {
            return Err(bad("app_id"));
        }
        if self.create_at == 0 {
            return Err(bad("create_at"));
        }
        if self.update_at == 0 {
            return Err(bad("update_at"));
        }
        if !is_valid_id(&self.creator_id) && !self.is_dynamically_registered {
            return Err(bad("creator_id"));
        }
        if self.client_secret.len() > 128 {
            return Err(bad("client_secret"));
        }
        if self.name.is_empty() || self.name.len() > 64 {
            return Err(bad("name"));
        }
        // fmt.Sprintf("%s", urls): "[a b]".
        let urls = &self.callback_urls.0;
        if urls.is_empty() || urls.join(" ").len() + 2 > 1024 || urls.iter().any(|u| !valid_http_url(u)) {
            return Err(bad("callback"));
        }
        if self.homepage.is_empty() && !self.is_dynamically_registered {
            return Err(bad("homepage"));
        }
        if !self.homepage.is_empty() && (self.homepage.len() > 256 || !valid_http_url(&self.homepage)) {
            return Err(bad("homepage"));
        }
        if self.description.chars().count() > 512 {
            return Err(bad("description"));
        }
        if !self.icon_url.is_empty() && (self.icon_url.len() > 512 || !valid_http_url(&self.icon_url)) {
            return Err(bad("icon_url"));
        }
        if self.mattermost_app_id.len() > 32 {
            return Err(bad("mattermost_app_id"));
        }
        Ok(())
    }
    fn sanitize(mut self) -> OAuthApp {
        self.client_secret.clear();
        self
    }
}

impl App {
    fn oauth_on(&self, id: &str) -> ApiResult<()> {
        if self.cfg.bool("ServiceSettings.EnableOAuthServiceProvider") {
            Ok(())
        } else {
            Err(AppError::new(StatusCode::NOT_IMPLEMENTED, id, ""))
        }
    }

    /// `GetOAuthApp`.
    pub async fn oauth_app(&self, id: &str) -> ApiResult<OAuthApp> {
        self.oauth_on("api.oauth.allow_oauth.turn_off.app_error")?;
        sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} WHERE o.Id = $1")))
            .bind(id)
            .fetch_optional(&self.db)
            .await
            .map_err(|e| AppError::internal("app.oauth.get_app.finding.app_error", e.to_string()))?
            .ok_or_else(|| AppError::not_found("app.oauth.get_app.find.app_error", ""))
    }

    /// `SqlOAuthStore.UpdateApp`: creation time and creator kept from the stored app.
    async fn store_oauth_app(&self, mut a: OAuthApp) -> ApiResult<OAuthApp> {
        a.update_at = now_ms();
        a.check()?;
        let old = self
            .oauth_app(&a.id)
            .await
            .map_err(|_| AppError::bad_request("app.oauth.update_app.find.app_error", ""))?;
        a.create_at = old.create_at;
        a.creator_id = old.creator_id;
        sqlx::query(
            "UPDATE OAuthApps SET UpdateAt = $1, ClientSecret = $2, Name = $3, Description = $4, IconURL = $5,
                 CallbackUrls = $6, Homepage = $7, IsTrusted = $8, MattermostAppID = $9, IsDynamicallyRegistered = $10
             WHERE Id = $11",
        )
        .bind(a.update_at)
        .bind(&a.client_secret)
        .bind(&a.name)
        .bind(&a.description)
        .bind(&a.icon_url)
        .bind(&a.callback_urls)
        .bind(&a.homepage)
        .bind(a.is_trusted)
        .bind(&a.mattermost_app_id)
        .bind(a.is_dynamically_registered)
        .bind(&a.id)
        .execute(&self.db)
        .await
        .map_err(|e| AppError::internal("app.oauth.update_app.updating.app_error", e.to_string()))?;
        Ok(a)
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AppRequest {
    name: String,
    description: String,
    icon_url: String,
    callback_urls: Option<Vec<String>>,
    homepage: String,
    is_trusted: bool,
    is_public: bool,
}

/// `createOAuthApp`: trusted only by system admins; a public client has no secret.
async fn create(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Response> {
    let r: AppRequest = decode_json(&body, "oauth_app")?;
    if s.is_oauth {
        let e = AppError::forbidden_perm(MANAGE_OAUTH);
        let d = format!("{}, attempted access by oauth app", e.detailed_error);
        return Err(e.detail(d));
    }
    app.require(&s, MANAGE_OAUTH).await?;
    let trusted = r.is_trusted && app.has_perm(&s, MANAGE_SYSTEM).await;
    app.oauth_on("api.oauth.register_oauth_app.turn_off.app_error")?;
    let now = now_ms();
    let a = OAuthApp {
        id: new_id(),
        creator_id: s.user_id.clone(),
        create_at: now,
        update_at: now,
        client_secret: if r.is_public { String::new() } else { new_id() },
        name: r.name,
        description: r.description,
        icon_url: r.icon_url,
        callback_urls: JsonText(r.callback_urls.unwrap_or_default()),
        homepage: r.homepage,
        is_trusted: trusted,
        ..Default::default()
    };
    a.check()?;
    sqlx::query(
        "INSERT INTO OAuthApps (Id, CreatorId, CreateAt, UpdateAt, ClientSecret, Name, Description, IconURL, CallbackUrls,
             Homepage, IsTrusted, MattermostAppID, IsDynamicallyRegistered)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(&a.id)
    .bind(&a.creator_id)
    .bind(a.create_at)
    .bind(a.update_at)
    .bind(&a.client_secret)
    .bind(&a.name)
    .bind(&a.description)
    .bind(&a.icon_url)
    .bind(&a.callback_urls)
    .bind(&a.homepage)
    .bind(a.is_trusted)
    .bind(&a.mattermost_app_id)
    .bind(a.is_dynamically_registered)
    .execute(&app.db)
    .await
    .map_err(|e| AppError::internal("app.oauth.save_app.save.app_error", e.to_string()))?;
    crate::audit::log(&app, Some(&s), &format!("client_id={}", a.id)).await;
    Ok(crate::oauth_provider::go_encode(StatusCode::CREATED, &a))
}

fn require_app_id(id: &str) -> ApiResult<()> {
    if is_valid_id(id) { Ok(()) } else { Err(AppError::invalid_url_param("app_id")) }
}

/// The app the session may manage: its own, or any with `manage_system_wide_oauth`.
async fn managed(app: &App, s: &crate::model::Session, id: &str) -> ApiResult<OAuthApp> {
    let a = app.oauth_app(id).await?;
    if a.creator_id != s.user_id {
        app.require(s, MANAGE_SYSTEM_WIDE_OAUTH).await?;
    }
    Ok(a)
}

/// `updateOAuthApp`.
async fn update(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<crate::oauth_provider::GoJson<OAuthApp>> {
    require_app_id(&id)?;
    crate::audit::log(&app, Some(&s), "attempt").await;
    app.require(&s, MANAGE_OAUTH).await?;
    let mut a: OAuthApp = decode_json(&body, "oauth_app")?;
    if a.id != id {
        return Err(AppError::invalid_param("app_id"));
    }
    let old = managed(&app, &s, &id).await?;
    if !app.has_perm(&s, MANAGE_SYSTEM).await {
        a.is_trusted = old.is_trusted;
    }
    app.oauth_on("api.oauth.allow_oauth.turn_off.app_error")?;
    a.creator_id = old.creator_id.clone();
    a.create_at = old.create_at;
    a.client_secret = old.client_secret.clone();
    a.is_dynamically_registered = old.is_dynamically_registered;
    let a = app.store_oauth_app(a).await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(crate::oauth_provider::GoJson(a))
}

/// `getOAuthApps`: all of them with `manage_system_wide_oauth`, else one's own.
async fn list(
    St(app): St<State>,
    Auth(s): Auth,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<OAuthApp>>> {
    if !app.has_perm(&s, MANAGE_OAUTH).await {
        return Err(AppError::forbidden().with_id("api.command.admin_only.app_error"));
    }
    app.oauth_on("api.oauth.allow_oauth.turn_off.app_error")?;
    let (page, per_page) = paging(&q);
    let all = app.has_perm(&s, MANAGE_SYSTEM_WIDE_OAUTH).await;
    let sql = if all {
        format!("{SELECT} LIMIT $1 OFFSET $2")
    } else {
        format!("{SELECT} WHERE o.CreatorId = $3 LIMIT $1 OFFSET $2")
    };
    let mut query = sqlx::query_as::<_, OAuthApp>(sqlx::AssertSqlSafe(sql)).bind(per_page).bind(page * per_page);
    if !all {
        query = query.bind(&s.user_id);
    }
    let id = if all { "app.oauth.get_apps.find.app_error" } else { "app.oauth.get_app_by_user.find.app_error" };
    Ok(Json(query.fetch_all(&app.db).await.map_err(|e| AppError::internal(id, e.to_string()))?))
}

/// `getOAuthApp`.
async fn get_one(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
) -> ApiResult<crate::oauth_provider::GoJson<OAuthApp>> {
    require_app_id(&id)?;
    app.require(&s, MANAGE_OAUTH).await?;
    Ok(crate::oauth_provider::GoJson(managed(&app, &s, &id).await?))
}

/// `getOAuthAppInfo`: any user, without the secret.
async fn info(
    St(app): St<State>,
    _: Auth,
    Path(id): Path<String>,
) -> ApiResult<crate::oauth_provider::GoJson<OAuthApp>> {
    require_app_id(&id)?;
    Ok(crate::oauth_provider::GoJson(app.oauth_app(&id).await?.sanitize()))
}

/// `deleteOAuthApp`: the app, the sessions and access data it granted, and its authorizations. Go then
/// invalidates every cache; here the sessions it ended leave the session cache.
async fn delete(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    require_app_id(&id)?;
    crate::audit::log(&app, Some(&s), "attempt").await;
    app.require(&s, MANAGE_OAUTH).await?;
    let a = managed(&app, &s, &id).await?;
    app.oauth_on("api.oauth.allow_oauth.turn_off.app_error")?;
    let fail = |e: sqlx::Error| AppError::internal("app.oauth.delete_app.app_error", e.to_string());
    let tokens: Vec<(String,)> = sqlx::query_as("SELECT Token FROM OAuthAccessData WHERE ClientId = $1")
        .bind(&a.id)
        .fetch_all(&app.db)
        .await
        .map_err(fail)?;
    let mut tx = app.db.begin().await.map_err(fail)?;
    sqlx::query("DELETE FROM OAuthApps WHERE Id = $1").bind(&a.id).execute(&mut *tx).await.map_err(fail)?;
    sqlx::query("DELETE FROM Sessions s USING OAuthAccessData o WHERE o.Token = s.Token AND o.ClientId = $1")
        .bind(&a.id)
        .execute(&mut *tx)
        .await
        .map_err(fail)?;
    sqlx::query("DELETE FROM OAuthAccessData WHERE ClientId = $1").bind(&a.id).execute(&mut *tx).await.map_err(fail)?;
    sqlx::query("DELETE FROM Preferences WHERE Category = 'oauth_app' AND Name = $1")
        .bind(&a.id)
        .execute(&mut *tx)
        .await
        .map_err(fail)?;
    tx.commit().await.map_err(fail)?;
    for (t,) in tokens {
        app.caches.session.remove(&t);
        app.hub.close_session(&t);
    }
    // DeleteOAuthApp: InvalidateAllCaches (with its license_changed to everyone).
    crate::license::invalidate_all_caches(&app).await;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(ok())
}

/// `regenerateOAuthAppSecret`.
async fn regen_secret(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
) -> ApiResult<crate::oauth_provider::GoJson<OAuthApp>> {
    require_app_id(&id)?;
    app.require(&s, MANAGE_OAUTH).await?;
    let mut a = managed(&app, &s, &id).await?;
    if a.client_secret.is_empty() {
        return Err(AppError::bad_request("api.oauth.regenerate_secret.public_client.app_error", ""));
    }
    app.oauth_on("api.oauth.allow_oauth.turn_off.app_error")?;
    a.client_secret = new_id();
    let a = app.store_oauth_app(a).await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(crate::oauth_provider::GoJson(a))
}

/// `getAuthorizedOAuthApps`: the apps of the user's `oauth_app` preferences, without secrets.
async fn authorized(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<OAuthApp>>> {
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    app.oauth_on("api.oauth.allow_oauth.turn_off.app_error")?;
    let (page, per_page) = paging(&q);
    let apps: Vec<OAuthApp> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} INNER JOIN Preferences p ON p.Name = o.Id AND p.UserId = $1 LIMIT $2 OFFSET $3"
    )))
    .bind(&user_id)
    .bind(per_page)
    .bind(page * per_page)
    .fetch_all(&app.db)
    .await
    .map_err(|e| AppError::internal("app.oauth.get_apps.find.app_error", e.to_string()))?;
    Ok(Json(apps.into_iter().map(OAuthApp::sanitize).collect()))
}

/// `registerOAuthClient` (RFC 7591): only the errors of a server with dynamic client registration off.
async fn register(St(app): St<State>, body: Bytes) -> Response {
    let dcr = |error: &str, description: &str| {
        (StatusCode::BAD_REQUEST, Json(json!({"error": error, "error_description": description}))).into_response()
    };
    if serde_json::Deserializer::from_slice(&body)
        .into_iter::<serde_json::Map<String, Value>>()
        .next()
        .is_none_or(|r| r.is_err())
    {
        return dcr("invalid_client_metadata", "Invalid JSON in request body");
    }
    if !app.cfg.bool("ServiceSettings.EnableOAuthServiceProvider") {
        return dcr("unsupported_operation", "OAuth service provider is disabled");
    }
    if !app.cfg.bool("ServiceSettings.EnableDynamicClientRegistration") {
        return dcr("unsupported_operation", "Dynamic client registration is disabled");
    }
    AppError::not_implemented("Dynamic client registration").into_response()
}
