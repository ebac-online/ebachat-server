//! Server state, errors in Mattermost's shape, and the session behind a request.

// Based on Mattermost server/channels/app/session.go, web/handlers.go and model/utils.go (AppError),
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::FromRequestParts;
use axum::http::{Method, StatusCode, request::Parts};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use sqlx::PgPool;

use crate::cache::Caches;
use crate::config::Config;
use crate::db::{self, Table};
use crate::files::FileStore;
use crate::hub::Hub;
use crate::model::{Session, User, session};

pub type State = Arc<App>;

pub struct App {
    pub db: PgPool,
    pub cfg: Config,
    pub caches: Caches,
    pub hub: Hub,
    pub files: FileStore,
    pub offline_queue: crate::status::OfflineQueue,
}

impl App {
    pub fn new(db: PgPool, cfg: Config) -> anyhow::Result<State> {
        let caches = Caches::new(cfg.i64("ServiceSettings.SessionCacheInMinutes").max(1) as u64);
        let files = FileStore::new(&cfg)?;
        DEVELOPER.store(cfg.bool("ServiceSettings.EnableDeveloper"), std::sync::atomic::Ordering::Relaxed);
        Ok(Arc::new(App { db, cfg, caches, hub: Hub::default(), files, offline_queue: Default::default() }))
    }

