//! Signing in through an OAuth provider, as an unlicensed server does it: GitLab is the only provider (Google,
//! Office 365 and OpenID Connect are Enterprise providers, so they answer "not available"). The routes outside
//! `/api/`: `/oauth/{service}/login`, `mobile_login`, `signup`, the provider's callback (`/oauth|signup|login/
//! {service}/complete`, `/api/v3/oauth/{service}/complete`), `/oauth/intune` (Enterprise, never available), and the
//! guest magic link `/login/one_time_link`. Also the logins they lead to: `DoLogin` for an SSO user, and the
//! desktop app's token.

// Based on Mattermost server/channels/web/oauth.go, web/magic_link.go, app/oauth.go, app/login.go,
// app/desktop_login.go, app/user.go (CreateOAuthUser, LoginByOAuth, UpdateOAuthUserAttrs,
// AuthenticateUserForGuestMagicLink), app/team.go (AddUserToTeamByToken, AddUserToTeamByInviteId),
// app/oauthproviders/gitlab/gitlab.go and utils/api.go, utils/utils.go, Copyright (c) 2015-present Mattermost,
// Inc. Licensed under AGPL-3.0.

use std::collections::BTreeMap;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Request, State as St};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::app::{ApiResult, App, AppError, MaybeAuth, State, decode_json, new_id, now_ms};
use crate::gourl::{self, Url};
use crate::model::{Session, User, session};
use crate::oauth_provider::{protocol, redirect, site_url_header};
use crate::web::{render_web_error, web_error};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/oauth/{service}/login", get(login))
        .route("/oauth/{service}/mobile_login", get(mobile_login))
        .route("/oauth/{service}/signup", get(signup))
        .route("/oauth/{service}/complete", get(complete))
        .route("/api/v3/oauth/{service}/complete", get(complete))
        .route("/signup/{service}/complete", get(complete))
        .route("/login/{service}/complete", get(complete))
        .route("/oauth/intune", post(intune))
        .route("/login/one_time_link", get(one_time_link))
}

const COOKIE_OAUTH: &str = "MMOAUTH";
const COOKIE_MAX_AGE: i64 = 30 * 60;
const ACTION_SIGNUP: &str = "signup";
const ACTION_LOGIN: &str = "login";
const ACTION_EMAIL_TO_SSO: &str = "email_to_sso";
const ACTION_SSO_TO_EMAIL: &str = "sso_to_email";
const ACTION_MOBILE: &str = "mobile";

/// `model.NewRandomString`: base32 with Mattermost's alphabet, `length` characters.
pub fn new_random_string(length: usize) -> String {
    const ALPHABET: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";
    let mut out = String::with_capacity(length);
    while out.len() < length {
        out.push(ALPHABET[(rand::random::<u8>() & 31) as usize] as char);
    }
    out
}

