//! This server as an OAuth 2.0 provider for the apps registered with it (`src/oauth.rs`): the authorize page and
//! its API (`/oauth/authorize`), the token endpoint (`/oauth/access_token`, authorization code with PKCE and
//! refresh token grants), `/oauth/deauthorize` and the authorization server metadata (RFC 8414). Codes are
//! `OAuthAuthData` rows, tokens `OAuthAccessData` rows with an OAuth session each. (Go also registers
//! `/api/v4/oauth_test`, but behind the API router it is never reached: a 404.)

// Based on Mattermost server/channels/web/oauth.go, app/oauth.go, app/session.go, model/authorize.go,
// model/access.go, model/oauth.go, model/oauth_metadata.go and store/sqlstore/oauth_store.go,
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State as St};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

use crate::app::{
    ApiResult, App, AppError, Auth, MaybeAuth, State, decode_json, is_valid_id, map_from_json, new_id, now_ms,
};
use crate::db::Table;
use crate::gourl::{self, Url};
use crate::model::{Session, User};
use crate::oauth::{OAuthApp, valid_http_url};
use crate::users::ok;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/oauth/authorize", get(authorize_page).post(authorize_app))
        .route("/oauth/deauthorize", post(deauthorize))
        .route("/oauth/access_token", post(access_token))
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/.well-known/oauth-authorization-server/{*rest}", get(metadata))
}

const AUTH_CODE_EXPIRE_SECS: i32 = 600;
const DEFAULT_SCOPE: &str = "user";
const CODE: &str = "code";
const IMPLICIT: &str = "token";
pub const GRANT_CODE: &str = "authorization_code";
const GRANT_REFRESH: &str = "refresh_token";
const MAX_SESSIONS: i64 = 500;

/// `model.AuthorizeRequest`.
#[derive(Deserialize, Default, Clone)]
#[serde(default)]
pub struct AuthorizeRequest {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scope: String,
    pub state: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub resource: String,
}

fn bad(id: &str) -> AppError {
    AppError::bad_request(id, "")
}

/// `validatePKCEParameters`.
fn validate_pkce(challenge: &str, method: &str) -> ApiResult<()> {
    if challenge.is_empty() {
        return Err(bad("model.authorize.is_valid.code_challenge.app_error"));
    }
    if method.is_empty() {
        return Err(bad("model.authorize.is_valid.code_challenge_method.app_error"));
    }
    if method != "S256" {
        return Err(bad("model.authorize.is_valid.code_challenge_method.unsupported.app_error"));
    }
    if !(43..=128).contains(&challenge.len()) {
        return Err(bad("model.authorize.is_valid.code_challenge.length.app_error"));
    }
    if !challenge.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err(bad("model.authorize.is_valid.code_challenge.format.app_error"));
    }
    Ok(())
}

/// `model.ValidateResourceParameter` (RFC 8707).
pub fn validate_resource(resource: &str) -> ApiResult<()> {
    if resource.is_empty() {
        return Ok(());
    }
    if resource.len() > 512 {
        return Err(bad("model.authorize.is_valid.resource.length.app_error"));
    }
    let u = Url::parse(resource).map_err(|_| bad("model.authorize.is_valid.resource.invalid_uri.app_error"))?;
    if !u.is_abs() {
        return Err(bad("model.authorize.is_valid.resource.not_absolute.app_error"));
    }
    if u.fragment.as_deref().is_some_and(|f| !f.is_empty()) {
        return Err(bad("model.authorize.is_valid.resource.has_fragment.app_error"));
    }
    Ok(())
}

impl AuthorizeRequest {
    /// `AuthorizeRequest.IsValid`.
    pub fn check(&self) -> ApiResult<()> {
        if !is_valid_id(&self.client_id) {
            return Err(bad("model.authorize.is_valid.client_id.app_error"));
        }
        if self.response_type.is_empty() {
            return Err(bad("model.authorize.is_valid.response_type.app_error"));
        }
        if self.redirect_uri.is_empty() || self.redirect_uri.len() > 256 || !valid_http_url(&self.redirect_uri) {
            return Err(bad("model.authorize.is_valid.redirect_uri.app_error"));
        }
        if self.state.len() > 1024 {
            return Err(bad("model.authorize.is_valid.state.app_error"));
        }
        if self.scope.len() > 128 {
            return Err(bad("model.authorize.is_valid.scope.app_error"));
        }
        if !self.code_challenge.is_empty() || !self.code_challenge_method.is_empty() {
            validate_pkce(&self.code_challenge, &self.code_challenge_method)?;
        }
        validate_resource(&self.resource)
    }
}

/// `model.AuthData`: an `OAuthAuthData` row.
#[derive(sqlx::FromRow, Default, Clone)]
struct AuthData {
    client_id: String,
    user_id: String,
    code: String,
    expires_in: i32,
    create_at: i64,
    redirect_uri: String,
    state: String,
    scope: String,
    code_challenge: String,
    code_challenge_method: String,
    resource: String,
}

const AUTH_DATA_SELECT: &str = "SELECT COALESCE(ClientId, '') AS client_id, COALESCE(UserId, '') AS user_id,
    Code AS code, COALESCE(ExpiresIn, 0) AS expires_in, COALESCE(CreateAt, 0) AS create_at,
    COALESCE(RedirectUri, '') AS redirect_uri, COALESCE(State, '') AS state, COALESCE(Scope, '') AS scope,
    COALESCE(CodeChallenge, '') AS code_challenge, COALESCE(CodeChallengeMethod, '') AS code_challenge_method,
    COALESCE(Resource, '') AS resource FROM OAuthAuthData";