    /// `App.GetSession`: the session cache, the database (with the user's team memberships), or a personal
    /// access token.
    pub async fn session(&self, token: &str) -> ApiResult<Arc<Session>> {
        let s = match self.caches.session.get(token) {
            Some(s) => s,
            None => {
                let found: Option<Session> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "{} WHERE token = $1 OR id = $1 LIMIT 1",
                    Session::SELECT
                )))
                .bind(token)
                .fetch_optional(&self.db)
                .await?;
                let mut s = match found {
                    Some(s) => s,
                    None => self.session_for_access_token(token).await?,
                };
                if s.token != token {
                    return Err(invalid_token());
                }
                s.team_members = self.session_team_members(&s.user_id).await?;
                let s = Arc::new(s);
                if !s.is_expired(now_ms()) {
                    self.caches.session.put(token.to_string(), s.clone());
                }
                s
            }
        };
        if s.id.is_empty() || s.is_expired(now_ms()) {
            return Err(invalid_token());
        }
        let idle = self.cfg.i64("ServiceSettings.SessionIdleTimeoutInMinutes");
        if idle > 0
            && !s.is_oauth
            && !s.is_mobile_app()
            && !s.is_user_access_token()
            && !self.cfg.bool("ServiceSettings.ExtendSessionLengthWithActivity")
            && now_ms() - s.last_activity_at > idle * 60_000
        {
            let _ = self.revoke_session(&s).await;
            return Err(invalid_token());
        }
        Ok(s)
    }

    /// `GetTeamsForUser` without the deleted ones, for the session.
    pub async fn session_team_members(&self, user_id: &str) -> ApiResult<Vec<crate::model::TeamMember>> {
        crate::perm::team_members(self, "tm.userid = $1 AND tm.deleteat = 0", &[user_id]).await
    }

    /// `createSessionForUserAccessToken`: a session saved for the token, as Go does.
    async fn session_for_access_token(&self, token: &str) -> ApiResult<Session> {
        let t: Option<(String, String, bool, i64)> = sqlx::query_as(
            "SELECT Id, COALESCE(UserId, ''), COALESCE(IsActive, false), ExpiresAt FROM UserAccessTokens WHERE Token = $1",
        )
        .bind(token)
        .fetch_optional(&self.db)
        .await?;
        let missing = || {
            AppError::new(
                StatusCode::UNAUTHORIZED,
                "app.user_access_token.invalid_or_missing",
                "Invalid or missing token.",
            )
        };
        let Some((token_id, user_id, active, expires)) = t else { return Err(missing()) };
        if !active {
            return Err(missing());
        }
        let user = self.user(&user_id).await?.ok_or_else(missing)?;
        if !self.cfg.bool("ServiceSettings.EnableUserAccessTokens") && !user.is_bot {
            return Err(missing());
        }
        if expires > 0 && now_ms() > expires {
            return Err(AppError::new(StatusCode::UNAUTHORIZED, "app.user_access_token.expired", "Token expired."));
        }
        if user.delete_at != 0 {
            return Err(missing());
        }
        let mut s =
            Session { token: token.into(), user_id: user.id.clone(), roles: user.roles.clone(), ..Default::default() };
        let p = &mut s.props.0;
        p.insert("user_access_token_id".into(), token_id);
        p.insert(session::PROP_TYPE.into(), session::TYPE_USER_ACCESS_TOKEN.into());
        if user.is_bot {
            p.insert(session::PROP_IS_BOT.into(), "true".into());
        }
        p.insert("is_guest".into(), user.is_guest().to_string());
        s.expires_at = now_ms() + 100 * 365 * 24 * 3_600_000;
        if expires > 0 && expires < s.expires_at {
            s.expires_at = expires;
        }
        self.save_session(&mut s).await?;
        Ok(s)
    }

    /// `SqlSessionStore.Save`.
    pub async fn save_session(&self, s: &mut Session) -> ApiResult<()> {
        s.id = new_id();
        s.create_at = now_ms();
        if s.last_activity_at == 0 {
            s.last_activity_at = s.create_at;
        }
        db::insert(&self.db, s).await?;
        Ok(())
    }

    /// `RevokeSession`: deleted, out of the cache, and its websockets closed.
    pub async fn revoke_session(&self, s: &Session) -> ApiResult<()> {
        sqlx::query("DELETE FROM Sessions WHERE Id = $1").bind(&s.id).execute(&self.db).await?;
        self.caches.session.remove(&s.token);
        self.hub.close_session(&s.token);
        Ok(())
    }

    /// `ClearSessionCacheForUser`: the user's sessions are read again (roles, team memberships changed).
    /// Its websockets load their session again before their next event (`invalidateWebConnSessionCacheForUser`).
    pub fn clear_sessions_for_user(&self, user_id: &str) {
        self.caches.session.remove_where(|_, s| s.user_id == user_id);
        self.hub.invalidate_user(user_id);
    }

    /// `ExtendSessionExpiryIfNeeded`.
    pub async fn extend_session_if_needed(&self, s: &Session) {
        if !self.cfg.bool("ServiceSettings.ExtendSessionLengthWithActivity") || s.is_expired(now_ms()) {
            return;
        }
        let length = self.session_length(s);
        let threshold = ((length as f64 * 0.01).min(86_400_000.0) as i64).max(5 * 60_000);
        let now = now_ms();
        if now - (s.expires_at - length) < threshold {
            return;
        }
        let expiry = now + length;
        // Only a session still in the database goes back into the cache: never a revoked one.
        if sqlx::query("UPDATE Sessions SET ExpiresAt = $1 WHERE Id = $2")
            .bind(expiry)
            .bind(&s.id)
            .execute(&self.db)
            .await
            .is_ok_and(|r| r.rows_affected() == 1)
        {
            let mut n = s.clone();
            n.expires_at = expiry;
            self.caches.session.put(n.token.clone(), Arc::new(n));
        }
    }

    /// `GetSessionLengthInMillis`.
    pub fn session_length(&self, s: &Session) -> i64 {
        if s.is_user_access_token() && s.expires_at > 0 {
            return (s.expires_at - now_ms()).max(0);
        }
        let hours = if s.is_mobile_app() {
            self.cfg.i64("ServiceSettings.SessionLengthMobileInHours")
        } else if s.is_sso_login() {
            self.cfg.i64("ServiceSettings.SessionLengthSSOInHours")
        } else {
            self.cfg.i64("ServiceSettings.SessionLengthWebInHours")
        };
        hours * 3_600_000
    }

    /// `UpdateLastActivityAtIfNeeded` (websocket activity).
    pub async fn touch_session(&self, s: &Session) {
        let now = now_ms();
        if now - s.last_activity_at < 5 * 60_000 {
            return;
        }
        // Only a session still in the database goes back into the cache: never a revoked one.
        let updated = sqlx::query("UPDATE Sessions SET LastActivityAt = $1 WHERE Id = $2")
            .bind(now)
            .bind(&s.id)
            .execute(&self.db)
            .await
            .is_ok_and(|r| r.rows_affected() == 1);
        if updated {
            let mut n = s.clone();
            n.last_activity_at = now;
            self.caches.session.put(n.token.clone(), Arc::new(n));
        }
    }

    /// `UserStore.Get` through the profile cache, with the bot fields.
    pub async fn user(&self, id: &str) -> ApiResult<Option<Arc<User>>> {
        Ok(self.users(&[id.to_string()]).await?.into_iter().next())
    }

    /// `UserStore.GetProfileByIds` / `GetMany` through the profile cache: those found, in no order.
    pub async fn users(&self, ids: &[String]) -> ApiResult<Vec<Arc<User>>> {
        let mut out = Vec::with_capacity(ids.len());
        let mut missing: Vec<String> = vec![];
        for id in ids {
            match self.caches.user.get(id) {
                Some(u) => out.push(u),
                None if !missing.contains(id) => missing.push(id.clone()),
                None => {}
            }
        }
        if !missing.is_empty() {
            for u in crate::users::load_users(self, "u.id = ANY($1)", &missing).await? {
                let u = Arc::new(u);
                self.caches.user.put(u.id.clone(), u.clone());
                out.push(u);
            }
        }
        Ok(out)
    }

    /// `ChannelStore.Get` through the channel cache: message channels only, as the store's `messageChannelTypes`
    /// filter; boards and space backing channels are not found.
    pub async fn channel(&self, id: &str) -> ApiResult<Option<Arc<crate::model::Channel>>> {
        Ok(self.channel_any_type(id).await?.filter(|c| crate::model::channel::is_message_type(&c.r#type)))
    }

    /// api4 `rejectBoardChannelByID`: 400 when the id is a board channel's. Only a board found rejects; a
    /// failed lookup lets the request through, as Go's.
    pub async fn reject_board_channel(&self, id: &str) -> ApiResult<()> {
        match self.channel_any_type(id).await {
            Ok(Some(c)) if c.is_board() => Err(board_channel_error()),
            _ => Ok(()),
        }
    }

    /// api4 `rejectSpaceChannelByID`: 400 when the id is a space's backing channel. It fails closed: a lookup
    /// error other than not-found is returned, as `GetChannelOfType`'s 500. Go reads the primary so a fresh
    /// space cannot slip through on replica lag; there is one database here, and a cached channel's type never
    /// becomes a space's, so the cache answers as well.
    pub async fn reject_space_channel(&self, id: &str) -> ApiResult<()> {
        match self.channel_any_type(id).await {
            Ok(Some(c)) if c.is_space() => Err(space_channel_error()),
            Ok(_) => Ok(()),
            Err(e) => {
                Err(AppError::internal("app.channel.get.find.app_error", e.detailed_error).with_param("channel_id", id))
            }
        }
    }

    /// `rejectBoardChannelByID` then `rejectSpaceChannelByID`, Go's order on most guarded endpoints.
    pub async fn reject_board_or_space_channel(&self, id: &str) -> ApiResult<()> {
        self.reject_board_channel(id).await?;
        self.reject_space_channel(id).await
    }

    /// A channel of any type through the channel cache: the row without `Get`'s type filter, as Go's
    /// `GetChannelOfType` and `GetBoardChannel` read it. The cache may hold boards and spaces;
    /// [`App::channel`] filters them out.
    pub async fn channel_any_type(&self, id: &str) -> ApiResult<Option<Arc<crate::model::Channel>>> {
        if let Some(c) = self.caches.channel.get(id) {
            return Ok(Some(c));
        }
        let c: Option<crate::model::Channel> = db::get(&self.db, id).await?;
        Ok(c.map(|c| {
            let c = Arc::new(c);
            self.caches.channel.put(id.to_string(), c.clone());
            c
        }))
    }

    /// The channel or 404 as `GetChannel` answers.
    pub async fn channel_or_404(&self, id: &str) -> ApiResult<Arc<crate::model::Channel>> {
        self.channel(id).await?.ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                "app.channel.get.existing.app_error",
                "Unable to find the existing channel.",
            )
        })
    }
}