/// `strings.Title` of a one-word service name.
fn title(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// `model.MapToJSON`: Go's `json.Marshal` of a string map (sorted keys, HTML characters escaped).
pub fn go_map_json(m: &BTreeMap<String, String>) -> String {
    let s = serde_json::to_string(m).unwrap_or_default();
    s.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// An error whose translation wants parameters Go did not pass: they print as `<no value>`.
fn no_value(mut e: AppError) -> AppError {
    if let Some(m) = crate::i18n::tr(&e.id, &[]) {
        e.message = regex::Regex::new(r"\{\{\s*\.[A-Za-z]+\s*\}\}").unwrap().replace_all(&m, "<no value>").into_owned();
    }
    e
}

/// `[A-Za-z0-9]+`, the pattern of Go's `{service}`: anything else is no route.
fn valid_service(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The settings of an SSO service (`Config.GetSSOService`), `None` for an unknown one.
struct Sso {
    enable: bool,
    id: String,
    secret: String,
    scope: String,
    auth_endpoint: String,
    token_endpoint: String,
    user_api_endpoint: String,
}

impl App {
    fn sso_settings(&self, service: &str) -> Option<Sso> {
        let section = match service {
            "gitlab" => "GitLabSettings",
            "google" => "GoogleSettings",
            "office365" => "Office365Settings",
            "openid" => "OpenIdSettings",
            _ => return None,
        };
        let s = |k: &str| self.cfg.str(&format!("{section}.{k}")).to_string();
        Some(Sso {
            enable: self.cfg.bool(&format!("{section}.Enable")),
            id: s("Id"),
            secret: s("Secret"),
            scope: s("Scope"),
            auth_endpoint: s("AuthEndpoint"),
            token_endpoint: s("TokenEndpoint"),
            user_api_endpoint: s("UserAPIEndpoint"),
        })
    }

    /// `getSSOProvider`: GitLab is the only provider an unlicensed server registers.
    fn sso_provider(&self, service: &str) -> ApiResult<Sso> {
        let sso = self.sso_settings(service).filter(|s| s.enable).ok_or_else(|| {
            no_value(AppError::new(
                StatusCode::NOT_IMPLEMENTED,
                "api.user.authorize_oauth_user.unsupported.app_error",
                "",
            ))
            .detail(format!("service={service}"))
        })?;
        let provider = if sso.scope.contains("openid") { "openid" } else { service };
        if provider != "gitlab" {
            return Err(AppError::new(
                StatusCode::NOT_IMPLEMENTED,
                "api.user.login_by_oauth.not_available.app_error",
                "",
            )
            .with_param("Service", &title(service)));
        }
        Ok(sso)
    }

    /// `GetSiteURL`, or the request's own scheme and host without one.
    fn site_url_or_request(&self, headers: &HeaderMap) -> String {
        let site = self.cfg.str("ServiceSettings.SiteURL");
        if !site.trim().is_empty() {
            return site.to_string();
        }
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        format!("{}://{host}", protocol(headers))
    }

    /// `GetAuthorizationCode`: the provider's authorize URL, with the state (the props and a one-time `oauth`
    /// token) and the `MMOAUTH` cookie the callback must bring back.
    pub async fn authorization_code_url(
        &self,
        headers: &HeaderMap,
        service: &str,
        mut props: BTreeMap<String, String>,
        login_hint: &str,
    ) -> ApiResult<(String, String)> {
        let sso = self.sso_provider(service)?;
        let secure = protocol(headers) == "https";
        let cookie_value = new_id();
        let sub = crate::web::subpath(self.cfg.str("ServiceSettings.SiteURL"));
        let expires = crate::users::httpdate(now_ms() / 1000 + COOKIE_MAX_AGE);
        let cookie = format!(
            "{COOKIE_OAUTH}={cookie_value}; Path={sub}; Expires={expires}; Max-Age={COOKIE_MAX_AGE}; HttpOnly{}",
            if secure { "; Secure" } else { "" }
        );
        let extra = format!(
            "{}:{}:{cookie_value}",
            props.get("email").map(String::as_str).unwrap_or(""),
            props.get("action").map(String::as_str).unwrap_or("")
        );
        let token = new_random_string(64);
        sqlx::query("INSERT INTO Tokens (Token, CreateAt, Type, Extra) VALUES ($1, $2, 'oauth', $3)")
            .bind(&token)
            .bind(now_ms())
            .bind(&extra)
            .execute(&self.db)
            .await
            .map_err(|e| AppError::internal("app.recover.save.app_error", e.to_string()))?;
        props.insert("token".into(), token);
        use base64::Engine;
        let state = base64::engine::general_purpose::STANDARD.encode(go_map_json(&props));
        let redirect_uri = format!("{}/signup/{service}/complete", self.site_url_or_request(headers));
        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&state={}",
            sso.auth_endpoint,
            sso.id,
            gourl::query_escape(&redirect_uri),
            gourl::query_escape(&state)
        );
        if !sso.scope.is_empty() {
            url.push_str(&format!("&scope={}", url_encode(&sso.scope)));
        }
        if !login_hint.is_empty() {
            url.push_str(&format!("&login_hint={}", url_encode(login_hint)));
        }
        Ok((url, cookie))
    }

    /// `GetOAuthLoginEndpoint`.
    #[allow(clippy::too_many_arguments)]
    async fn oauth_login_endpoint(
        &self,
        headers: &HeaderMap,
        service: &str,
        action: &str,
        redirect_to: &str,
        login_hint: &str,
        is_mobile: bool,
        desktop_token: &str,
        invite_token: &str,
        invite_id: &str,
    ) -> ApiResult<(String, String)> {
        let mut props = BTreeMap::new();
        props.insert("action".to_string(), action.to_string());
        if !invite_token.is_empty() {
            props.insert("invite_token".into(), invite_token.into());
        } else if !invite_id.is_empty() {
            props.insert("invite_id".into(), invite_id.into());
        }
        if !redirect_to.is_empty() {
            props.insert("redirect_to".into(), redirect_to.into());
        }
        if !desktop_token.is_empty() {
            props.insert("desktop_token".into(), desktop_token.into());
        }
        props.insert("isMobile".into(), is_mobile.to_string());
        self.authorization_code_url(headers, service, props, login_hint).await
    }

    /// `DoLogin` + `AttachSessionCookies` for a user who signed in another way than a password: the new session,
    /// with its `Token` header and cookies set on `h` (cookies only when `cookies`).
    #[allow(clippy::too_many_arguments)]
    pub async fn login_session(
        &self,
        req: &HeaderMap,
        h: &mut HeaderMap,
        user: &User,
        device_id: &str,
        mut is_mobile: bool,
        is_oauth: bool,
        is_saml: bool,
        cookies: bool,
    ) -> ApiResult<Session> {
        if !device_id.is_empty() {
            if !valid_device_id(device_id) {
                return Err(AppError::bad_request("api.user.attach_device_id.invalid_device_id.app_error", ""));
            }
            is_mobile = true;
        }
        let ua = req.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
        let mut s = Session {
            user_id: user.id.clone(),
            roles: user.roles.clone(),
            device_id: device_id.into(),
            ..Default::default()
        };
        let p = &mut s.props.0;
        p.insert("isMobile".into(), is_mobile.to_string());
        p.insert("isSaml".into(), is_saml.to_string());
        p.insert("isOAuthUser".into(), is_oauth.to_string());
        p.insert(session::PROP_CSRF.into(), new_id());
        let agent = crate::user_agent::parse(ua);
        p.insert("platform".into(), agent.platform.into());
        p.insert("os".into(), agent.os.into());
        p.insert("browser".into(), agent.browser);
        p.insert("is_guest".into(), user.is_guest().to_string());
        let hours = if is_mobile {
            self.cfg.i64("ServiceSettings.SessionLengthMobileInHours")
        } else if is_oauth || is_saml {
            self.cfg.i64("ServiceSettings.SessionLengthSSOInHours")
        } else {
            self.cfg.i64("ServiceSettings.SessionLengthWebInHours")
        };
        s.token = new_id();
        s.expires_at = now_ms() + hours * 3_600_000;
        let internal = |mut e: AppError| {
            e.status_code = 500;
            e
        };
        if !device_id.is_empty() {
            // RevokeOtherSessionsForDeviceId
            let old: Vec<(String, String)> =
                sqlx::query_as("SELECT Id, Token FROM Sessions WHERE UserId = $1 AND DeviceId = $2")
                    .bind(&user.id)
                    .bind(device_id)
                    .fetch_all(&self.db)
                    .await
                    .map_err(|e| internal(e.into()))?;
            for (id, token) in old {
                let _ = self.revoke_session(&Session { id, token, ..Default::default() }).await;
            }
        }
        // CreateSession
        self.limit_sessions(&user.id).await.map_err(internal)?;
        self.save_session(&mut s).await.map_err(internal)?;
        self.clear_imported_inactive(user).await;
        sqlx::query("UPDATE Users SET LastLogin = $1, UpdateAt = $3 WHERE Id = $2")
            .bind(s.create_at)
            .bind(&user.id)
            .bind(now_ms())
            .execute(&self.db)
            .await
            .map_err(|e| AppError::internal("app.login.doLogin.updateLastLogin.error", e.to_string()))?;
        self.invalidate_user(&user.id);
        h.insert("Token", HeaderValue::from_str(&s.token).unwrap());
        if cookies {
            crate::users::attach_cookies(self, h, &s, req);
        }
        Ok(s)
    }

    /// `GetUserByAuth`.
    async fn user_by_auth(&self, auth_data: &str, service: &str) -> ApiResult<Option<User>> {
        if auth_data.is_empty() {
            return Ok(None);
        }
        let users = crate::users::query_users(
            self,
            "",
            "u.AuthData = $1 AND u.AuthService = $2",
            "",
            vec![crate::users::Arg::S(auth_data.into()), crate::users::Arg::S(service.into())],
        )
        .await
        .map_err(|e| AppError::internal("app.user.get_by_auth.other.app_error", e.detailed_error))?;
        Ok(users.into_iter().next())
    }

    async fn user_by(&self, cond: &str, v: &str) -> Option<User> {
        crate::users::query_users(self, "", cond, "", vec![crate::users::Arg::S(v.into())])
            .await
            .ok()?
            .into_iter()
            .next()
    }

    /// `CreateOAuthUser`.
    async fn create_oauth_user(
        &self,
        service: &str,
        info: &[u8],
        invite_token: &str,
        invite_id: &str,
    ) -> ApiResult<User> {
        if !self.cfg.bool("TeamSettings.EnableUserCreation") {
            return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.user.create_user.disabled.app_error", ""));
        }
        self.sso_provider(service)?;
        let mut user = gitlab_user(info, self).map_err(|e| {
            AppError::internal("api.user.create_oauth_user.create.app_error", e).with_param("Service", service)
        })?;
        let mut count = 0;
        while crate::user_edit::valid_username(&user.username)
            && is_valid_username(&user.username)
            && self.user_by("u.Username = $1", &user.username).await.is_some()
        {
            user.username = format!("{}{count}", user.username);
            count += 1;
        }
        let auth = user.auth_data.clone().unwrap_or_default();
        if let Ok(Some(u)) = self.user_by_auth(&auth, service).await {
            return Ok(u);
        }
        if let Some(by_email) = self.user_by("u.Email = $1", &user.email).await {
            // GitLab's `IsSameUser` compares two pointers, so an account with the same email never matches.
            let auth_service = if by_email.auth_service.is_empty() { "email" } else { by_email.auth_service.as_str() };
            return Err(AppError::bad_request("api.user.create_oauth_user.already_attached.app_error", "")
                .with_params(&[("Service", service), ("Auth", auth_service)])
                .detail(format!("email={}", user.email)));
        }
        user.email_verified = true;
        let ruser = self.create_user(user).await?;
        if let Err(e) = self.add_user_to_team_by_invite_if_needed(&ruser, invite_token, invite_id).await {
            eprintln!("Failed to add user to team: {}", e.id);
        }
        Ok(ruser)
    }

    /// `LoginByOAuth`.
    async fn login_by_oauth(&self, service: &str, info: &[u8], invite_token: &str, invite_id: &str) -> ApiResult<User> {
        self.sso_provider(service)?;
        let parse =
            || AppError::bad_request("api.user.login_by_oauth.parse.app_error", "").with_param("Service", service);
        let auth_user = gitlab_user(info, self).map_err(|e| parse().detail(e))?;
        let auth = auth_user.auth_data.clone().unwrap_or_default();
        if auth.is_empty() {
            return Err(parse());
        }
        let user = match self.user_by_auth(&auth, service).await? {
            None => return self.create_oauth_user(service, info, invite_token, invite_id).await,
            Some(u) => u,
        };
        if user.is_bot {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "api.user.login_by_oauth.bot_login_forbidden.app_error",
                "",
            ));
        }
        let user = self.update_oauth_user_attrs(user, auth_user).await?;
        if let Err(e) = self.add_user_to_team_by_invite_if_needed(&user, invite_token, invite_id).await {
            eprintln!("Failed to add user to team: {}", e.id);
        }
        Ok(user)
    }

    /// `UpdateOAuthUserAttrs`: the username, name and email the provider has now (unless taken), and the account
    /// active again.
    async fn update_oauth_user_attrs(&self, mut user: User, from: User) -> ApiResult<User> {
        // The stored row as the store reads it before writing: it keeps the columns a profile update never sets.
        let old = self.user(&user.id).await?.map(|u| (*u).clone()).ok_or_else(crate::users::missing_account)?;
        let mut changed = false;
        if from.username != user.username && self.user_by("u.Username = $1", &from.username).await.is_none() {
            user.username = from.username.clone();
            changed = true;
        }
        if from.full_name() != user.full_name() {
            user.first_name = from.first_name.clone();
            user.last_name = from.last_name.clone();
            changed = true;
        }
        if from.email != user.email && self.user_by("u.Email = $1", &from.email).await.is_none() {
            user.email = from.email.clone();
            changed = true;
        }
        if user.delete_at > 0 {
            user.delete_at = 0;
            changed = true;
        }
        if changed {
            // `Store().User().Update(user, trusted)`: the whole row, so the store's rules apply too (an SSO
            // account's email marked verified, the mention keys of a renamed user).
            crate::user_edit::pre_update(&mut user);
            crate::user_edit::check_user(&user)?;
            self.store_update_user(&mut user, &old, true).await.map_err(|e| {
                if e.status_code == 400 && e.id.starts_with("app.user.save.") {
                    AppError::internal("app.user.update.finding.app_error", e.message)
                } else {
                    e
                }
            })?;
            self.invalidate_user(&user.id);
            self.clear_sessions_for_user(&user.id);
        }
        Ok(user)
    }

    /// `AddUserToTeamByInviteIfNeeded`: a team invite token or invite id carried through the sign-in.
    async fn add_user_to_team_by_invite_if_needed(
        &self,
        user: &User,
        invite_token: &str,
        invite_id: &str,
    ) -> ApiResult<()> {
        let team = if !invite_token.is_empty() {
            let t = token_row(self, invite_token)
                .await
                .ok_or_else(|| AppError::bad_request("api.user.create_user.signup_link_invalid.app_error", ""))?;
            self.add_user_to_team_with_token(user, t).await?
        } else if !invite_id.is_empty() {
            let team = crate::teams::team_by_invite(self, invite_id).await?;
            if team.group_constrained == Some(true) {
                return Err(AppError::new(StatusCode::FORBIDDEN, "app.team.invite_id.group_constrained.error", ""));
            }
            self.join_user_to_team(&team, user, "").await?;
            team
        } else {
            return Ok(());
        };
        if let Err(e) = self.add_direct_channels(&team.id, &user.id).await {
            eprintln!("Failed to add direct channels: {}", e.id);
        }
        Ok(())
    }

    /// `AddUserToTeamWithToken`.
    async fn add_user_to_team_with_token(&self, user: &User, t: TokenRow) -> ApiResult<crate::model::Team> {
        let invalid = || AppError::bad_request("api.user.create_user.signup_link_invalid.app_error", "");
        if !matches!(t.kind.as_str(), "team_invitation" | "guest_invitation" | "guest_magic_link_invitation") {
            return Err(invalid());
        }
        if now_ms() > t.create_at + 48 * 3_600_000 {
            delete_token(self, &t.token).await;
            return Err(AppError::bad_request("api.user.create_user.signup_link_expired.app_error", ""));
        }
        let data = crate::app::map_from_json(t.extra.as_bytes());
        let team_id = data.get("teamId").cloned().unwrap_or_default();
        let team: crate::model::Team = crate::db::get(&self.db, &team_id)
            .await
            .map_err(|e| AppError::internal("app.team.get.finding.app_error", e.to_string()))?
            .ok_or_else(|| AppError::not_found("app.team.get.find.app_error", ""))?;
        if team.group_constrained == Some(true) {
            return Err(AppError::new(StatusCode::FORBIDDEN, "app.team.invite_token.group_constrained.error", ""));
        }
        let wrong_type = || AppError::bad_request("api.user.create_user.invalid_invitation_type.app_error", "");
        if user.is_guest() && t.kind == "team_invitation" {
            return Err(wrong_type());
        }
        if !user.is_guest() && t.kind != "team_invitation" {
            return Err(wrong_type());
        }
        if let Some(e) = data.get("email").filter(|e| !e.is_empty())
            && !e.eq_ignore_ascii_case(&user.email)
        {
            return Err(AppError::bad_request("api.user.create_user.bad_token_email_data.app_error", ""));
        }
        self.join_user_to_team(&team, user, "").await?;
        if t.kind != "team_invitation" {
            let ids: Vec<String> =
                data.get("channels").map(|c| c.split(' ').map(str::to_string).collect()).unwrap_or_default();
            for id in ids {
                if let Ok(Some(c)) = self.channel(&id).await
                    && let Err(e) = self.add_user_to_channel(user, &c, false).await
                {
                    eprintln!("Error adding user to channel: {}", e.id);
                }
            }
        }
        delete_token(self, &t.token).await;
        Ok(team)
    }

    /// `GenerateAndSaveDesktopToken`.
    async fn desktop_token(&self, user_id: &str) -> ApiResult<String> {
        let token = new_random_string(64);
        let r = sqlx::query("INSERT INTO DesktopTokens (Token, CreateAt, UserId) VALUES ($1, $2, $3)")
            .bind(&token)
            .bind(now_ms() / 1000)
            .bind(user_id)
            .execute(&self.db)
            .await;
        if let Err(e) = r {
            let _ = sqlx::query("DELETE FROM DesktopTokens WHERE UserId = $1").bind(user_id).execute(&self.db).await;
            return Err(AppError::bad_request("app.desktop_token.generateServerToken.invalid_or_expired", "")
                .detail(e.to_string()));
        }
        Ok(token)
    }
}