impl AuthData {
    /// `AuthData.IsValid` after `PreSave`.
    fn check(&self) -> ApiResult<()> {
        if !is_valid_id(&self.client_id) {
            return Err(bad("model.authorize.is_valid.client_id.app_error"));
        }
        if !is_valid_id(&self.user_id) {
            return Err(bad("model.authorize.is_valid.user_id.app_error"));
        }
        if self.code.is_empty() || self.code.len() > 128 {
            return Err(bad("model.authorize.is_valid.auth_code.app_error"));
        }
        if self.expires_in == 0 {
            return Err(bad("model.authorize.is_valid.expires.app_error"));
        }
        if self.create_at <= 0 {
            return Err(bad("model.authorize.is_valid.create_at.app_error"));
        }
        if self.redirect_uri.len() > 256 || !valid_http_url(&self.redirect_uri) {
            return Err(bad("model.authorize.is_valid.redirect_uri.app_error"));
        }
        if self.state.len() > 1024 {
            return Err(bad("model.authorize.is_valid.state.app_error"));
        }
        if self.scope.len() > 128 {
            return Err(bad("model.authorize.is_valid.scope.app_error"));
        }
        if !self.code_challenge.is_empty() || !self.code_challenge_method.is_empty() {
            validate_pkce(&self.code_challenge, &self.code_challenge_method)?;
        }
        validate_resource(&self.resource)
    }

    fn is_expired(&self) -> bool {
        now_ms() > self.create_at + self.expires_in as i64 * 1000
    }

    /// `VerifyPKCE`.
    fn verify_pkce(&self, verifier: &str) -> bool {
        use base64::Engine;
        use sha2::Digest;
        if self.code_challenge.is_empty() && self.code_challenge_method.is_empty() {
            return true;
        }
        if self.code_challenge.is_empty() || self.code_challenge_method.is_empty() {
            return false;
        }
        if !(43..=128).contains(&verifier.len())
            || !verifier.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
        {
            return false;
        }
        if self.code_challenge_method != "S256" {
            return false;
        }
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()))
            == self.code_challenge
    }

    /// `ValidatePKCEForClientType`.
    fn validate_pkce_for_client(&self, public: bool, verifier: &str) -> ApiResult<()> {
        let failed = || bad("model.authorize.validate_pkce.verification_failed.app_error");
        let required = || bad("model.authorize.validate_pkce.verifier_required.app_error");
        if public {
            if self.code_challenge.is_empty() {
                return Err(bad("model.authorize.validate_pkce.public_client_required.app_error"));
            }
            if verifier.is_empty() {
                return Err(required());
            }
            if !self.verify_pkce(verifier) {
                return Err(failed());
            }
        } else if !self.code_challenge.is_empty() {
            if verifier.is_empty() {
                return Err(required());
            }
            if !self.verify_pkce(verifier) {
                return Err(failed());
            }
        } else if !verifier.is_empty() {
            return Err(bad("model.authorize.validate_pkce.not_used_in_auth.app_error"));
        }
        Ok(())
    }
}

/// `model.AccessData`: an `OAuthAccessData` row.
#[derive(sqlx::FromRow, Default, Clone)]
struct AccessData {
    client_id: String,
    user_id: String,
    token: String,
    refresh_token: String,
    redirect_uri: String,
    expires_at: i64,
    scope: String,
    audience: String,
}

const ACCESS_SELECT: &str = "SELECT COALESCE(ClientId, '') AS client_id, COALESCE(UserId, '') AS user_id,
    Token AS token, COALESCE(RefreshToken, '') AS refresh_token, COALESCE(RedirectUri, '') AS redirect_uri,
    COALESCE(ExpiresAt, 0) AS expires_at, COALESCE(Scope, '') AS scope, COALESCE(Audience, '') AS audience
    FROM OAuthAccessData";

impl AccessData {
    /// `AccessData.IsValid`.
    fn check(&self) -> ApiResult<()> {
        if self.client_id.is_empty() || self.client_id.len() > 26 {
            return Err(bad("model.access.is_valid.client_id.app_error"));
        }
        if self.user_id.is_empty() || self.user_id.len() > 26 {
            return Err(bad("model.access.is_valid.user_id.app_error"));
        }
        if self.token.len() != 26 {
            return Err(bad("model.access.is_valid.access_token.app_error"));
        }
        if self.refresh_token.len() > 26 {
            return Err(bad("model.access.is_valid.refresh_token.app_error"));
        }
        if self.redirect_uri.is_empty() || self.redirect_uri.len() > 256 || !valid_http_url(&self.redirect_uri) {
            return Err(bad("model.access.is_valid.redirect_uri.app_error"));
        }
        validate_resource(&self.audience)
    }
    fn is_expired(&self) -> bool {
        self.expires_at > 0 && now_ms() > self.expires_at
    }
}

/// `model.AccessResponse`.
#[derive(Serialize)]
struct AccessResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: i32,
    scope: String,
    refresh_token: String,
    id_token: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    audience: String,
}

impl OAuthApp {
    /// `IsPublicClient`: an app without a secret.
    pub fn is_public(&self) -> bool {
        self.client_secret.is_empty()
    }

    /// `ValidateForGrantType`.
    fn validate_grant(&self, grant: &str, secret: &str, verifier: &str) -> ApiResult<()> {
        if self.is_public() {
            if !secret.is_empty() {
                return Err(bad("model.oauth.validate_grant.public_client_secret.app_error"));
            }
            if grant == GRANT_REFRESH {
                return Err(bad("model.oauth.validate_grant.public_client_refresh_token.app_error"));
            }
            if grant == GRANT_CODE && verifier.is_empty() {
                return Err(bad("model.oauth.validate_grant.pkce_required.app_error"));
            }
            return Ok(());
        }
        if !subtle_eq(self.client_secret.as_bytes(), secret.as_bytes()) {
            return Err(AppError::new(
                StatusCode::UNAUTHORIZED,
                "model.oauth.validate_grant.credentials.app_error",
                "",
            ));
        }
        Ok(())
    }
}