/// api4 `rejectBoardChannelByID`'s error.
pub fn board_channel_error() -> AppError {
    AppError::bad_request("api.channel.board_channel.app_error", "")
        .detail("board channels cannot be accessed via /channels endpoints")
}

/// api4 `rejectSpaceChannelByID`'s error.
pub fn space_channel_error() -> AppError {
    AppError::bad_request("api.channel.space_channel.app_error", "")
        .detail("space channels cannot be accessed via /channels endpoints")
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// `model.NewId`: 16 random bytes in base32 with Mattermost's alphabet, 26 characters.
pub fn new_id() -> String {
    const ALPHABET: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";
    let bytes: [u8; 16] = rand::random();
    let mut out = String::with_capacity(26);
    let (mut acc, mut bits) = (0u32, 0);
    for b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
    }
    out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    out
}

/// `model.IsValidId`.
pub fn is_valid_id(s: &str) -> bool {
    s.len() == 26 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

// ---------- errors ----------

/// `model.AppError` as the API sends it.
#[derive(Debug, Serialize)]
pub struct AppError {
    pub id: String,
    pub message: String,
    pub detailed_error: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub request_id: String,
    pub status_code: u16,
    /// Context for the client (a plugin install conflict's versions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub props: Option<Box<std::collections::BTreeMap<String, String>>>,
    /// `Context.RemoveSessionCookie` before the error is written: the `Set-Cookie` header that clears the session.
    #[serde(skip)]
    pub set_cookie: Option<Box<String>>,
}

impl AppError {
    pub fn new(status: StatusCode, id: &str, message: &str) -> AppError {
        AppError {
            id: id.into(),
            message: crate::i18n::t(id).map(Into::into).unwrap_or_else(|| crate::brand::apply(message).into_owned()),
            detailed_error: String::new(),
            request_id: new_id(),
            status_code: status.as_u16(),
            props: None,
            set_cookie: None,
        }
    }
    /// The error with `Context.RemoveSessionCookie`'s header: `MMAUTHTOKEN` cleared under the site's subpath.
    pub fn clearing_session_cookie(mut self, app: &App) -> AppError {
        self.set_cookie = Some(remove_session_cookie(app).into());
        self
    }
    pub fn detail(mut self, d: impl Into<String>) -> AppError {
        self.detailed_error = d.into();
        self
    }
    /// The message with Go's `{{.Name}}` parameters filled in.
    pub fn with_params(mut self, args: &[(&str, &str)]) -> AppError {
        if let Some(m) = crate::i18n::tr(&self.id, args) {
            self.message = m;
        }
        self
    }
    pub fn with_param(self, k: &str, v: &str) -> AppError {
        self.with_params(&[(k, v)])
    }
    /// The same error under another id (and its message).
    pub fn with_id(mut self, id: &str) -> AppError {
        self.id = id.into();
        if let Some(m) = crate::i18n::t(id) {
            self.message = m.into();
        }
        self
    }
    /// `SetInvalidParam`.
    pub fn invalid_param(name: &str) -> AppError {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "api.context.invalid_body_param.app_error",
            &format!("Invalid or missing {name} in request body."),
        )
    }
    /// `SetInvalidURLParam`.
    pub fn invalid_url_param(name: &str) -> AppError {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "api.context.invalid_url_param.app_error",
            &format!("Invalid or missing {name} parameter in request URL."),
        )
    }
    pub fn forbidden() -> AppError {
        AppError::new(
            StatusCode::FORBIDDEN,
            "api.context.permissions.app_error",
            "You do not have the appropriate permissions.",
        )
    }
    /// `SetPermissionError`.
    pub fn forbidden_perm(p: &str) -> AppError {
        AppError::forbidden().detail(format!("userId=, permission={p}"))
    }
    pub fn not_found(id: &str, message: &str) -> AppError {
        AppError::new(StatusCode::NOT_FOUND, id, message)
    }
    pub fn bad_request(id: &str, message: &str) -> AppError {
        AppError::new(StatusCode::BAD_REQUEST, id, message)
    }
    pub fn internal(id: &str, detail: impl Into<String>) -> AppError {
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, id, "An internal error occurred.").detail(detail)
    }
    pub fn not_implemented(what: &str) -> AppError {
        AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "api.not_implemented.app_error",
            &format!("{what} is not implemented."),
        )
    }
}