/// `model.IsValidStandardDeviceId`-ish: Go accepts `<platform>:<id>` and `<platform>_v2:<id>` and plain ids up to
/// 512 characters.
fn valid_device_id(id: &str) -> bool {
    id.len() <= 512
}

/// `model.IsValidUsername`.
fn is_valid_username(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_'))
        && !matches!(s, "all" | "channel" | "matterbot" | "system")
}

/// `model.CleanUsername`.
pub fn clean_username(username: &str) -> String {
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
    let mut s = username.replace(' ', "-").to_lowercase();
    if RESERVED.contains(&s.as_str()) {
        s.clear();
    }
    let s: String = s
        .trim()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_') { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if is_valid_username(&s) { s } else { format!("a{}", new_id()) }
}

/// `GitLabProvider.GetUserFromJSON`: the user GitLab describes.
fn gitlab_user(info: &[u8], app: &App) -> Result<User, String> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct GitLabUser {
        id: i64,
        username: String,
        login: String,
        email: String,
        name: String,
        preferred_username: String,
    }
    let g: GitLabUser = serde_json::Deserializer::from_slice(info)
        .into_iter::<GitLabUser>()
        .next()
        .ok_or_else(|| "EOF".to_string())?
        .map_err(|e| e.to_string())?;
    if g.id == 0 {
        return Err("user id can't be 0".into());
    }
    if g.email.is_empty() {
        return Err("user e-mail should not be empty".into());
    }
    let username = if app.cfg.bool("GitLabSettings.UsePreferredUsername") && !g.preferred_username.is_empty() {
        g.preferred_username.split('@').next().unwrap_or("").to_string()
    } else if !g.username.is_empty() {
        g.username.clone()
    } else {
        g.login.clone()
    };
    let mut u = User { username: clean_username(&username), ..Default::default() };
    let split: Vec<&str> = g.name.split(' ').collect();
    if split.len() >= 2 {
        u.first_name = split[0].to_string();
        u.last_name = split[1..].join(" ");
    } else {
        u.first_name = g.name.clone();
    }
    u.email = g.email.to_lowercase();
    u.auth_data = Some(g.id.to_string());
    u.auth_service = "gitlab".into();
    Ok(u)
}

/// `utils.URLEncode`: `url.QueryEscape` of each space-separated part, joined with `%20`.
fn url_encode(s: &str) -> String {
    s.split(' ').map(gourl::query_escape).collect::<Vec<_>>().join("%20")
}

struct TokenRow {
    token: String,
    create_at: i64,
    kind: String,
    extra: String,
}

async fn token_row(app: &App, token: &str) -> Option<TokenRow> {
    let r: Option<(String, i64, String, String)> = sqlx::query_as(
        "SELECT Token, COALESCE(CreateAt, 0), COALESCE(Type, ''), COALESCE(Extra, '') FROM Tokens WHERE Token = $1",
    )
    .bind(token)
    .fetch_optional(&app.db)
    .await
    .ok()?;
    r.map(|(token, create_at, kind, extra)| TokenRow { token, create_at, kind, extra })
}

