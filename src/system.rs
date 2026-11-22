//! System endpoints: ping, the client config and license, roles by name, and what a server without plugins or a
//! license answers (plugins, trial license, notices, onboarding, the image proxy, push acks).

// Based on Mattermost server/channels/api4/system.go, config.go, license.go, role.go, plugin.go, image.go and
// app/platform/config.go (ClientConfigWithComputed), Copyright (c) 2015-present Mattermost, Inc.
// Licensed under AGPL-3.0.

use std::collections::BTreeMap;

use crate::app::Query;
use axum::extract::State as St;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, MaybeAuth, State};
use crate::model::Role;
use crate::perm;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/system/ping", get(ping))
        .route("/api/v4/config/client", get(client_config))
        .route("/api/v4/license/load_metric", get(load_metric))
        .route("/api/v4/roles/names", post(roles_by_names))
        .route("/api/v4/brand/image", get(|| async { StatusCode::NOT_FOUND }))
        .route("/api/v4/system/onboarding/complete", get(onboarding))
        .route("/api/v4/notifications/ack", post(push_ack))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PingQuery {
    get_server_status: String,
    use_rest_semantics: String,
    device_id: String,
}

/// `getSystemPing`.
async fn ping(St(app): St<State>, MaybeAuth(s): MaybeAuth, Query(q): Query<PingQuery>) -> Response {
    let mut m = BTreeMap::new();
    let ok = Value::from("OK");
    for k in ["AndroidLatestVersion", "AndroidMinVersion", "IosLatestVersion", "IosMinVersion"] {
        m.insert(k.to_string(), Value::from(app.cfg.str(&format!("ClientRequirements.{k}"))));
    }
    m.insert("ActiveSearchBackend".into(), "database".into());
    m.insert("status".into(), ok.clone());
    let mut healthy = true;
    if q.get_server_status == "true" {
        let db = sqlx::query("SELECT 1").execute(&app.db).await.is_ok();
        let files = app.files.write("testfile", bytes::Bytes::from_static(b"testingwrite")).await.is_ok()
            && app.files.remove("testfile").await.is_ok();
        m.insert("database_status".into(), if db { ok.clone() } else { "UNHEALTHY".into() });
        m.insert("filestore_status".into(), if files { ok.clone() } else { "UNHEALTHY".into() });
        healthy = db && files;
        if !healthy {
            m.insert("status".into(), "UNHEALTHY".into());
        }
        if let Some(s) = &s
            && app.has_perm(s, perm::MANAGE_SYSTEM).await
        {
            m.insert("root_status".into(), false.into());
        }
    }
    if !q.device_id.is_empty() {
        m.insert("CanReceiveNotifications".into(), crate::push::send_test(&app, &q.device_id).await.into());
    }
    let code =
        if healthy || q.use_rest_semantics == "true" { StatusCode::OK } else { StatusCode::INTERNAL_SERVER_ERROR };
    (code, Json(m)).into_response()
}