pub fn invalid_token() -> AppError {
    AppError::new(
        StatusCode::UNAUTHORIZED,
        "api.context.session_expired.app_error",
        "Invalid or expired session, please login again.",
    )
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> AppError {
        AppError::internal("app.sql.error", e.to_string())
    }
}

/// `ServiceSettings.EnableDeveloper`: errors keep `detailed_error` only then, as Go's `WipeDetailed`.
pub static DEVELOPER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl IntoResponse for AppError {
    fn into_response(mut self) -> Response {
        // `LogErrorByCode`: 4xx at debug, 501 at info, the rest at error.
        let mut fields = crate::logs::request_fields(&self.request_id);
        fields.extend([
            ("err_where", serde_json::Value::from("")),
            ("http_code", self.status_code.into()),
            ("error", format!("{}: {}, {}", self.id, self.message, self.detailed_error).into()),
        ]);
        let level = match self.status_code {
            400..=499 => crate::logs::Level::Debug,
            501 => crate::logs::Level::Info,
            _ => crate::logs::Level::Error,
        };
        crate::logs::log(level, &self.message, &fields);
        // Go blocks out the detail when not in developer mode.
        if !DEVELOPER.load(std::sync::atomic::Ordering::Relaxed) {
            self.detailed_error.clear();
        }
        let status = StatusCode::from_u16(self.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let rid = self.request_id.clone();
        let cookie = self.set_cookie.take();
        let mut r = (status, axum::Json(self)).into_response();
        if let Ok(v) = rid.parse() {
            r.headers_mut().insert("X-Request-Id", v);
        }
        if let Some(v) = cookie.and_then(|c| axum::http::HeaderValue::from_str(&c).ok()) {
            r.headers_mut().append(axum::http::header::SET_COOKIE, v);
        }
        r
    }
}

pub type ApiResult<T> = Result<T, AppError>;

// ---------- the session of a request ----------

#[derive(PartialEq, Clone, Copy)]
pub enum TokenLocation {
    Header,
    Cookie,
    Query,
}

/// `Context.RemoveSessionCookie`: `MMAUTHTOKEN` cleared (`MaxAge: -1`, `HttpOnly`) under the site's subpath.
pub fn remove_session_cookie(app: &App) -> String {
    let path = crate::web::subpath(app.cfg.str("ServiceSettings.SiteURL"));
    format!("MMAUTHTOKEN=; Path={path}; Max-Age=0; HttpOnly")
}

/// textproto.TrimString: without ASCII spaces, tabs, CRs and LFs around.
fn trim_ascii_ws(s: &[u8]) -> &[u8] {
    let ws = |b: &u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n');
    let start = s.iter().position(|b| !ws(b)).unwrap_or(s.len());
    let end = s.iter().rposition(|b| !ws(b)).map_or(start, |i| i + 1);
    &s[start..end]
}

/// `Request.Cookie(name)` (`readCookies`): the first well-formed cookie of that name in the `Cookie` headers, its
/// value without surrounding double quotes. A cookie whose name or value has a byte Go refuses is skipped.
pub fn go_cookie(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    // isTokenRune: visible ASCII that is not a separator.
    let token_byte = |b: u8| b > 0x20 && b < 0x7f && !br#"()<>@,;:\"/[]?={}"#.contains(&b);
    let value_byte = |b: u8| (0x20..0x7f).contains(&b) && b != b'"' && b != b';' && b != b'\\';
    for line in headers.get_all(axum::http::header::COOKIE) {
        for part in trim_ascii_ws(line.as_bytes()).split(|b| *b == b';') {
            let part = trim_ascii_ws(part);
            if part.is_empty() {
                continue;
            }
            let (n, val) = match part.iter().position(|b| *b == b'=') {
                Some(i) => (&part[..i], &part[i + 1..]),
                None => (part, &b""[..]),
            };
            let n = trim_ascii_ws(n);
            if n.is_empty() || !n.iter().all(|b| token_byte(*b)) || n != name.as_bytes() {
                continue;
            }
            let v = if val.len() > 1 && val[0] == b'"' && val[val.len() - 1] == b'"' {
                &val[1..val.len() - 1]
            } else {
                val
            };
            if v.iter().all(|b| value_byte(*b)) {
                return Some(String::from_utf8_lossy(v).into_owned());
            }
        }
    }
    None
}

/// `ParseAuthTokenFromRequest`: the `MMAUTHTOKEN` cookie first (even an empty one), then `Authorization: Bearer
/// <t>` or `Token <t>`, then `?access_token=` as `URL.Query` reads it; cut to 50 bytes. An empty token is none: Go
/// only looks a token up when it is not empty.
pub fn token_from(parts: &Parts) -> Option<(String, TokenLocation)> {
    let (t, loc) = parse_auth_token(parts)?;
    if t.is_empty() { None } else { Some((t, loc)) }
}

fn parse_auth_token(parts: &Parts) -> Option<(String, TokenLocation)> {
    let cut = |b: &[u8]| String::from_utf8_lossy(&b[..b.len().min(50)]).into_owned();
    if let Some(v) = go_cookie(&parts.headers, "MMAUTHTOKEN") {
        return Some((cut(v.as_bytes()), TokenLocation::Cookie));
    }
    let auth = parts.headers.get(axum::http::header::AUTHORIZATION).map(|v| v.as_bytes()).unwrap_or(b"");
    if auth.len() > 6 && auth[..6].eq_ignore_ascii_case(b"bearer") {
        return Some((cut(&auth[7..]), TokenLocation::Header));
    }
    if auth.len() > 5 && auth[..5].eq_ignore_ascii_case(b"token") {
        return Some((cut(&auth[6..]), TokenLocation::Header));
    }
    let q = crate::gourl::parse_query(parts.uri.query().unwrap_or("")).0;
    let t = q.get("access_token");
    if !t.is_empty() {
        return Some((cut(t.as_bytes()), TokenLocation::Query));
    }
    None
}

/// A request with a valid session (`APISessionRequired`), CSRF-checked for cookie sessions.
pub struct Auth(pub Arc<Session>);

impl FromRequestParts<State> for Auth {
    type Rejection = AppError;
    async fn from_request_parts(parts: &mut Parts, app: &State) -> Result<Auth, AppError> {
        session_required(parts, app, false).await.map(Auth)
    }
}

/// `APISessionRequiredTrustRequester`: a valid session without the CSRF check (`client_perf` beacons,
/// `roles/names`, file downloads by HEAD).
pub struct AuthTrusted(pub Arc<Session>);

impl FromRequestParts<State> for AuthTrusted {
    type Rejection = AppError;
    async fn from_request_parts(parts: &mut Parts, app: &State) -> Result<AuthTrusted, AppError> {
        session_required(parts, app, true).await.map(AuthTrusted)
    }
}

/// What `web.Handler.ServeHTTP` makes of the request's token before the handler runs: the session; none (no token,
/// or a token Go cannot find where no session is required); or the error the handler is not run for:
///
/// - a session lookup failing with a 500;
/// - an unknown or expired token where a session is required: "session expired", the session cookie removed;
/// - a token in the query string that is not an OAuth session's;
/// - a cookie session failing `checkCSRFToken` on anything but GET, unless the handler trusts the requester (the
///   `X-CSRF-Token` header must equal the session's CSRF token, both possibly empty, or `X-Requested-With` be
///   `XMLHttpRequest` without `ExperimentalStrictCSRFEnforcement`): "session expired", the session cookie removed.
pub async fn request_session(
    parts: &Parts,
    app: &State,
    require: bool,
    trust_requester: bool,
) -> Result<Option<Arc<Session>>, AppError> {
    let Some((token, loc)) = token_from(parts) else { return Ok(None) };
    let s = match app.session(&token).await {
        Ok(s) => s,
        Err(e) if e.status_code == 500 => return Err(e),
        Err(_) if require => return Err(invalid_token().clearing_session_cookie(app)),
        Err(_) => return Ok(None),
    };
    if loc == TokenLocation::Query && !s.is_oauth {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "api.context.token_provided.app_error",
            "Session is not OAuth but token was provided in the query string",
        ));
    }
    if loc == TokenLocation::Cookie && !trust_requester && parts.method != Method::GET {
        // `Header.Get`: the first value.
        let first = |n: &str| parts.headers.get(n).map(|v| v.as_bytes()).unwrap_or(b"");
        let passed = first("x-csrf-token") == s.prop(session::PROP_CSRF).as_bytes()
            || (first("x-requested-with") == b"XMLHttpRequest"
                && !app.cfg.bool("ServiceSettings.ExperimentalStrictCSRFEnforcement"));
        if !passed {
            return Err(invalid_token().clearing_session_cookie(app));
        }
    }
    Ok(Some(s))
}