async fn delete_token(app: &App, token: &str) {
    let _ = sqlx::query("DELETE FROM Tokens WHERE Token = $1").bind(token).execute(&app.db).await;
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for c in headers.get_all(header::COOKIE) {
        for kv in c.to_str().unwrap_or("").split(';') {
            if let Some((k, v)) = kv.trim().split_once('=')
                && k == name
            {
                return Some(v.trim_matches('"').to_string());
            }
        }
    }
    None
}

/// `utils.ValidateWebAuthRedirectUrl`: a relative URL, or one on the site's scheme and host.
fn validate_web_auth_redirect(app: &App, redirect: &str) -> Result<(), String> {
    let u = Url::parse(redirect).map_err(|e| format!("failed to parse redirect URL: {e}"))?;
    if u.scheme.is_empty() && u.host.is_empty() {
        return Ok(());
    }
    let site = Url::parse(app.cfg.str("ServiceSettings.SiteURL"))
        .map_err(|e| format!("failed to parse SiteURL from config: {e}"))?;
    if u.scheme != site.scheme {
        return Err(format!("redirect URL scheme {:?} does not match site URL scheme {:?}", u.scheme, site.scheme));
    }
    if u.host != site.host {
        return Err(format!("redirect URL host {:?} does not match site URL host {:?}", u.host, site.host));
    }
    Ok(())
}

fn custom_schemes(app: &App) -> Vec<String> {
    app.cfg
        .get("NativeAppSettings.AppCustomURLSchemes")
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// `utils.IsValidMobileAuthRedirectURL`.
fn valid_mobile_redirect(app: &App, redirect: &str) -> bool {
    let r = redirect.to_lowercase();
    custom_schemes(app).iter().any(|s| r.starts_with(&s.to_lowercase()))
}

/// `pathWithinPrefix`.
fn path_within_prefix(target: &str, prefix: &str) -> bool {
    let target = gourl::clean_path(target);
    let prefix = gourl::clean_path(prefix);
    if prefix == "." || prefix == "/" {
        return target == "." || target.starts_with('/');
    }
    target == prefix || target.starts_with(&format!("{prefix}/"))
}

/// `fullyQualifiedRedirectURL`: a redirect kept on the site (or a native app's `callback`), else the site.
fn fully_qualified_redirect(site_prefix: &str, target: &str, schemes: &[String]) -> String {
    let Ok(parsed) = Url::parse(target) else { return site_prefix.into() };
    let Ok(prefix) = Url::parse(site_prefix) else { return site_prefix.into() };
    if schemes.contains(&format!("{}://", parsed.scheme))
        && parsed.host == "callback"
        && parsed.raw_path.is_empty()
        && parsed.raw_query.is_empty()
        && parsed.fragment.as_deref().unwrap_or("").is_empty()
    {
        return target.into();
    }
    if parsed.scheme == prefix.scheme
        && parsed.host == prefix.host
        && path_within_prefix(&parsed.path(), &prefix.path())
    {
        return target.into();
    } else if !parsed.scheme.is_empty() || !parsed.host.is_empty() {
        return site_prefix.into();
    }
    let mut t = target.to_string();
    if !t.is_empty() && !t.starts_with('/') {
        t = format!("/{t}");
    }
    let Ok(joined) = gourl::url_join_path(site_prefix, &t) else { return site_prefix.into() };
    let Ok(unescaped) = gourl::path_unescape(&joined) else { return site_prefix.into() };
    let Ok(p) = Url::parse(&unescaped) else { return site_prefix.into() };
    if !path_within_prefix(&p.path(), &prefix.path()) {
        return site_prefix.into();
    }
    p.to_go_string()
}

/// `utils.AppendQueryParamsToURL`: Go ranges over a map, but `Encode` sorts, so the order is fixed.
fn append_query_params(base: &str, params: &[(&str, &str)]) -> String {
    let Ok(mut u) = Url::parse(base) else { return String::new() };
    let (mut q, bad) = gourl::parse_query(&u.raw_query);
    if bad {
        return String::new();
    }
    for (k, v) in params {
        q.add(k, v);
    }
    u.raw_query = q.encode();
    u.to_go_string()
}

// ---------- pages ----------

/// `template.HTMLEscapeString`.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('\'', "&#39;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&#34;")
        .replace('\0', "\u{FFFD}")
}

/// `RenderMobileMessage`: a page the mobile app's browser shows, status 200.
fn mobile_message(message: &str) -> Response {
    let body = format!(
        "\n\t\t<!DOCTYPE html>\n\t\t<html>\n\t\t\t<head>\n\t\t\t\t<meta charset=\"utf-8\">\n\t\t\t\t<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0, minimum-scale=1.0, user-scalable=yes, viewport-fit=cover\">\n\t\t\t\t<style>\n\t\t\t\t\tbody {{\n\t\t\t\t\t\tcolor: #333;\n\t\t\t\t\t\tbackground-color: #fff;\n\t\t\t\t\t\tfont-family: \"Helvetica Neue\",Helvetica,Arial,sans-serif;\n\t\t\t\t\t\tfont-size: 14px;\n\t\t\t\t\t\tline-height: 1.42857143;\n\t\t\t\t\t}}\n\t\t\t\t\ta {{\n\t\t\t\t\t\tcolor: #337ab7;\n\t\t\t\t\t\ttext-decoration: none;\n\t\t\t\t\t}}\n\t\t\t\t\ta:focus, a:hover {{\n\t\t\t\t\t\tcolor: #23527c;\n\t\t\t\t\t\ttext-decoration: underline;\n\t\t\t\t\t}}\n\t\t\t\t\th2 {{\n\t\t\t\t\t\tfont-size: 30px;\n\t\t\t\t\t\tmargin: 20px 0 10px 0;\n\t\t\t\t\t\tfont-weight: 500;\n\t\t\t\t\t\tline-height: 1.1\n\t\t\t\t\t}}\n\t\t\t\t\tp {{\n\t\t\t\t\t\tmargin: 0 0 10px;\n\t\t\t\t\t}}\n\t\t\t\t\t.message-container {{\n\t\t\t\t\t\tcolor: #555;\n\t\t\t\t\t\tdisplay: table-cell;\n\t\t\t\t\t\tpadding: 5em 0;\n\t\t\t\t\t\ttext-align: left;\n\t\t\t\t\t\tvertical-align: top;\n\t\t\t\t\t}}\n\t\t\t\t</style>\n\t\t\t</head>\n\t\t\t<body>\n\t\t\t\t<!-- mobile app message -->\n\t\t\t\t<div class=\"message-container\">\n\t\t\t\t\t{message}\n\t\t\t\t</div>\n\t\t\t</body>\n\t\t</html>\n\t\n"
    );
    let mut r = body.into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
    r
}

/// `RenderMobileError`: the error, with a link back to the app (or the site for a scheme the app does not own).
fn mobile_error(app: &App, e: &AppError, redirect: &str) -> Response {
    let schemes = custom_schemes(app);
    let mut link = html_escape(redirect);
    match Url::parse(redirect) {
        Ok(u) if schemes.contains(&u.scheme) => {}
        _ => link = html_escape(app.cfg.str("ServiceSettings.SiteURL")),
    }
    let back = crate::i18n::tr("api.back_to_app", &[("SiteName", crate::brand::name())]).unwrap_or_default();
    let err = crate::i18n::t("error").unwrap_or("Error");
    mobile_message(&format!(
        "\n\t\t<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 576 512\" style=\"width: 64px; height: 64px; fill: #ccc\">\n\t\t\t<!-- Font Awesome Free 5.15.3 by @fontawesome - https://fontawesome.com License - https://fontawesome.com/license/free (Icons: CC BY 4.0, Fonts: SIL OFL 1.1, Code: MIT License) -->\n\t\t\t<path d=\"M569.517 440.013C587.975 472.007 564.806 512 527.94 512H48.054c-36.937 0-59.999-40.055-41.577-71.987L246.423 23.985c18.467-32.009 64.72-31.951 83.154 0l239.94 416.028zM288 354c-25.405 0-46 20.595-46 46s20.595 46 46 46 46-20.595 46-46-20.595-46-46-46zm-43.673-165.346l7.418 136c.347 6.364 5.609 11.346 11.982 11.346h48.546c6.373 0 11.635-4.982 11.982-11.346l7.418-136c.375-6.874-5.098-12.654-11.982-12.654h-63.383c-6.884 0-12.356 5.78-11.981 12.654z\"/>\n\t\t</svg>\n\t\t<h2> {err} </h2>\n\t\t<p> {} </p>\n\t\t<a href=\"{link}\">\n\t\t\t{back}\n\t\t</a>\n\t",
        html_escape(&e.message)
    ))
}