/// `subtle.ConstantTimeCompare`.
fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn turned_off(id: &str) -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, id, "")
}

impl App {
    fn oauth_enabled(&self) -> bool {
        self.cfg.bool("ServiceSettings.EnableOAuthServiceProvider")
    }

    /// `OAuthStore.GetApp` without the config check.
    async fn stored_oauth_app(&self, id: &str) -> Result<Option<OAuthApp>, sqlx::Error> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!("{} WHERE o.Id = $1", crate::oauth::SELECT)))
            .bind(id)
            .fetch_optional(&self.db)
            .await
    }

    /// `limitNumberOfSessions`: the user keeps at most 499 sessions before a new one.
    pub async fn limit_sessions(&self, user_id: &str) -> ApiResult<()> {
        let old: Vec<Session> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{} WHERE UserId = $1 ORDER BY LastActivityAt DESC LIMIT 100 OFFSET $2",
            Session::SELECT
        )))
        .bind(user_id)
        .bind(MAX_SESSIONS - 1)
        .fetch_all(&self.db)
        .await
        .map_err(|e| AppError::internal("app.session.save.app_error", e.to_string()))?;
        for s in old {
            self.revoke_session(&s)
                .await
                .map_err(|e| AppError::internal("app.session.save.app_error", e.detailed_error))?;
        }
        Ok(())
    }

    /// `newSession`: an OAuth session for the app, `SessionLengthSSOInHours` long.
    async fn new_oauth_session(&self, app: &OAuthApp, user: &User) -> ApiResult<Session> {
        let internal = |d: String| AppError::internal("api.oauth.get_access_token.internal_session.app_error", d);
        self.limit_sessions(&user.id).await.map_err(|e| internal(e.detailed_error))?;
        let mut s = Session {
            user_id: user.id.clone(),
            roles: user.roles.clone(),
            is_oauth: true,
            token: new_id(),
            ..Default::default()
        };
        s.expires_at = now_ms() + self.cfg.i64("ServiceSettings.SessionLengthSSOInHours") * 3_600_000;
        let p = &mut s.props.0;
        p.insert(crate::model::session::PROP_CSRF.into(), new_id());
        p.insert("platform".into(), app.name.clone());
        p.insert("oauth_app_id".into(), app.id.clone());
        p.insert("mattermost_app_id".into(), app.mattermost_app_id.clone());
        p.insert("os".into(), "OAuth2".into());
        p.insert("browser".into(), "OAuth2".into());
        self.save_session(&mut s).await.map_err(|e| internal(e.detailed_error))?;
        Ok(s)
    }

    /// `AllowOAuthAppAccessToUser`: the redirect with a code (or a token), and the app saved as authorized.
    pub async fn allow_oauth_app(&self, user_id: &str, mut r: AuthorizeRequest) -> ApiResult<String> {
        if !self.oauth_enabled() {
            return Err(turned_off("api.oauth.allow_oauth.turn_off.app_error"));
        }
        if r.scope.is_empty() {
            r.scope = DEFAULT_SCOPE.into();
        }
        let app = self
            .stored_oauth_app(&r.client_id)
            .await
            .map_err(|e| AppError::internal("app.oauth.get_app.finding.app_error", e.to_string()))?
            .ok_or_else(|| AppError::not_found("app.oauth.get_app.find.app_error", ""))?;
        if !app.callback_urls.0.contains(&r.redirect_uri) {
            return Err(bad("api.oauth.allow_oauth.redirect_callback.app_error"));
        }
        if app.is_public() && r.response_type == CODE && r.code_challenge.is_empty() {
            return Err(bad("api.oauth.allow_oauth.pkce_required_public.app_error"));
        }
        let server_error = format!("{}?error=server_error&state={}", r.redirect_uri, r.state);
        let redirect = match r.response_type.as_str() {
            CODE => self.code_redirect(user_id, &r).await,
            IMPLICIT => self.implicit_redirect(&app, user_id, &r).await,
            _ => return Ok(format!("{}?error=unsupported_response_type&state={}", r.redirect_uri, r.state)),
        };
        let Ok(redirect) = redirect else { return Ok(server_error) };
        let saved = sqlx::query(
            "INSERT INTO Preferences (UserId, Category, Name, Value) VALUES ($1, 'oauth_app', $2, $3)
             ON CONFLICT (userid, category, name) DO UPDATE SET Value = $3",
        )
        .bind(user_id)
        .bind(&r.client_id)
        .bind(&r.scope)
        .execute(&self.db)
        .await;
        if saved.is_err() {
            return Ok(server_error);
        }
        Ok(redirect)
    }

    /// `GetOAuthCodeRedirect`: a 10-minute code in `OAuthAuthData`, added to the callback's query.
    async fn code_redirect(&self, user_id: &str, r: &AuthorizeRequest) -> ApiResult<String> {
        let ad = AuthData {
            client_id: r.client_id.clone(),
            user_id: user_id.into(),
            code: format!("{}{}", new_id(), new_id()),
            expires_in: AUTH_CODE_EXPIRE_SECS,
            create_at: now_ms(),
            redirect_uri: r.redirect_uri.clone(),
            state: r.state.clone(),
            scope: if r.scope.is_empty() { DEFAULT_SCOPE.into() } else { r.scope.clone() },
            code_challenge: r.code_challenge.clone(),
            code_challenge_method: r.code_challenge_method.clone(),
            resource: r.resource.clone(),
        };
        let Ok(mut uri) = Url::parse(&r.redirect_uri) else {
            return Ok(format!("{}?error=redirect_uri_parse_error&state={}", r.redirect_uri, r.state));
        };
        let mut q = uri.query();
        let saved = match ad.check() {
            Err(_) => false,
            Ok(()) => sqlx::query(
                "INSERT INTO OAuthAuthData (ClientId, UserId, Code, ExpiresIn, CreateAt, RedirectUri, State, Scope,
                 CodeChallenge, CodeChallengeMethod, Resource) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            )
            .bind(&ad.client_id)
            .bind(&ad.user_id)
            .bind(&ad.code)
            .bind(ad.expires_in)
            .bind(ad.create_at)
            .bind(&ad.redirect_uri)
            .bind(&ad.state)
            .bind(&ad.scope)
            .bind(&ad.code_challenge)
            .bind(&ad.code_challenge_method)
            .bind(&ad.resource)
            .execute(&self.db)
            .await
            .is_ok(),
        };
        if !saved {
            q.set("error", "server_error");
            q.set("state", &r.state);
        } else {
            q.set("code", &ad.code);
            q.set("state", &ad.state);
        }
        uri.raw_query = q.encode();
        Ok(uri.to_go_string())
    }

    /// `GetOAuthImplicitRedirect`: a session for the app, its token in the callback's fragment.
    async fn implicit_redirect(&self, app: &OAuthApp, user_id: &str, r: &AuthorizeRequest) -> ApiResult<String> {
        let user = self.user(user_id).await?.ok_or_else(crate::users::missing_account)?;
        if user.delete_at != 0 {
            return Err(AppError::new(StatusCode::FORBIDDEN, "api.oauth.get_access_token.expired_code.app_error", ""));
        }
        let s = self.new_oauth_session(app, &user).await?;
        let ad = AccessData {
            client_id: r.client_id.clone(),
            user_id: user.id.clone(),
            token: s.token.clone(),
            redirect_uri: r.redirect_uri.clone(),
            expires_at: s.expires_at,
            scope: r.scope.clone(),
            ..Default::default()
        };
        self.save_access_data(&ad).await?;
        let mut v = gourl::Values::default();
        v.add("access_token", &s.token);
        v.add("token_type", "bearer");
        v.add("expires_in", &((s.expires_at - now_ms()) / 1000).to_string());
        v.add("scope", &r.scope);
        v.add("state", &r.state);
        Ok(format!("{}#{}", r.redirect_uri, v.encode()))
    }

    async fn save_access_data(&self, ad: &AccessData) -> ApiResult<()> {
        let internal = |d: String| AppError::internal("api.oauth.get_access_token.internal_saving.app_error", d);
        ad.check().map_err(|e| internal(e.id))?;
        sqlx::query(
            "INSERT INTO OAuthAccessData (ClientId, UserId, Token, RefreshToken, RedirectUri, ExpiresAt, Scope, Audience)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&ad.client_id)
        .bind(&ad.user_id)
        .bind(&ad.token)
        .bind(&ad.refresh_token)
        .bind(&ad.redirect_uri)
        .bind(ad.expires_at)
        .bind(&ad.scope)
        .bind(&ad.audience)
        .execute(&self.db)
        .await
        .map_err(|e| internal(e.to_string()))?;
        Ok(())
    }

    /// `GetOAuthAccessTokenForCodeFlow`.
    #[allow(clippy::too_many_arguments)]
    async fn access_token_for_code_flow(
        &self,
        client_id: &str,
        grant: &str,
        redirect_uri: &str,
        code: &str,
        secret: &str,
        refresh_token: &str,
        verifier: &str,
        resource: &str,
    ) -> ApiResult<AccessResponse> {
        if !self.oauth_enabled() {
            return Err(turned_off("api.oauth.get_access_token.disabled.app_error"));
        }
        let app = self
            .stored_oauth_app(client_id)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| AppError::not_found("api.oauth.get_access_token.credentials.app_error", ""))?;
        app.validate_grant(grant, secret, verifier)?;
        if grant == GRANT_CODE {
            self.code_grant(&app, redirect_uri, code, verifier, client_id, resource).await
        } else {
            self.refresh_grant(&app, refresh_token, resource).await
        }
    }

    async fn oauth_user(&self, id: &str) -> ApiResult<User> {
        let u = self
            .user(id)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| AppError::not_found("api.oauth.get_access_token.internal_user.app_error", ""))?;
        if u.delete_at != 0 {
            return Err(AppError::new(StatusCode::FORBIDDEN, "api.oauth.get_access_token.expired_code.app_error", ""));
        }
        Ok((*u).clone())
    }

    async fn remove_auth_data(&self, code: &str) {
        let _ = sqlx::query("DELETE FROM OAuthAuthData WHERE Code = $1").bind(code).execute(&self.db).await;
    }

    /// `handleAuthorizationCodeGrant`.
    async fn code_grant(
        &self,
        app: &OAuthApp,
        redirect_uri: &str,
        code: &str,
        verifier: &str,
        client_id: &str,
        resource: &str,
    ) -> ApiResult<AccessResponse> {
        let expired = |s: StatusCode| AppError::new(s, "api.oauth.get_access_token.expired_code.app_error", "");
        let ad: AuthData = sqlx::query_as(sqlx::AssertSqlSafe(format!("{AUTH_DATA_SELECT} WHERE Code = $1")))
            .bind(code)
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten()
            .filter(|a: &AuthData| !a.code.is_empty())
            .ok_or_else(|| expired(StatusCode::BAD_REQUEST))?;
        if ad.is_expired() {
            self.remove_auth_data(&ad.code).await;
            return Err(expired(StatusCode::FORBIDDEN));
        }
        if ad.client_id != client_id {
            return Err(bad("api.oauth.get_access_token.client_id_mismatch.app_error"));
        }
        if ad.redirect_uri != redirect_uri {
            return Err(bad("api.oauth.get_access_token.redirect_uri.app_error"));
        }
        ad.validate_pkce_for_client(app.is_public(), verifier)?;
        let user = self.oauth_user(&ad.user_id).await?;
        // From here on the code is spent, whatever the outcome (Go's deferred RemoveAuthData).
        let out = async {
            let mut audience = String::new();
            if !resource.is_empty() {
                validate_resource(resource)?;
                if !ad.resource.is_empty() && resource != ad.resource {
                    return Err(bad("api.oauth.get_access_token.resource_mismatch.app_error"));
                }
                audience = resource.to_string();
            } else if !ad.resource.is_empty() {
                audience = ad.resource.clone();
            }
            self.access_token_response(app, &user, client_id, redirect_uri, &ad.scope, &audience).await
        }
        .await;
        self.remove_auth_data(&ad.code).await;
        out
    }

    /// `generateAccessTokenResponse`: the user's latest token for the app if still good, else a new one.
    async fn access_token_response(
        &self,
        app: &OAuthApp,
        user: &User,
        client_id: &str,
        redirect_uri: &str,
        scope: &str,
        audience: &str,
    ) -> ApiResult<AccessResponse> {
        let prev: Option<AccessData> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{ACCESS_SELECT} WHERE UserId = $1 AND ClientId = $2 ORDER BY ExpiresAt DESC LIMIT 1"
        )))
        .bind(&user.id)
        .bind(client_id)
        .fetch_optional(&self.db)
        .await
        .map_err(|e| {
            AppError::bad_request("api.oauth.get_access_token.internal.app_error", "").detail(e.to_string())
        })?;
        if let Some(prev) = prev {
            if prev.is_expired() {
                return self.new_session_update_token(app, prev, user, audience).await;
            }
            return Ok(AccessResponse {
                access_token: prev.token.clone(),
                token_type: "bearer",
                refresh_token: if app.is_public() { String::new() } else { prev.refresh_token.clone() },
                expires_in: ((prev.expires_at - now_ms()) / 1000) as i32,
                scope: String::new(),
                id_token: String::new(),
                audience: prev.audience,
            });
        }
        let s = self.new_oauth_session(app, user).await?;
        let ad = AccessData {
            client_id: client_id.into(),
            user_id: user.id.clone(),
            token: s.token.clone(),
            refresh_token: if app.is_public() { String::new() } else { new_id() },
            redirect_uri: redirect_uri.into(),
            expires_at: s.expires_at,
            scope: scope.into(),
            audience: audience.into(),
        };
        self.save_access_data(&ad).await?;
        Ok(AccessResponse {
            access_token: s.token,
            token_type: "bearer",
            refresh_token: if app.is_public() { String::new() } else { ad.refresh_token },
            expires_in: (self.cfg.i64("ServiceSettings.SessionLengthSSOInHours") * 3600) as i32,
            scope: String::new(),
            id_token: String::new(),
            audience: audience.into(),
        })
    }

    /// `handleRefreshTokenGrant`.
    async fn refresh_grant(&self, app: &OAuthApp, refresh_token: &str, resource: &str) -> ApiResult<AccessResponse> {
        app.validate_grant(GRANT_REFRESH, &app.client_secret, "")?;
        let ad: AccessData =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("{ACCESS_SELECT} WHERE RefreshToken = $1 LIMIT 1")))
                .bind(refresh_token)
                .fetch_optional(&self.db)
                .await
                .ok()
                .flatten()
                .ok_or_else(|| AppError::not_found("api.oauth.get_access_token.refresh_token.app_error", ""))?;
        if ad.client_id != app.id {
            return Err(bad("api.oauth.get_access_token.client_id_mismatch.app_error"));
        }
        let user = self.oauth_user(&ad.user_id).await?;
        let mut audience = ad.audience.clone();
        if !resource.is_empty() {
            validate_resource(resource)?;
            if !ad.audience.is_empty() && resource != ad.audience {
                return Err(bad("api.oauth.get_access_token.resource_mismatch.app_error"));
            }
            audience = resource.to_string();
        }
        self.new_session_update_token(app, ad, &user, &audience).await
    }

    /// `newSessionUpdateToken`: the old session gone, a new one, and the access data moved to it.
    async fn new_session_update_token(
        &self,
        app: &OAuthApp,
        mut ad: AccessData,
        user: &User,
        audience: &str,
    ) -> ApiResult<AccessResponse> {
        let _ = sqlx::query("DELETE FROM Sessions WHERE Id = $1 OR Token = $1").bind(&ad.token).execute(&self.db).await;
        self.caches.session.remove(&ad.token);
        // The old token's websockets stop getting events (the user's others load their session again).
        self.hub.invalidate_session(&ad.token);
        self.clear_sessions_for_user(&user.id);
        let s = self.new_oauth_session(app, user).await?;
        ad.token = s.token.clone();
        ad.refresh_token = if app.is_public() { String::new() } else { new_id() };
        ad.expires_at = s.expires_at;
        if !audience.is_empty() {
            ad.audience = audience.into();
        }
        let failed = |d: String| AppError::internal("web.get_access_token.internal_saving.app_error", d);
        ad.check().map_err(|e| failed(e.id))?;
        sqlx::query(
            "UPDATE OAuthAccessData SET Token = $1, ExpiresAt = $2, RefreshToken = $3, Audience = $4
             WHERE ClientId = $5 AND UserId = $6",
        )
        .bind(&ad.token)
        .bind(ad.expires_at)
        .bind(&ad.refresh_token)
        .bind(&ad.audience)
        .bind(&ad.client_id)
        .bind(&ad.user_id)
        .execute(&self.db)
        .await
        .map_err(|e| failed(e.to_string()))?;
        Ok(AccessResponse {
            access_token: s.token,
            refresh_token: ad.refresh_token,
            token_type: "bearer",
            expires_in: (self.cfg.i64("ServiceSettings.SessionLengthSSOInHours") * 3600) as i32,
            scope: String::new(),
            id_token: String::new(),
            audience: ad.audience,
        })
    }

    /// `DeauthorizeOAuthAppForUser`: the app's tokens and sessions for the user, its codes, and the authorization.
    async fn deauthorize_oauth_app(&self, user_id: &str, app_id: &str) -> ApiResult<()> {
        if !self.oauth_enabled() {
            return Err(turned_off("api.oauth.allow_oauth.turn_off.app_error"));
        }
        let tokens: Vec<(String,)> =
            sqlx::query_as("SELECT Token FROM OAuthAccessData WHERE UserId = $1 AND ClientId = $2")
                .bind(user_id)
                .bind(app_id)
                .fetch_all(&self.db)
                .await
                .map_err(|e| {
                    AppError::internal("app.oauth.get_access_data_by_user_for_app.app_error", e.to_string())
                })?;
        for (token,) in tokens {
            self.revoke_oauth_access_token(&token).await?;
        }
        sqlx::query("DELETE FROM OAuthAuthData WHERE ClientId = $1 AND UserId = $2")
            .bind(app_id)
            .bind(user_id)
            .execute(&self.db)
            .await
            .map_err(|e| AppError::internal("app.oauth.remove_auth_data_by_client_id.app_error", e.to_string()))?;
        sqlx::query("DELETE FROM Preferences WHERE UserId = $1 AND Category = 'oauth_app' AND Name = $2")
            .bind(user_id)
            .bind(app_id)
            .execute(&self.db)
            .await
            .map_err(|e| AppError::internal("app.preference.delete.app_error", e.to_string()))?;
        Ok(())
    }

    /// `RevokeAccessToken`: the session and the access data of an OAuth token.
    async fn revoke_oauth_access_token(&self, token: &str) -> ApiResult<()> {
        let session: Option<Session> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("{} WHERE Token = $1 LIMIT 1", Session::SELECT)))
                .bind(token)
                .fetch_optional(&self.db)
                .await
                .ok()
                .flatten();
        let removed =
            sqlx::query("DELETE FROM Sessions WHERE Id = $1 OR Token = $1").bind(token).execute(&self.db).await;
        let found: Option<(String,)> = sqlx::query_as("SELECT Token FROM OAuthAccessData WHERE Token = $1")
            .bind(token)
            .fetch_optional(&self.db)
            .await
            .map_err(|e| {
                AppError::bad_request("api.oauth.revoke_access_token.get.app_error", "").detail(e.to_string())
            })?;
        if found.is_none() {
            return Err(AppError::bad_request("api.oauth.revoke_access_token.get.app_error", ""));
        }
        sqlx::query("DELETE FROM OAuthAccessData WHERE Token = $1")
            .bind(token)
            .execute(&self.db)
            .await
            .map_err(|e| AppError::internal("api.oauth.revoke_access_token.del_token.app_error", e.to_string()))?;
        removed
            .map_err(|e| AppError::internal("api.oauth.revoke_access_token.del_session.app_error", e.to_string()))?;
        self.caches.session.remove(token);
        // Its websockets stop getting events, as Go's `InvalidateCache` then `IsBasicAuthenticated` do.
        self.hub.invalidate_session(token);
        if let Some(s) = session {
            self.clear_sessions_for_user(&s.user_id);
        }
        Ok(())
    }
}