async fn session_required(parts: &Parts, app: &State, trust_requester: bool) -> Result<Arc<Session>, AppError> {
    let s = request_session(parts, app, true, trust_requester).await?.ok_or_else(invalid_token)?;
    // `Context.SessionRequired`.
    if (!app.cfg.bool("ServiceSettings.EnableUserAccessTokens")
        && s.is_user_access_token()
        && s.prop(session::PROP_IS_BOT) != "true")
        || s.user_id.is_empty()
    {
        return Err(invalid_token());
    }
    crate::audit::set_user(&s.user_id);
    Ok(s)
}

/// `APISessionRequiredDisableWhenBusy`: `Auth`, then Go's server-busy error (`SetServerBusyError`) while the
/// server is marked busy, before the handler runs.
pub struct BusyAuth(pub Arc<Session>);

impl FromRequestParts<State> for BusyAuth {
    type Rejection = AppError;
    async fn from_request_parts(parts: &mut Parts, app: &State) -> Result<BusyAuth, AppError> {
        let Auth(s) = Auth::from_request_parts(parts, app).await?;
        if crate::admin::is_busy() {
            return Err(AppError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "api.context.server_busy.app_error",
                "Server is busy, non-critical services are temporarily unavailable.",
            ));
        }
        Ok(BusyAuth(s))
    }
}