/// `RenderMobileAuthComplete`: the page that hands the session to the mobile app.
fn mobile_auth_complete(redirect: &str) -> Response {
    let link = html_escape(redirect);
    let t = |id: &str| crate::i18n::t(id).unwrap_or("").to_string();
    let click = crate::i18n::tr("api.oauth.click_redirect", &[("Link", &html_escape(redirect))]).unwrap_or_default();
    mobile_message(&format!(
        "\n\t\t<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 512 512\" style=\"width: 64px; height: 64px; fill: #3c763d\">\n\t\t\t<!-- Font Awesome Free 5.15.3 by @fontawesome - https://fontawesome.com License - https://fontawesome.com/license/free (Icons: CC BY 4.0, Fonts: SIL OFL 1.1, Code: MIT License) -->\n\t\t\t<path stroke=\"green\" d=\"M504 256c0 136.967-111.033 248-248 248S8 392.967 8 256 119.033 8 256 8s248 111.033 248 248zM227.314 387.314l184-184c6.248-6.248 6.248-16.379 0-22.627l-22.627-22.627c-6.248-6.249-16.379-6.249-22.628 0L216 308.118l-70.059-70.059c-6.248-6.248-16.379-6.248-22.628 0l-22.627 22.627c-6.248 6.248-6.248 16.379 0 22.627l104 104c6.249 6.249 16.379 6.249 22.628.001z\"/>\n\t\t</svg>\n\t\t<h2> {} </h2>\n\t\t<p id=\"redirecting-message\"> {} </p>\n\t\t<p id=\"close-tab-message\" style=\"display: none\"> {} </p>\n\t\t<p> {click} </p>\n\t\t<meta http-equiv=\"refresh\" content=\"2; url={link}\">\n\t\t<script>\n\t\t\twindow.onload = function() {{\n\t\t\t\tsetTimeout(function() {{\n\t\t\t\t\tdocument.getElementById('redirecting-message').style.display = 'none';\n\t\t\t\t\tdocument.getElementById('close-tab-message').style.display = 'block';\n\t\t\t\t}}, 2000);\n\t\t\t}}\n\t\t</script>\n\t",
        t("api.oauth.auth_complete"),
        t("api.oauth.redirecting_back"),
        t("api.oauth.close_browser"),
    ))
}

// ---------- handlers ----------

fn query(req: &Request) -> gourl::Values {
    gourl::parse_query(req.uri().query().unwrap_or("")).0
}

/// No route for a service outside `[A-Za-z0-9]+`: the client's page, as for any unknown GET.
fn no_route(app: &State, req: &Request) -> impl Future<Output = Response> + Send + use<> {
    let (app, headers, uri) = (app.clone(), req.headers().clone(), req.uri().clone());
    async move { crate::web::root_for(&app, &headers, &uri).await }
}

fn with_cookie(mut r: Response, cookie: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(cookie) {
        r.headers_mut().append(header::SET_COOKIE, v);
    }
    r
}

/// `loginWithOAuth`.
async fn login(St(app): St<State>, MaybeAuth(s): MaybeAuth, Path(service): Path<String>, req: Request) -> Response {
    if !valid_service(&service) {
        return no_route(&app, &req).await;
    }
    let headers = req.headers().clone();
    let q = query(&req);
    let redirect_to = q.get("redirect_to");
    if !redirect_to.is_empty()
        && let Err(e) = validate_web_auth_redirect(&app, redirect_to)
    {
        return web_error(&app, &headers, AppError::bad_request("api.invalid_redirect_url", "").detail(e)).await;
    }
    match app
        .oauth_login_endpoint(
            &headers,
            &service,
            ACTION_LOGIN,
            redirect_to,
            q.get("login_hint"),
            false,
            q.get("desktop_token"),
            q.get("t"),
            q.get("id"),
        )
        .await
    {
        Ok((url, cookie)) => {
            crate::audit::log(&app, s.as_deref(), "success").await;
            with_cookie(redirect(StatusCode::FOUND, &url, "application/json"), &cookie)
        }
        Err(e) => web_error(&app, &headers, e).await,
    }
}

/// `mobileLoginWithOAuth`.
async fn mobile_login(
    St(app): St<State>,
    MaybeAuth(s): MaybeAuth,
    Path(service): Path<String>,
    req: Request,
) -> Response {
    if !valid_service(&service) {
        return no_route(&app, &req).await;
    }
    let headers = req.headers().clone();
    let q = query(&req);
    let redirect_to = crate::web::html_escape(q.get("redirect_to"));
    if !redirect_to.is_empty() && !valid_mobile_redirect(&app, &redirect_to) {
        let e = AppError::bad_request("api.invalid_custom_url_scheme", "");
        return mobile_error(&app, &e, &redirect_to);
    }
    match app
        .oauth_login_endpoint(&headers, &service, ACTION_MOBILE, &redirect_to, "", true, "", q.get("t"), q.get("id"))
        .await
    {
        Ok((url, cookie)) => {
            crate::audit::log(&app, s.as_deref(), "success").await;
            with_cookie(redirect(StatusCode::FOUND, &url, "application/json"), &cookie)
        }
        Err(e) => web_error(&app, &headers, e).await,
    }
}

/// `signupWithOAuth`.
async fn signup(St(app): St<State>, MaybeAuth(s): MaybeAuth, Path(service): Path<String>, req: Request) -> Response {
    if !valid_service(&service) {
        return no_route(&app, &req).await;
    }
    let headers = req.headers().clone();
    if !app.cfg.bool("TeamSettings.EnableUserCreation") {
        let m = crate::i18n::t("api.oauth.signup_with_oauth.disabled.app_error").unwrap_or("");
        return render_web_error(&app, StatusCode::BAD_REQUEST, &[("message", m)]).await;
    }
    let q = query(&req);
    let mut props = BTreeMap::new();
    props.insert("action".to_string(), ACTION_SIGNUP.to_string());
    if !q.get("t").is_empty() {
        props.insert("invite_token".into(), q.get("t").into());
    } else if !q.get("id").is_empty() {
        props.insert("invite_id".into(), q.get("id").into());
    }
    if !q.get("desktop_token").is_empty() {
        props.insert("desktop_token".into(), q.get("desktop_token").into());
    }
    match app.authorization_code_url(&headers, &service, props, "").await {
        Ok((url, cookie)) => {
            crate::audit::log(&app, s.as_deref(), "success").await;
            with_cookie(redirect(StatusCode::FOUND, &url, "application/json"), &cookie)
        }
        Err(e) => web_error(&app, &headers, e).await,
    }
}

/// What `AuthorizeOAuthUser` learns: the provider's user info, and the state's props.
type Authorized = (Vec<u8>, BTreeMap<String, String>);