impl App {
    /// `ClientConfigWithComputed` / `LimitedClientConfigWithComputed`.
    pub async fn client_config(&self, limited: bool) -> ApiResult<BTreeMap<String, String>> {
        let mut c = self.cfg.client(limited);
        let (users,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM Users").fetch_one(&self.db).await?;
        c.insert("NoAccounts".into(), (users == 0).to_string());
        if let Some(k) = self.signing_public_key().await {
            c.insert("AsymmetricSigningPublicKey".into(), k);
        }
        if c.get("EnableCustomTermsOfService").is_some_and(|v| v == "true")
            && let Some(id) = self.latest_terms_of_service_id().await?
        {
            c.insert("CustomTermsOfServiceId".into(), id);
        }
        if limited {
            return Ok(c);
        }
        c.insert("MaxPostSize".into(), self.max_post_size().await.to_string());
        c.insert("UpgradedFromTE".into(), "false".into());
        let installed: Option<(String,)> = sqlx::query_as("SELECT Value FROM Systems WHERE Name = 'InstallationDate'")
            .fetch_optional(&self.db)
            .await?;
        c.insert("InstallationDate".into(), installed.map(|v| v.0).unwrap_or_default());
        let schema: Option<(i64,)> = sqlx::query_as("SELECT MAX(Version)::bigint FROM db_migrations")
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten();
        if let Some((v,)) = schema {
            c.insert("SchemaVersion".into(), v.to_string());
        }
        Ok(c)
    }

    /// `TermsOfServiceStore.GetLatest` through its cache.
    pub async fn latest_terms_of_service_id(&self) -> ApiResult<Option<String>> {
        if let Some(v) = self.caches.terms_of_service.get("latest") {
            return Ok(v["id"].as_str().map(str::to_string));
        }
        let t: Option<(String, i64, String, String)> = sqlx::query_as(
            "SELECT Id, COALESCE(CreateAt, 0), COALESCE(UserId, ''), COALESCE(Text, '') FROM TermsOfService
             ORDER BY CreateAt DESC LIMIT 1",
        )
        .fetch_optional(&self.db)
        .await?;
        Ok(t.map(|(id, create_at, user_id, text)| {
            let v = json!({"id": id, "create_at": create_at, "user_id": user_id, "text": text});
            self.caches.terms_of_service.put("latest".into(), std::sync::Arc::new(v));
            id
        }))
    }
}

impl App {
    /// `GetMaxPostSize`: Posts.Message's size at 4 bytes a character, at least `PostMessageMaxRunesV2`; read once.
    pub async fn max_post_size(&self) -> i64 {
        static SIZE: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
        if let Some(n) = SIZE.get() {
            return *n;
        }
        let r: Option<(Option<i32>,)> = sqlx::query_as(
            "SELECT character_maximum_length FROM information_schema.columns
             WHERE table_name = 'posts' AND column_name = 'message'",
        )
        .fetch_optional(&self.db)
        .await
        .ok()
        .flatten();
        let bytes = r.and_then(|r| r.0).unwrap_or(0) as i64;
        *SIZE.get_or_init(|| (bytes / 4).max(crate::posts::MAX_POST_SIZE as i64))
    }

    /// The public half of the `AsymmetricSigningKey` (P-256), as DER SubjectPublicKeyInfo in base64.
    async fn signing_public_key(&self) -> Option<String> {
        let (v,): (String,) = sqlx::query_as("SELECT Value FROM Systems WHERE Name = 'AsymmetricSigningKey'")
            .fetch_optional(&self.db)
            .await
            .ok()??;
        let num = |k: &str| -> Option<[u8; 32]> {
            let i = v.find(&format!("\"{k}\":"))? + k.len() + 3;
            let digits: String = v[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
            let mut out = [0u8; 32];
            for d in digits.bytes() {
                let mut carry = (d - b'0') as u32;
                for b in out.iter_mut().rev() {
                    let x = *b as u32 * 10 + carry;
                    *b = x as u8;
                    carry = x >> 8;
                }
                if carry != 0 {
                    return None;
                }
            }
            Some(out)
        };
        let (x, y) = (num("x")?, num("y")?);
        let mut der = vec![
            0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48,
            0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04,
        ];
        der.extend_from_slice(&x);
        der.extend_from_slice(&y);
        use base64::Engine;
        Some(base64::engine::general_purpose::STANDARD.encode(der))
    }
}

/// `getClientConfig`: the limited config without a session (any `format`; 12.1 no longer checks it).
async fn client_config(St(app): St<State>, MaybeAuth(s): MaybeAuth) -> ApiResult<Response> {
    Ok(Json(app.client_config(s.is_none()).await?).into_response())
}

async fn load_metric(_: Auth) -> Json<Value> {
    Json(json!({"load": 0}))
}

async fn onboarding(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    crate::audit::log(&app, Some(&s), "attempt").await;
    app.require(&s, perm::MANAGE_SYSTEM).await?;
    let v: Option<(String,)> = sqlx::query_as("SELECT Value FROM Systems WHERE Name = 'FirstAdminSetupComplete'")
        .fetch_optional(&app.db)
        .await?;
    Ok(Json(json!({"name": "FirstAdminSetupComplete", "value": v.map(|v| v.0).unwrap_or_else(|| "false".into())})))
}

/// `getRolesByNames` through the role cache.
async fn roles_by_names(
    St(app): St<State>,
    _: crate::app::AuthTrusted,
    body: axum::body::Bytes,
) -> ApiResult<Json<Vec<Role>>> {
    // `SortedArrayFromJSON`: a bad body is a payload error, duplicates dropped.
    let mut names: Vec<String> = serde_json::from_slice::<Option<Vec<String>>>(&body)
        .map_err(|e| AppError::bad_request("api.payload.parse.error", "").detail(e.to_string()))?
        .unwrap_or_default();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Err(AppError::invalid_param("rolenames"));
    }
    if names.len() > 100 {
        let mut e = AppError::bad_request("api.roles.get_multiple_by_name_too_many.request_error", "");
        if let Some(m) = crate::i18n::tr(&e.id, &[("MaxNames", "100")]) {
            e.message = m;
        }
        return Err(e);
    }
    // CleanRoleNames
    let mut clean = vec![];
    for n in names {
        let n = n.trim().to_string();
        if n.is_empty() || n.len() > 64 || !n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(AppError::invalid_param("rolename"));
        }
        clean.push(n);
    }
    let refs: Vec<&str> = clean.iter().map(String::as_str).collect();
    let mut roles: Vec<Role> = app.roles(&refs).await?.iter().map(|r| (**r).clone()).collect();
    roles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(roles))
}