/// A handler's query parameters as Go reads them (`r.URL.Query()`, then `Values.Get`): `url.ParseQuery` drops
/// every pair with a raw `;` or a bad escape, and a key given more than once has its first value.
pub struct Query<T>(pub T);

impl<T: serde::de::DeserializeOwned, S: Send + Sync> FromRequestParts<S> for Query<T> {
    type Rejection = axum::extract::rejection::QueryRejection;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Query<T>, Self::Rejection> {
        axum::extract::Query::<T>::try_from_uri(&go_query_uri(parts.uri.query().unwrap_or(""))).map(|q| Query(q.0))
    }
}

impl<T> std::ops::Deref for Query<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// `/?` and the query `url.ParseQuery` keeps, one (the first) value per key.
fn go_query_uri(raw: &str) -> axum::http::Uri {
    let esc = crate::gourl::query_escape;
    let kept: Vec<String> = crate::gourl::parse_query(raw)
        .0
        .0
        .iter()
        .filter_map(|(k, vs)| vs.first().map(|v| format!("{}={}", esc(k), esc(v))))
        .collect();
    format!("/?{}", kept.join("&")).parse().unwrap_or_else(|_| axum::http::Uri::from_static("/"))
}

/// The session if there is a valid one (`APIHandler` endpoints): no token, or a token Go cannot find, is no
/// session; a query-string token that is not an OAuth session's, a failed CSRF check or a 500 from the session
/// lookup stop the request as in Go (JSON under the API, the web error page elsewhere).
pub struct MaybeAuth(pub Option<Arc<Session>>);