impl App {
    /// `AuthorizeOAuthUser`: the state checked against its token and the `MMOAUTH` cookie (which is cleared),
    /// the code exchanged at the provider, and the user info read with the access token. The props come back
    /// with an error too, once the state is read.
    async fn authorize_oauth_user(
        &self,
        headers: &HeaderMap,
        cleared_cookie: &mut Option<String>,
        service: &str,
        code: &str,
        state: &str,
        redirect_uri: &str,
    ) -> Result<Authorized, Box<(AppError, Option<BTreeMap<String, String>>)>> {
        use base64::Engine;
        let sso = self.sso_provider(service).map_err(|e| Box::new((e, None)))?;
        let invalid = || AppError::bad_request("api.user.authorize_oauth_user.invalid_state.app_error", "");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(state)
            .map_err(|e| Box::new((invalid().detail(e.to_string()), None)))?;
        let props: BTreeMap<String, String> = crate::app::map_from_json(&decoded).into_iter().collect();
        let fail = |e: AppError| Box::new((e, Some(props.clone())));
        let token = props.get("token").map(String::as_str).unwrap_or("");
        let t = token_row(self, token)
            .await
            .filter(|t| t.kind == "oauth")
            .ok_or_else(|| fail(AppError::bad_request("api.oauth.invalid_state_token.app_error", "")))?;
        let state_email = props.get("email").map(String::as_str).unwrap_or("");
        let state_action = props.get("action").map(String::as_str).unwrap_or("");
        if state_action == ACTION_EMAIL_TO_SSO && state_email.is_empty() {
            return Err(fail(invalid()));
        }
        let cookie = cookie_value(headers, COOKIE_OAUTH).ok_or_else(|| fail(invalid()))?;
        let parts: Vec<&str> = t.extra.split(':').collect();
        if parts.len() != 3 || parts[0] != state_email || parts[1] != state_action || parts[2] != cookie {
            return Err(fail(invalid()));
        }
        delete_token(self, &t.token).await;
        let sub = crate::web::subpath(self.cfg.str("ServiceSettings.SiteURL"));
        *cleared_cookie = Some(format!("{COOKIE_OAUTH}=; Path={sub}; Max-Age=0; HttpOnly"));

        let token_failed =
            |d: String| fail(AppError::internal("api.user.authorize_oauth_user.token_failed.app_error", d));
        let mut form = gourl::Values::default();
        form.set("client_id", &sso.id);
        form.set("client_secret", &sso.secret);
        form.set("code", code);
        form.set("grant_type", "authorization_code");
        form.set("redirect_uri", redirect_uri);
        let url = reqwest::Url::parse(&sso.token_endpoint).map_err(|e| token_failed(e.to_string()))?;
        if !self.untrusted_url_allowed(&url) {
            return Err(token_failed("address forbidden".into()));
        }
        let resp = self
            .integration_client()
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(form.encode())
            .send()
            .await
            .map_err(|e| token_failed(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.unwrap_or_default();
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Access {
            access_token: String,
            token_type: String,
        }
        let ar = serde_json::Deserializer::from_slice(&body).into_iter::<Option<Access>>().next();
        let ar = match ar {
            Some(Ok(a)) if status == 200 => a.unwrap_or_default(),
            other => {
                let err = match other {
                    Some(Err(e)) => e.to_string(),
                    None => "EOF".into(),
                    _ => "<nil>".into(),
                };
                let d = format!(
                    "response_body={}, status_code={status}, error={err}",
                    redact_token_response(&String::from_utf8_lossy(&body))
                );
                return Err(fail(AppError::internal("api.user.authorize_oauth_user.bad_response.app_error", d)));
            }
        };
        let body_s = redact_token_response(&String::from_utf8_lossy(&body));
        if ar.token_type.to_lowercase() != "bearer" {
            let d = format!("token_type={}, response_body={body_s}", ar.token_type);
            return Err(fail(AppError::internal("api.user.authorize_oauth_user.bad_token.app_error", d)));
        }
        if ar.access_token.is_empty() {
            return Err(fail(AppError::internal(
                "api.user.authorize_oauth_user.missing.app_error",
                format!("response_body={body_s}"),
            )));
        }
        let service_err = |d: String| {
            fail(
                AppError::internal("api.user.authorize_oauth_user.service.app_error", d).with_param("Service", service),
            )
        };
        let url = reqwest::Url::parse(&sso.user_api_endpoint).map_err(|e| service_err(e.to_string()))?;
        if !self.untrusted_url_allowed(&url) {
            return Err(service_err("address forbidden".into()));
        }
        let resp = self
            .integration_client()
            .get(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {}", ar.access_token))
            .send()
            .await
            .map_err(|e| service_err(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.unwrap_or_default().to_vec();
        if status != 200 {
            let text = String::from_utf8_lossy(&body).into_owned();
            if service == "gitlab" && status == 403 && text.contains("Terms of Service") {
                let host = Url::parse(&sso.user_api_endpoint).map(|u| u.hostname().to_string()).unwrap_or_default();
                return Err(fail(AppError::bad_request("oauth.gitlab.tos.error", "").with_param("URL", &host)));
            }
            return Err(fail(AppError::internal(
                "api.user.authorize_oauth_user.response.app_error",
                format!("response_body={text}"),
            )));
        }
        Ok((body, props))
    }

    /// `CompleteOAuth`.
    async fn complete_oauth(&self, service: &str, info: &[u8], props: &BTreeMap<String, String>) -> ApiResult<User> {
        let g = |k: &str| props.get(k).map(String::as_str).unwrap_or("");
        let (invite_token, invite_id) = (g("invite_token"), g("invite_id"));
        match g("action") {
            ACTION_SIGNUP => self.create_oauth_user(service, info, invite_token, invite_id).await,
            ACTION_EMAIL_TO_SSO => self.complete_switch_with_oauth(service, info, g("email")).await,
            _ => self.login_by_oauth(service, info, invite_token, invite_id).await,
        }
    }

    /// `CompleteSwitchWithOAuth`: an email account moved to the provider; every session ends.
    async fn complete_switch_with_oauth(&self, service: &str, info: &[u8], email: &str) -> ApiResult<User> {
        self.sso_provider(service)?;
        if email.is_empty() {
            return Err(AppError::bad_request("api.user.complete_switch_with_oauth.blank_email.app_error", ""));
        }
        let parse = || {
            AppError::bad_request("api.user.complete_switch_with_oauth.parse.app_error", "")
                .with_param("Service", service)
        };
        let sso_user = gitlab_user(info, self).map_err(|e| parse().detail(e))?;
        let auth = sso_user.auth_data.clone().unwrap_or_default();
        if auth.is_empty() {
            return Err(parse());
        }
        // `GetByEmail`: `Email = lower(?)`, so the address in the state may have any case.
        let user = self
            .user_by("u.Email = lower($1)", email)
            .await
            .ok_or_else(|| AppError::internal("app.user.missing_account.const", ""))?;
        self.revoke_all_sessions(&user.id).await?;
        // `UpdateAuthData(id, service, authData, email, resetMfa=true)`.
        let r = sqlx::query(
            "UPDATE Users SET Password = '', LastPasswordUpdate = $1, UpdateAt = $1, FailedAttempts = 0, AuthService = $2,
             AuthData = $3, Email = lower($4), MfaActive = false, MfaSecret = '', MfaUsedTimestamps = '[]' WHERE Id = $5",
        )
        .bind(now_ms())
        .bind(service)
        .bind(&auth)
        .bind(&sso_user.email)
        .bind(&user.id)
        .execute(&self.db)
        .await;
        if let Err(e) = r {
            let id = if e.to_string().contains("email") {
                "app.user.update_auth_data.email_exists.app_error"
            } else {
                "app.user.update_auth_data.app_error"
            };
            return Err(if id.contains("email_exists") {
                AppError::bad_request(id, "")
            } else {
                AppError::internal(id, e.to_string())
            });
        }
        self.invalidate_user(&user.id);
        let method = format!("{} SSO", title(service));
        let (email, locale) = (user.email.clone(), user.locale.clone());
        crate::email::go(self, "signin change email", move |app| async move {
            crate::email::send_sign_in_change(&app, &email, &method, &locale).await
        });
        Ok(user)
    }
}

/// `redactOAuthTokenResponse`: token values in a token endpoint's answer masked.
fn redact_token_response(body: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r#"(?i)((?:^|[^0-9a-z_])(?:access|refresh|id)(?:_|%5[Ff]|\\u005[Ff])token\s*"?\s*[=:]\s*"?)((?:[^&;,"'\s}\\]|\\.)*)"#)
            .unwrap()
    });
    re.replace_all(body, "${1}[REDACTED]").into_owned()
}

/// `completeOAuth`: the provider's callback.
async fn complete(
    St(app): St<State>,
    MaybeAuth(cur): MaybeAuth,
    Path(service): Path<String>,
    req: Request,
) -> Response {
    if !valid_service(&service) {
        return no_route(&app, &req).await;
    }
    let headers = req.headers().clone();
    let q = query(&req);
    let svc = title(&service);
    if q.get("error") == "access_denied" {
        return render_web_error(
            &app,
            StatusCode::TEMPORARY_REDIRECT,
            &[("type", "oauth_access_denied"), ("service", &svc)],
        )
        .await;
    }
    let code = q.get("code");
    if code.is_empty() {
        return render_web_error(
            &app,
            StatusCode::TEMPORARY_REDIRECT,
            &[("type", "oauth_missing_code"), ("service", &svc)],
        )
        .await;
    }
    let site = site_url_header(&app, &headers);
    let uri = format!("{site}/signup/{service}/complete");
    let mut cleared = None;
    let result = app.authorize_oauth_user(&headers, &mut cleared, &service, code, q.get("state"), &uri).await;
    let (info, props, err) = match result {
        Ok((info, props)) => (info, props, None),
        Err(b) => {
            let (e, props) = *b;
            (vec![], props.unwrap_or_default(), Some(e))
        }
    };
    let g = |k: &str| props.get(k).cloned().unwrap_or_default();
    let action = g("action");
    let is_mobile = action == ACTION_MOBILE;
    let has_redirect = !g("redirect_to").is_empty();
    let redirect_url = fully_qualified_redirect(&site, &g("redirect_to"), &custom_schemes(&app));
    let finish = |r: Response| match &cleared {
        Some(c) => with_cookie(r, c),
        None => r,
    };
    let render_error = |e: AppError| {
        let app = app.clone();
        let redirect_url = redirect_url.clone();
        async move {
            if is_mobile && has_redirect {
                mobile_error(&app, &e, &redirect_url)
            } else {
                let status = StatusCode::from_u16(e.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                render_web_error(&app, status, &[("message", &e.message)]).await
            }
        }
    };
    if let Some(e) = err {
        return finish(render_error(e).await);
    }
    let user = match app.complete_oauth(&service, &info, &props).await {
        Ok(u) => u,
        Err(e) => return finish(render_error(e).await),
    };
    let to = if action == ACTION_EMAIL_TO_SSO {
        format!("{site}/login?extra=signin_change")
    } else if action == ACTION_SSO_TO_EMAIL {
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        format!("{}://{host}/claim?email={}", protocol(&headers), gourl::query_escape(&g("email")))
    } else {
        let desktop_token = g("desktop_token");
        if !desktop_token.is_empty() {
            let server_token = match app.desktop_token(&user.id).await {
                Ok(t) => t,
                Err(e) => return finish(render_error(e).await),
            };
            let mut params: Vec<(&str, &str)> = vec![("client_token", &desktop_token), ("server_token", &server_token)];
            let rt = g("redirect_to");
            if props.contains_key("redirect_to") {
                params.push(("redirect_to", &rt));
            }
            if desktop_token.starts_with("dev-") {
                params.push(("isDesktopDev", "true"));
            }
            let to = append_query_params(&format!("{site}/login/desktop"), &params);
            crate::audit::log_user(&app, cur.as_deref(), &user.id, "authenticated").await;
            return finish(redirect(StatusCode::TEMPORARY_REDIRECT, &to, "text/html; charset=utf-8"));
        }
        let mut h = HeaderMap::new();
        let is_oauth = matches!(user.auth_service.as_str(), "gitlab" | "google" | "office365" | "openid");
        let mobile_with_redirect = is_mobile && has_redirect;
        let s = match app
            .login_session(&headers, &mut h, &user, "", is_mobile, is_oauth, false, !mobile_with_redirect)
            .await
        {
            Ok(s) => s,
            Err(e) => return finish(render_error(e).await),
        };
        if is_mobile && !has_redirect {
            // The old mobile app reads the cookies of an empty answer.
            let mut r = StatusCode::OK.into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            r.headers_mut().extend(h);
            crate::audit::log(&app, Some(&s), "success").await;
            return finish(r);
        }
        if mobile_with_redirect {
            let site_url = app.cfg.str("ServiceSettings.SiteURL").to_string();
            let to = append_query_params(
                &redirect_url,
                &[("MMAUTHTOKEN", &s.token), ("MMCSRF", s.prop(session::PROP_CSRF)), ("srv", &site_url)],
            );
            let mut r = mobile_auth_complete(&to);
            r.headers_mut().extend(h);
            crate::audit::log(&app, Some(&s), "success").await;
            return finish(r);
        }
        let mut r = redirect(StatusCode::TEMPORARY_REDIRECT, &redirect_url, "text/html; charset=utf-8");
        r.headers_mut().extend(h);
        crate::audit::log(&app, Some(&s), "success").await;
        return finish(r);
    };
    crate::audit::log(&app, cur.as_deref(), "success").await;
    finish(redirect(StatusCode::TEMPORARY_REDIRECT, &to, "text/html; charset=utf-8"))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IntuneLogin {
    access_token: String,
    device_id: String,
    voip_device_id: String,
}

/// `loginByIntune`: Microsoft Intune is an Enterprise feature, never available here.
async fn intune(St(app): St<State>, MaybeAuth(s): MaybeAuth, body: Bytes) -> ApiResult<Response> {
    let r: IntuneLogin = decode_json(&body, "request_body")?;
    if r.access_token.is_empty() {
        return Err(AppError::invalid_param("access_token"));
    }
    if !r.voip_device_id.is_empty() && !crate::sso::valid_voip_device_id(&r.voip_device_id) {
        return Err(AppError::invalid_param("voip_device_id"));
    }
    let _ = r.device_id;
    crate::audit::log(&app, s.as_deref(), "attempt").await;
    Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.user.login_by_intune.not_available.app_error", ""))
}

/// `model.IsValidVoIPDeviceId`.
pub fn valid_voip_device_id(id: &str) -> bool {
    id.len() <= 512
}

/// `loginWithMagicLinkToken`: a guest's one-time link.
async fn one_time_link(St(app): St<State>, MaybeAuth(s): MaybeAuth, req: Request) -> Response {
    let headers = req.headers().clone();
    let site = site_url_header(&app, &headers);
    if !app.cfg.bool("GuestAccountsSettings.EnableGuestMagicLink") {
        let to =
            format!("{site}/login?extra=login_error&message={}", crate::web::html_escape("Magic link is disabled"));
        return redirect(StatusCode::FOUND, &to, "application/json");
    }
    if s.is_some_and(|s| !s.user_id.is_empty()) {
        return redirect(
            StatusCode::FOUND,
            &format!("{site}/error?type=magic_link_already_logged_in"),
            "application/json",
        );
    }
    let q = query(&req);
    let token = q.get("t");
    if token.is_empty() {
        let e = AppError::bad_request("api.user.guest_magic_link.missing_token.app_error", "");
        return web_error(&app, &headers, e).await;
    }
    let redirect_to = crate::web::html_escape(q.get("redirect_to"));
    let user = match app.guest_magic_link_user(token).await {
        Ok(u) => u,
        Err(e) => {
            let to = format!("{site}/login?extra=login_error&message={}", crate::web::html_escape(&e.message));
            return redirect(StatusCode::FOUND, &to, "application/json");
        }
    };
    crate::audit::log_user(&app, None, &user.id, "attempt - guest_magic_link").await;
    let web_app_error = |e: AppError| {
        let app = app.clone();
        async move {
            let status = StatusCode::from_u16(e.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            render_web_error(&app, status, &[("message", &e.message)]).await
        }
    };
    if let Err(e) = app.check_user_all_authentication_criteria(&user).await {
        crate::audit::log_user(&app, None, &user.id, "failure - guest_magic_link").await;
        return web_app_error(e).await;
    }
    let mut h = HeaderMap::new();
    let session = match app.login_session(&headers, &mut h, &user, "", false, false, true, true).await {
        Ok(s) => s,
        Err(e) => return web_app_error(e).await,
    };
    crate::audit::log_user(&app, Some(&session), &user.id, "success - guest_magic_link").await;
    let to = if redirect_to.is_empty() {
        site.clone()
    } else {
        fully_qualified_redirect(&site, &redirect_to, &custom_schemes(&app))
    };
    let mut r = redirect(StatusCode::FOUND, &to, "application/json");
    r.headers_mut().extend(h);
    r
}

impl App {
    /// `AuthenticateUserForGuestMagicLink`: the token used up; a login link names an existing user, an invitation
    /// link creates the guest.
    async fn guest_magic_link_user(&self, token: &str) -> ApiResult<User> {
        let invalid = || AppError::bad_request("api.user.guest_magic_link.invalid_token.app_error", "");
        let mut found = None;
        for kind in ["guest_magic_link_invitation", "guest_magic_link"] {
            let r: Option<(String, i64, String, String)> = sqlx::query_as(
                "DELETE FROM Tokens WHERE Token = $1 AND Type = $2
                 RETURNING Token, COALESCE(CreateAt, 0), COALESCE(Type, ''), COALESCE(Extra, '')",
            )
            .bind(token)
            .bind(kind)
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten();
            if let Some((token, create_at, kind, extra)) = r {
                found = Some(TokenRow { token, create_at, kind, extra });
                break;
            }
        }
        let t = found.ok_or_else(invalid)?;
        let ttl = if t.kind == "guest_magic_link" { 5 * 60_000 } else { 48 * 3_600_000 };
        if now_ms() > t.create_at + ttl {
            return Err(AppError::bad_request("api.user.guest_magic_link.expired_token.app_error", ""));
        }
        let email = crate::app::map_from_json(t.extra.as_bytes()).remove("email").unwrap_or_default();
        let existing = self.user_by("u.Email = $1", &email.to_lowercase()).await;
        if t.kind == "guest_magic_link" {
            return existing.ok_or_else(invalid);
        }
        if existing.is_some() {
            return Err(invalid());
        }
        let base = clean_username(email.split('@').next().unwrap_or(""));
        let mut username = base.clone();
        let mut free = false;
        for _ in 0..10 {
            if self.user_by("u.Username = $1", &username).await.is_none() {
                free = true;
                break;
            }
            username = format!("{base}{}", &new_id()[..8]);
        }
        if !free {
            return Err(AppError::internal("api.user.guest_magic_link.username_generation_failed.app_error", ""));
        }
        let u = User { email, email_verified: true, username, auth_service: "magic_link".into(), ..Default::default() };
        // `CreateGuest`: created as a guest, checked against the guests' accepted domains.
        let guest = self.create_user_or_guest(u, true).await?;
        // The token row is gone already: the team join reads it from here.
        self.add_user_to_team_with_token(&guest, t).await?;
        Ok(guest)
    }

    /// `CheckUserAllAuthenticationCriteria` without a password: active, not locked out, verified, not a bot.
    async fn check_user_all_authentication_criteria(&self, user: &User) -> ApiResult<()> {
        let unauthorized = |id: &str| AppError::new(StatusCode::UNAUTHORIZED, id, "");
        // Go's order: disabled, bot, attempts.
        if user.delete_at > 0 {
            return Err(unauthorized("api.user.login.inactive.app_error"));
        }
        if user.is_bot {
            return Err(unauthorized("api.user.login.bot_login_forbidden.app_error"));
        }
        if user.failed_attempts as i64 >= self.cfg.i64("ServiceSettings.MaximumLoginAttempts") {
            return Err(unauthorized("api.user.check_user_login_attempts.too_many.app_error"));
        }
        if !user.email_verified && self.cfg.bool("EmailSettings.RequireEmailVerification") {
            return Err(unauthorized("api.user.login.not_verified.app_error"));
        }
        Ok(())
    }
}

/// `loginWithDesktopToken` for the desktop app after an SSO sign-in in the browser: the token's user logged in.
pub async fn login_with_desktop_token(app: &State, headers: &HeaderMap, body: &[u8]) -> ApiResult<Response> {
    let props = crate::app::map_from_json(body);
    let token = props.get("token").cloned().unwrap_or_default();
    let device_id = props.get("device_id").cloned().unwrap_or_default();
    let invalid = || AppError::new(StatusCode::UNAUTHORIZED, "app.desktop_token.validate.invalid", "");
    let found: Option<(String,)> =
        sqlx::query_as("SELECT UserId FROM DesktopTokens WHERE Token = $1 AND CreateAt >= $2")
            .bind(&token)
            .bind(now_ms() / 1000 - 3 * 60)
            .fetch_optional(&app.db)
            .await
            .unwrap_or(None);
    let Some((user_id,)) = found else {
        let _ = sqlx::query("DELETE FROM DesktopTokens WHERE Token = $1").bind(&token).execute(&app.db).await;
        return Err(invalid());
    };
    let Some(user) = app.user(&user_id).await.ok().flatten() else {
        let _ = sqlx::query("DELETE FROM DesktopTokens WHERE Token = $1").bind(&token).execute(&app.db).await;
        return Err(AppError::internal("app.desktop_token.validate.no_user", ""));
    };
    let _ = sqlx::query("DELETE FROM DesktopTokens WHERE UserId = $1").bind(&user_id).execute(&app.db).await;
    let is_oauth = matches!(user.auth_service.as_str(), "gitlab" | "google" | "office365" | "openid");
    let is_saml = user.auth_service == "saml";
    if !is_oauth && !is_saml {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "api.user.login_with_desktop_token.not_oauth_or_saml_user.app_error",
            "",
        ));
    }
    let mut h = HeaderMap::new();
    let session = app.login_session(headers, &mut h, &user, &device_id, false, is_oauth, is_saml, true).await?;
    crate::audit::log_user(app, Some(&session), &user.id, "success").await;
    // Go writes the user as it read it; its secrets never leave this server.
    let mut u = (*app.user(&user_id).await?.unwrap_or(user)).clone();
    u.password.clear();
    u.mfa_secret.clear();
    u.mfa_used_timestamps = Default::default();
    let mut r = crate::oauth_provider::go_encode(StatusCode::OK, &u);
    r.headers_mut().extend(h);
    Ok(r)
}

/// `SwitchEmailToOAuth` after the password check: the provider's authorize URL (and its cookie).
pub async fn switch_email_to_oauth(
    app: &App,
    headers: &HeaderMap,
    email: &str,
    service: &str,
) -> ApiResult<(String, Option<String>)> {
    if service == "saml" {
        let token = new_random_string(64);
        sqlx::query("INSERT INTO Tokens (Token, CreateAt, Type, Extra) VALUES ($1, $2, 'saml', $3)")
            .bind(&token)
            .bind(now_ms())
            .bind(email)
            .execute(&app.db)
            .await
            .map_err(|e| AppError::internal("app.recover.save.app_error", e.to_string()))?;
        let site = app.cfg.str("ServiceSettings.SiteURL");
        return Ok((format!("{site}/login/sso/saml?action=email_to_sso&email_token={}", url_encode(&token)), None));
    }
    let mut props = BTreeMap::new();
    props.insert("action".to_string(), ACTION_EMAIL_TO_SSO.to_string());
    props.insert("email".to_string(), email.to_string());
    let (url, cookie) = app.authorization_code_url(headers, service, props, "").await?;
    Ok((url, Some(cookie)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(clean_username("Git Lab"), "git-lab");
        assert_eq!(clean_username("Ünï"), "n");
        assert!(clean_username("admin").starts_with('a') && clean_username("admin").len() == 27);
        assert_eq!(url_encode("openid profile"), "openid%20profile");
        assert_eq!(title("gitlab"), "Gitlab");
        let mut m = BTreeMap::new();
        m.insert("redirect_to".to_string(), "/a?b=1&c=<x>".to_string());
        assert_eq!(go_map_json(&m), r#"{"redirect_to":"/a?b=1\u0026c=\u003cx\u003e"}"#);
        assert_eq!(redact_token_response(r#"{"access_token":"abc","x":1}"#), r#"{"access_token":"[REDACTED]","x":1}"#);
        let s = vec!["mmauth://".to_string()];
        assert_eq!(fully_qualified_redirect("http://h:1", "/channels/x", &s), "http://h:1/channels/x");
        assert_eq!(fully_qualified_redirect("http://h:1", "http://evil/x", &s), "http://h:1");
        assert_eq!(fully_qualified_redirect("http://h:1", "mmauth://callback", &s), "mmauth://callback");
        assert_eq!(fully_qualified_redirect("http://h:1", "", &s), "http://h:1");
        assert_eq!(
            append_query_params("http://h/login/desktop", &[("server_token", "s"), ("client_token", "c")]),
            "http://h/login/desktop?client_token=c&server_token=s"
        );
    }
}