// ---------- the request's site URL ----------

/// `GetProtocol`: https behind a proxy that says so.
pub fn protocol(headers: &HeaderMap) -> &'static str {
    if headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https") { "https" } else { "http" }
}

/// `Context.GetSiteURLHeader`: the request's scheme and host with the configured subpath, no trailing slash.
pub fn site_url_header(app: &App, headers: &HeaderMap) -> String {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let sub = crate::web::subpath(app.cfg.str("ServiceSettings.SiteURL"));
    format!("{}://{host}{sub}", protocol(headers)).trim_end_matches('/').to_string()
}

/// `http.Redirect` from a handler that already set its content type: the location and no body.
pub fn redirect(status: StatusCode, location: &str, content_type: &'static str) -> Response {
    let mut r = status.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(v) = HeaderValue::from_str(&hex_escape_non_ascii(location)) {
        h.insert(header::LOCATION, v);
    }
    r
}

/// `hexEscapeNonASCII`.
fn hex_escape_non_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b >= 0x80 { out.push_str(&format!("%{b:02x}")) } else { out.push(b as char) }
    }
    out
}

/// Go's `json.Marshal`: `<`, `>`, `&` and the line separators escaped.
pub fn go_json<T: Serialize>(v: &T) -> Vec<u8> {
    let s = serde_json::to_string(v).unwrap_or_default();
    s.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
        .into_bytes()
}