impl FromRequestParts<State> for MaybeAuth {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, app: &State) -> Result<MaybeAuth, Response> {
        match request_session(parts, app, false, false).await {
            Ok(s) => {
                if let Some(s) = &s {
                    crate::audit::set_user(&s.user_id);
                }
                Ok(MaybeAuth(s))
            }
            Err(e) => Err(error_response(app, parts, e).await),
        }
    }
}

/// `handleContextError`'s answer for an error before the handler: JSON under the API, for webhooks, the OAuth API
/// and the mobile app; the web error page otherwise.
pub async fn error_response(app: &App, parts: &Parts, mut e: AppError) -> Response {
    let sub = crate::web::subpath(app.cfg.str("ServiceSettings.SiteURL"));
    let path = parts.uri.path();
    let rel = path.strip_prefix(sub.trim_end_matches('/')).unwrap_or(path);
    let oauth_api = (parts.method == Method::POST && rel == "/oauth/authorize")
        || matches!(rel, "/oauth/apps/authorized" | "/oauth/deauthorize" | "/oauth/access_token" | "/oauth/intune");
    if rel.starts_with("/api/") || rel.starts_with("/hooks/") || oauth_api {
        return e.into_response();
    }
    let cookie = e.set_cookie.take();
    let mut r = crate::web::web_error(app, &parts.headers, e).await;
    if let Some(v) = cookie.and_then(|c| axum::http::HeaderValue::from_str(&c).ok()) {
        r.headers_mut().append(axum::http::header::SET_COOKIE, v);
    }
    r
}

impl Session {
    /// `me` in a path, or the id itself.
    pub fn resolve(&self, id: &str) -> String {
        if id == "me" { self.user_id.clone() } else { id.to_string() }
    }
}

/// A JSON request body read as Go reads it: whatever the `Content-Type`, a bad body is a 400 for `name`.
/// A list of strings is read as `model.SortedArrayFromJSON`, which every such Go handler uses: `null` is an empty
/// list, anything else unreadable (an empty body too) is `api.payload.parse.error`.
pub struct Body<T>(pub T);

impl<T: serde::de::DeserializeOwned> axum::extract::FromRequest<State> for Body<T> {
    type Rejection = AppError;
    async fn from_request(req: axum::extract::Request, state: &State) -> Result<Body<T>, AppError> {
        let bytes = axum::body::Bytes::from_request(req, state).await.map_err(|_| AppError::invalid_param("body"))?;
        let blank = bytes.iter().all(u8::is_ascii_whitespace);
        let b: &[u8] = if blank { b"null" } else { &bytes };
        serde_json::from_slice(b).map(Body).or_else(|e| {
            if serde_json::from_slice::<T>(br#"["a"]"#).is_err() {
                return Err(AppError::invalid_param("body").detail(e.to_string()));
            }
            if !blank && serde_json::from_slice::<serde_json::Value>(b).is_ok_and(|v| v.is_null()) {
                return serde_json::from_slice(b"[]")
                    .map(Body)
                    .map_err(|e| AppError::invalid_param("body").detail(e.to_string()));
            }
            Err(AppError::bad_request("api.payload.parse.error", "Error parsing the payload.").detail(e.to_string()))
        })
    }
}