/// `pushNotificationAck`: the ack is passed on to the push proxy. An id-loaded notification would get its
/// message from the licensed notification service, which this server (as Go without a license) does not have.
async fn push_ack(St(app): St<State>, Auth(s): Auth, body: axum::body::Bytes) -> ApiResult<Response> {
    let parse = |e: String| AppError::bad_request("api.push_notifications_ack.message.parse.app_error", "").detail(e);
    let v = serde_json::Deserializer::from_slice(&body)
        .into_iter::<Value>()
        .next()
        .ok_or_else(|| parse("EOF".into()))?
        .map_err(|e| parse(e.to_string()))?;
    let ack: crate::push::Ack =
        if v.is_null() { Default::default() } else { serde_json::from_value(v).map_err(|e| parse(e.to_string()))? };
    if !app.cfg.bool("EmailSettings.SendPushNotifications") {
        return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.push_notification.disabled.app_error", ""));
    }
    if ack.notification_type == crate::push::MESSAGE
        && s.prop(crate::push::PROP_DEVICE_NOTIFICATION_DISABLED) == "true"
        && ack.client_platform == "ios"
    {
        // iOS sends no ack while notifications are off, so one means they are on again.
        let _ = crate::push::set_session_props(&app, &s, &[(crate::push::PROP_DEVICE_NOTIFICATION_DISABLED, "false")])
            .await;
        app.clear_sessions_for_user(&s.user_id);
    }
    let sent = crate::push::send_ack(&app, &ack).await;
    if ack.is_id_loaded {
        if !ack.post_id.is_empty() && ack.notification_type == crate::push::MESSAGE {
            let post: Option<crate::model::Post> = crate::db::get(&app.db, &ack.post_id).await?;
            let post =
                post.filter(|p| p.delete_at == 0).ok_or_else(|| AppError::not_found("app.post.get.app_error", ""))?;
            let c = app.channel_or_404(&post.channel_id).await?;
            if !app.can_read_channel(&s, &c).await?.0 {
                return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
            }
            return Err(AppError::new(StatusCode::FOUND, "api.system.id_loaded.not_available.app_error", ""));
        }
        return Ok(StatusCode::OK.into_response());
    }
    if let Err(e) = sent {
        return Err(AppError::internal("api.push_notifications_ack.forward.app_error", e));
    }
    Ok(crate::users::ok().into_response())
}