/// `json.NewEncoder(w).Encode(v)`: Go's JSON and a newline.
pub fn go_encode<T: Serialize>(status: StatusCode, v: &T) -> Response {
    let mut body = go_json(v);
    body.push(b'\n');
    let mut r = (status, body).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

/// A 200 answer written with `json.NewEncoder(w).Encode`.
pub struct GoJson<T>(pub T);

impl<T: Serialize> IntoResponse for GoJson<T> {
    fn into_response(self) -> Response {
        go_encode(StatusCode::OK, &self.0)
    }
}

/// `Expires: 0` as Go's API handlers send on every GET.
fn expires0(mut r: Response) -> Response {
    r.headers_mut().insert(header::EXPIRES, HeaderValue::from_static("0"));
    r
}

// ---------- handlers ----------

/// `authorizeOAuthPage`: to the login page without a session, to the callback for an app already authorized (or
/// trusted), otherwise the client's page that asks the user. Its errors are web error pages.
async fn authorize_page(St(app): St<State>, MaybeAuth(s): MaybeAuth, req: Request) -> Response {
    let headers = req.headers().clone();
    expires0(authorize_page_inner(&app, s, &headers, req).await)
}

async fn authorize_page_inner(app: &State, s: Option<Arc<Session>>, headers: &HeaderMap, req: Request) -> Response {
    let web_app_error = |e: AppError| async move {
        let status = StatusCode::from_u16(e.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        crate::web::render_web_error(app, status, &[("message", &e.message)]).await
    };
    if !app.oauth_enabled() {
        return web_app_error(turned_off("api.oauth.authorize_oauth.disabled.app_error")).await;
    }
    let q = gourl::parse_query(req.uri().query().unwrap_or("")).0;
    let r = AuthorizeRequest {
        response_type: q.get("response_type").into(),
        client_id: q.get("client_id").into(),
        redirect_uri: q.get("redirect_uri").into(),
        scope: q.get("scope").into(),
        state: q.get("state").into(),
        code_challenge: q.get("code_challenge").into(),
        code_challenge_method: q.get("code_challenge_method").into(),
        resource: q.get("resource").into(),
    };
    if let Err(e) = r.check() {
        let status = StatusCode::from_u16(e.status_code).unwrap_or(StatusCode::BAD_REQUEST);
        return crate::web::render_web_error(app, status, &[("type", "oauth_invalid_param"), ("message", &e.message)])
            .await;
    }
    let oauth_app = match app.oauth_app(&r.client_id).await {
        Ok(a) => a,
        Err(e) => return web_app_error(e).await,
    };
    let sess = s.clone();
    let Some(s) = s.filter(|s| !s.user_id.is_empty()) else {
        crate::audit::log(app, sess.as_deref(), "success").await;
        let request_uri = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
        let login = if q.get("login_hint") == "saml" { "/login/sso/saml" } else { "/login" };
        let loc = format!("{}{login}?redirect_to={}", site_url_header(app, headers), gourl::query_escape(request_uri));
        return redirect(StatusCode::FOUND, &loc, "application/json");
    };
    if !oauth_app.callback_urls.0.contains(&r.redirect_uri) {
        let message = crate::i18n::t("api.oauth.allow_oauth.redirect_callback.app_error").unwrap_or("");
        return crate::web::render_web_error(
            app,
            StatusCode::BAD_REQUEST,
            &[("type", "oauth_invalid_redirect_url"), ("message", message)],
        )
        .await;
    }
    if oauth_app.is_public() && r.response_type == CODE && r.code_challenge.is_empty() {
        return crate::web::render_web_error(
            app,
            StatusCode::BAD_REQUEST,
            &[
                ("type", "oauth_pkce_required"),
                ("message", "PKCE is required for public clients using authorization code flow"),
            ],
        )
        .await;
    }
    let authorized = app.preference(&s.user_id, "oauth_app", &r.client_id).await.ok().flatten().is_some();
    if oauth_app.is_trusted || authorized {
        return match app.allow_oauth_app(&s.user_id, r).await {
            Ok(loc) => {
                crate::audit::log(app, Some(&s), "success").await;
                redirect(StatusCode::FOUND, &loc, "application/json")
            }
            Err(e) => web_app_error(e).await,
        };
    }
    crate::audit::log(app, Some(&s), "success").await;
    // The client's page asks the user (`http.ServeFile` of root.html).
    let file = crate::web::client_dir().join("root.html");
    let mut r = crate::web::serve_file(
        &file,
        headers,
        req.method() == axum::http::Method::HEAD,
        Some("text/html; charset=utf-8"),
    )
    .await;
    let h = r.headers_mut();
    h.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
    if let Ok(v) = HeaderValue::from_str(
        format!("frame-ancestors 'self' {}", app.cfg.str("ServiceSettings.FrameAncestors")).trim_end(),
    ) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, max-age=31556926"));
    r
}

/// `authorizeOAuthApp`: the web client's "Allow" on the authorize page.
async fn authorize_app(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Response> {
    let r: Option<AuthorizeRequest> = decode_json::<Option<AuthorizeRequest>>(&body, "authorize_request")?;
    let r = r.ok_or_else(|| AppError::invalid_param("authorize_request"))?;
    r.check()?;
    if s.is_oauth {
        let mut e = AppError::forbidden_perm("edit_other_users");
        e.detailed_error = format!("userId={}, permission=edit_other_users, attempted access by oauth app", s.user_id);
        return Err(e);
    }
    crate::audit::log(&app, Some(&s), "attempt").await;
    let redirect = app.allow_oauth_app(&s.user_id, r).await?;
    crate::audit::log(&app, Some(&s), "success").await;
    let mut m = std::collections::BTreeMap::new();
    m.insert("redirect".to_string(), redirect);
    let mut r = crate::sso::go_map_json(&m).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(r)
}

/// `deauthorizeOAuthApp`.
async fn deauthorize(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Response> {
    let client_id = map_from_json(&body).remove("client_id").unwrap_or_default();
    if !is_valid_id(&client_id) {
        return Err(AppError::invalid_param("client_id"));
    }
    if s.is_oauth {
        let mut e = AppError::forbidden_perm("edit_other_users");
        e.detailed_error = format!("userId={}, permission=edit_other_users, attempted access by oauth app", s.user_id);
        return Err(e);
    }
    app.deauthorize_oauth_app(&s.user_id, &client_id).await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(ok().into_response())
}

/// `r.ParseForm` + `r.FormValue`: the urlencoded body first, then the query.
fn form_values(headers: &HeaderMap, query: &str, body: &[u8]) -> Result<gourl::Values, ()> {
    let (mut v, bad_q) = gourl::parse_query(query);
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let media = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let mut bad_b = false;
    if media == "application/x-www-form-urlencoded" || ct.is_empty() && !body.is_empty() {
        let (b, bad) = gourl::parse_query(&String::from_utf8_lossy(body));
        bad_b = bad;
        // Body values come first.
        let mut merged = b;
        for (k, vals) in v.0.drain(..) {
            for val in vals {
                merged.add(&k, &val);
            }
        }
        v = merged;
    }
    if bad_q || bad_b { Err(()) } else { Ok(v) }
}

/// `getAccessToken`: the token endpoint.
async fn access_token(St(app): St<State>, req: Request) -> ApiResult<Response> {
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, 64 << 20).await.unwrap_or_default();
    let form = form_values(&parts.headers, parts.uri.query().unwrap_or(""), &body)
        .map_err(|_| bad("api.oauth.get_access_token.bad_request.app_error"))?;
    let code = form.get("code");
    let refresh = form.get("refresh_token");
    let grant = form.get("grant_type");
    match grant {
        GRANT_CODE if code.is_empty() => return Err(bad("api.oauth.get_access_token.missing_code.app_error")),
        GRANT_REFRESH if refresh.is_empty() => {
            return Err(bad("api.oauth.get_access_token.missing_refresh_token.app_error"));
        }
        GRANT_CODE | GRANT_REFRESH => {}
        _ => return Err(bad("api.oauth.get_access_token.bad_grant.app_error")),
    }
    let client_id = form.get("client_id");
    if !is_valid_id(client_id) {
        return Err(bad("api.oauth.get_access_token.bad_client_id.app_error"));
    }
    crate::audit::log(&app, None, "attempt").await;
    let rsp = app
        .access_token_for_code_flow(
            client_id,
            grant,
            form.get("redirect_uri"),
            code,
            form.get("client_secret"),
            refresh,
            form.get("code_verifier"),
            form.get("resource"),
        )
        .await?;
    crate::audit::log(&app, None, "success").await;
    let mut body = go_json(&rsp);
    body.push(b'\n');
    let mut r = body.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    Ok(r)
}

/// `getAuthorizationServerMetadata` (RFC 8414). Errors are web error pages: the path is not an API one.
async fn metadata(St(app): St<State>, headers: HeaderMap) -> Response {
    match server_metadata(&app) {
        Ok(m) => {
            let mut body = go_json(&m);
            body.push(b'\n');
            let mut r = body.into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            expires0(r)
        }
        Err(e) => expires0(crate::web::web_error(&app, &headers, e).await),
    }
}

/// `model.AuthorizationServerMetadata`.
#[derive(Serialize)]
struct Metadata {
    issuer: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    authorization_endpoint: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    token_endpoint: String,
    response_types_supported: Vec<&'static str>,
    #[serde(skip_serializing_if = "String::is_empty")]
    registration_endpoint: String,
    scopes_supported: Vec<&'static str>,
    grant_types_supported: Vec<&'static str>,
    token_endpoint_auth_methods_supported: Vec<&'static str>,
    code_challenge_methods_supported: Vec<&'static str>,
}

fn server_metadata(app: &App) -> ApiResult<Metadata> {
    if !app.oauth_enabled() {
        return Err(turned_off("api.oauth.authorization_server_metadata.disabled.app_error"));
    }
    let site = app.cfg.str("ServiceSettings.SiteURL");
    if site.is_empty() {
        return Err(AppError::internal("api.oauth.authorization_server_metadata.site_url_required.app_error", ""));
    }
    let invalid = |e: String| AppError::internal("api.oauth.authorization_server_metadata.invalid_url.app_error", e);
    let mut m = Metadata {
        issuer: site.into(),
        authorization_endpoint: gourl::url_join_path(site, "/oauth/authorize").map_err(invalid)?,
        token_endpoint: gourl::url_join_path(site, "/oauth/access_token").map_err(invalid)?,
        response_types_supported: vec![CODE],
        registration_endpoint: String::new(),
        scopes_supported: vec![DEFAULT_SCOPE],
        grant_types_supported: vec![GRANT_CODE, GRANT_REFRESH],
        token_endpoint_auth_methods_supported: vec!["none", "client_secret_post"],
        code_challenge_methods_supported: vec!["S256"],
    };
    if app.cfg.bool("ServiceSettings.EnableDynamicClientRegistration") {
        m.registration_endpoint = gourl::url_join_path(site, "/api/v4/oauth/apps/register").map_err(invalid)?;
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce() {
        // RFC 7636 appendix B.
        let ad = AuthData {
            code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".into(),
            code_challenge_method: "S256".into(),
            ..Default::default()
        };
        assert!(ad.verify_pkce("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"));
        assert!(!ad.verify_pkce("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXx"));
        assert!(validate_resource("https://x.org/api").is_ok());
        assert!(validate_resource("/api").is_err());
        assert!(validate_resource("https://x.org/#a").is_err());
    }
}