/// A body decoded as Go's `json.NewDecoder(r.Body).Decode(&v)` into a struct: the first JSON value only, `null`
/// leaves the zero value, an empty or bad body is `SetInvalidParamWithErr(name)`.
pub fn decode_json<T: serde::de::DeserializeOwned + Default>(b: &[u8], name: &str) -> ApiResult<T> {
    let v = serde_json::Deserializer::from_slice(b)
        .into_iter::<serde_json::Value>()
        .next()
        .ok_or_else(|| AppError::invalid_param(name).detail("EOF"))?
        .map_err(|e| AppError::invalid_param(name).detail(e.to_string()))?;
    if v.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(v).map_err(|e| AppError::invalid_param(name).detail(e.to_string()))
}

/// `model.MapFromJSON`: a JSON object of strings, anything else an empty map.
pub fn map_from_json(b: &[u8]) -> std::collections::HashMap<String, String> {
    serde_json::Deserializer::from_slice(b)
        .into_iter::<std::collections::HashMap<String, String>>()
        .next()
        .and_then(Result::ok)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Channel;
    use crate::model::channel::{OPEN, OPEN_BOARD, PRIVATE_BOARD, SPACE};

    /// An app whose database cannot be reached: lookups that miss the channel cache fail.
    fn app() -> State {
        let dir = std::env::temp_dir().join("ebachat-board-guard-test");
        let cfg = Config::from_value(serde_json::json!({"FileSettings": {"Directory": dir.to_string_lossy()}}));
        let db = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(200))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();
        App::new(db, cfg).unwrap()
    }

    fn cache(app: &App, kind: &str) -> String {
        let id = new_id();
        app.caches
            .channel
            .put(id.clone(), Arc::new(Channel { id: id.clone(), r#type: kind.into(), ..Default::default() }));
        id
    }

    #[tokio::test]
    async fn board_and_space_guards() {
        let app = app();
        for kind in [OPEN_BOARD, PRIVATE_BOARD] {
            let id = cache(&app, kind);
            let e = app.reject_board_channel(&id).await.unwrap_err();
            assert_eq!((e.status_code, e.id.as_str()), (400, "api.channel.board_channel.app_error"));
            assert_eq!(e.detailed_error, "board channels cannot be accessed via /channels endpoints");
            assert!(e.message.starts_with("Board channels cannot be accessed"), "{}", e.message);
            assert!(app.reject_space_channel(&id).await.is_ok());
            assert_eq!(app.reject_board_or_space_channel(&id).await.unwrap_err().id, e.id);
            assert!(app.channel(&id).await.unwrap().is_none(), "Get leaves boards out");
            assert!(app.channel_any_type(&id).await.unwrap().is_some());
        }

        let id = cache(&app, SPACE);
        assert!(app.reject_board_channel(&id).await.is_ok());
        let e = app.reject_space_channel(&id).await.unwrap_err();
        assert_eq!((e.status_code, e.id.as_str()), (400, "api.channel.space_channel.app_error"));
        assert_eq!(e.detailed_error, "space channels cannot be accessed via /channels endpoints");
        assert_eq!(app.reject_board_or_space_channel(&id).await.unwrap_err().id, e.id);
        assert!(app.channel(&id).await.unwrap().is_none(), "Get leaves spaces out");

        let id = cache(&app, OPEN);
        assert!(app.reject_board_or_space_channel(&id).await.is_ok());
        assert!(app.channel(&id).await.unwrap().is_some());

        // A failed lookup: the board guard lets the request through, the space guard fails closed.
        let id = new_id();
        assert!(app.reject_board_channel(&id).await.is_ok());
        let e = app.reject_space_channel(&id).await.unwrap_err();
        assert_eq!((e.status_code, e.id.as_str()), (500, "app.channel.get.find.app_error"));
        assert!(e.message.contains(&id), "{}", e.message);
        assert_eq!(app.reject_board_or_space_channel(&id).await.unwrap_err().id, e.id);
    }

    #[test]
    fn channel_types() {
        let c = |t: &str| Channel { r#type: t.into(), ..Default::default() };
        assert!(c(OPEN_BOARD).is_board() && c(PRIVATE_BOARD).is_board() && !c(SPACE).is_board() && !c(OPEN).is_board());
        assert!(c(SPACE).is_space() && !c(OPEN_BOARD).is_space());
        for t in ["O", "P", "D", "G"] {
            assert!(crate::model::channel::is_message_type(t));
        }
        for t in [SPACE, OPEN_BOARD, PRIVATE_BOARD, ""] {
            assert!(!crate::model::channel::is_message_type(t));
        }
    }
}
