//! System administration: analytics, audits, caches, logs, the server-busy state, restart, jobs, usage, server
//! limits, the latest release, product notices, onboarding, and the connection tests of the system console.

// Based on Mattermost server/channels/api4/system.go, job.go, usage.go, limits.go, metrics.go, plugin.go
// (first admin visit), shared_channel.go (canUserDirectMessage), license.go, permission.go, and app/analytics.go,
// admin.go, usage.go, onboarding.go, product_notices.go, platform/busy.go, Copyright (c) 2015-present
// Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use crate::app::Query;
use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use crate::app::{ApiResult, App, AppError, Auth, MaybeAuth, State, is_valid_id, now_ms};
use crate::model::Session;
use crate::users::ok;

type Q = Query<HashMap<String, String>>;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/analytics/old", get(analytics))
        .route("/api/v4/audits", get(audits))
        .route("/api/v4/caches/invalidate", post(invalidate_caches))
        .route("/api/v4/client_perf", post(client_perf))
        .route("/api/v4/database/recycle", post(recycle_database))
        .route("/api/v4/latest_version", get(latest_version))
        .route("/api/v4/limits/server", get(server_limits))
        .route("/api/v4/plugins/marketplace/first_admin_visit", get(first_admin_visit).post(set_first_admin_visit))
        .route("/api/v4/server_busy", get(get_busy).post(set_busy).delete(clear_busy))
        .route("/api/v4/sharedchannels/users/{user_id}/can_dm/{other_user_id}", get(can_dm))
        .route("/api/v4/system/schema/version", get(schema_version))
        .route("/api/v4/system/timezones", get(timezones))
        .route("/api/v4/upgrade_to_enterprise", post(upgrade_to_enterprise))
        .route("/api/v4/upgrade_to_enterprise/status", get(upgrade_status))
        .route("/api/v4/usage/posts", get(usage_posts))
        .route("/api/v4/usage/storage", get(usage_storage))
        .route("/api/v4/usage/teams", get(usage_teams))
        .route("/api/v4/system/onboarding/complete", post(complete_onboarding))
        .route("/api/v4/system/notices/view", put(view_notices))
        .route("/api/v4/system/notices/{team_id}", get(notices))
        .route("/api/v4/redirect_location", get(redirect_location))
        .route("/api/v4/site_url/test", post(test_site_url))
        .route("/api/v4/email/test", post(test_email))
        .route("/api/v4/permissions/ancillary", post(ancillary_permissions))
        .route("/api/v4/restart", post(restart))
        .route("/api/v4/reports/users", get(user_report))
        .route("/api/v4/reports/users/count", get(user_report_count))
        .route("/api/v4/jobs", get(jobs))
        .route("/api/v4/jobs/{job_id}", get(job))
        .route("/api/v4/jobs/type/{job_type}", get(jobs_by_type))
        .route("/manualtest", get(manual_test))
}

const NO_CACHE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate");

impl App {
    /// `SessionHasPermissionToAndNotRestrictedAdmin`.
    pub(crate) async fn require_unrestricted(&self, s: &Session, permission: &str) -> ApiResult<()> {
        if self.has_perm(s, permission).await && !self.cfg.bool("ExperimentalSettings.RestrictSystemAdmin") {
            Ok(())
        } else {
            Err(AppError::forbidden_perm(permission))
        }
    }

    /// `System.SaveOrUpdate`.
    async fn save_system(&self, name: &str, value: &str) -> ApiResult<()> {
        sqlx::query("INSERT INTO Systems (Name, Value) VALUES ($1, $2) ON CONFLICT (Name) DO UPDATE SET Value = $2")
            .bind(name)
            .bind(value)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn admin_system_value(&self, name: &str) -> ApiResult<Option<String>> {
        Ok(sqlx::query_scalar("SELECT Value FROM Systems WHERE Name = $1").bind(name).fetch_optional(&self.db).await?)
    }

    async fn count(&self, sql: &str, team_id: &str) -> ApiResult<i64> {
        let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_string()));
        if !team_id.is_empty() {
            q = q.bind(team_id);
        }
        Ok(q.fetch_one(&self.db).await?)
    }
}

fn json_raw(v: &Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

fn query<'a>(q: &'a HashMap<String, String>, k: &str) -> &'a str {
    q.get(k).map(String::as_str).unwrap_or("")
}

fn page(q: &HashMap<String, String>) -> (i64, i64) {
    crate::posts::page_params(q.get("page").map(String::as_str), q.get("per_page").map(String::as_str))
}

// ---------- analytics ----------

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn rows(r: &[(&str, i64)]) -> Value {
    Value::Array(r.iter().map(|(n, v)| json!({"name": n, "value": v})).collect())
}

/// `getAnalytics` (`/analytics/old`).
async fn analytics(St(app): St<State>, Auth(s): Auth, Query(q): Q) -> ApiResult<Response> {
    let name = match query(&q, "name") {
        "" => "standard",
        n => n,
    };
    let team = query(&q, "team_id");
    app.require(&s, "get_analytics").await?;
    // Users.Count(UserCountOptions{}): people, not bots, not deleted.
    let people = "SELECT COUNT(*) FROM Users LEFT JOIN Bots ON Users.Id = Bots.UserId \
                  WHERE Users.DeleteAt = 0 AND (Users.RemoteId = '' OR Users.RemoteId IS NULL) AND Bots.UserId IS NULL";
    let system_users = app.count(people, "").await?;
    let v = match name {
        "standard" => {
            let tf = if team.is_empty() { "" } else { " AND TeamId = $1" };
            let open = app.count(&format!("SELECT COUNT(*) FROM Channels WHERE Type = 'O'{tf}"), team).await?;
            let private = app.count(&format!("SELECT COUNT(*) FROM Channels WHERE Type = 'P'{tf}"), team).await?;
            let posts = app
                .count(
                    &format!(
                        "SELECT COALESCE(SUM(num), 0)::bigint FROM posts_by_team_day{}",
                        if team.is_empty() { "" } else { " WHERE teamid = $1" }
                    ),
                    team,
                )
                .await?;
            let (users, inactive) = if team.is_empty() {
                let inactive = app
                    .count("SELECT COUNT(Users.Id) FROM Users LEFT JOIN Bots ON Users.Id = Bots.UserId WHERE Users.DeleteAt > 0 AND Bots.UserId IS NULL", "")
                    .await?;
                (system_users, inactive)
            } else {
                let n = app
                    .count(&format!("{people} AND Users.Id IN (SELECT UserId FROM TeamMembers WHERE TeamId = $1 AND DeleteAt = 0)"), team)
                    .await?;
                (n, -1)
            };
            let teams = app.count("SELECT COUNT(*) FROM Teams WHERE DeleteAt = 0", "").await?;
            let active = |since: i64| {
                format!(
                    "SELECT COUNT(*) FROM Status AS s LEFT JOIN Bots ON s.UserId = Bots.UserId LEFT JOIN Users ON s.UserId = Users.Id \
                     WHERE LastActivityAt > {} AND Bots.UserId IS NULL AND (Users.RemoteId = '' OR Users.RemoteId IS NULL) \
                     AND Users.DeleteAt = 0",
                    now_ms() - since
                )
            };
            let daily = app.count(&active(DAY_MS), "").await?;
            let monthly = app.count(&active(30 * DAY_MS), "").await?;
            rows(&[
                ("channel_open_count", open),
                ("channel_private_count", private),
                ("post_count", posts),
                ("unique_user_count", users),
                ("team_count", teams),
                ("total_websocket_connections", app.hub.connections() as i64),
                ("total_master_db_connections", app.db.size() as i64),
                ("total_read_db_connections", 0),
                ("daily_active_users", daily),
                ("monthly_active_users", monthly),
                ("inactive_user_count", inactive),
                ("single_channel_guest_count", 0),
            ])
        }
        "post_counts_day" | "bot_post_counts_day" => {
            let view = if name == "post_counts_day" { "posts_by_team_day" } else { "bot_posts_by_team_day" };
            let yesterday = chrono::Local::now().date_naive() - chrono::Duration::days(1);
            let (start, end) = ((yesterday - chrono::Duration::days(31)).to_string(), yesterday.to_string());
            let sql = if team.is_empty() {
                format!(
                    "SELECT TO_CHAR(day, 'YYYY-MM-DD') AS Name, COALESCE(SUM(num), 0)::bigint AS Value FROM {view} \
                     WHERE day >= $1::date AND day <= $2::date GROUP BY Name ORDER BY Name DESC LIMIT 30"
                )
            } else {
                format!(
                    "SELECT TO_CHAR(day, 'YYYY-MM-DD') AS Name, num::bigint AS Value FROM {view} \
                     WHERE teamid = $3 AND day >= $1::date AND day <= $2::date ORDER BY Name DESC LIMIT 30"
                )
            };
            let mut qy = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(sql)).bind(start).bind(end);
            if !team.is_empty() {
                qy = qy.bind(team);
            }
            let r = qy.fetch_all(&app.db).await?;
            Value::Array(r.into_iter().map(|(n, v)| json!({"name": n, "value": v})).collect())
        }
        "user_counts_with_posts_day" => {
            if system_users > app.cfg.i64("AnalyticsSettings.MaxUsersForStatistics") {
                rows(&[("", -1)])
            } else {
                let yesterday = chrono::Local::now().date_naive() - chrono::Duration::days(1);
                let ms = |d: chrono::NaiveDate, end: bool| {
                    let t = if end { d.and_hms_milli_opt(23, 59, 59, 999) } else { d.and_hms_opt(0, 0, 0) };
                    t.and_then(|t| t.and_local_timezone(chrono::Local).single()).map_or(0, |t| t.timestamp_millis())
                };
                let (start, end) = (ms(yesterday - chrono::Duration::days(31), false), ms(yesterday, true));
                let join = if team.is_empty() {
                    " WHERE".to_string()
                } else {
                    " INNER JOIN Channels ON Posts.ChannelId = Channels.Id AND Channels.TeamId = $3 AND".to_string()
                };
                let sql = format!(
                    "SELECT TO_CHAR(DATE(TO_TIMESTAMP(Posts.CreateAt / 1000)), 'YYYY-MM-DD') AS Name, \
                     COUNT(DISTINCT Posts.UserId)::bigint AS Value FROM Posts{join} Posts.CreateAt >= $1 AND Posts.CreateAt <= $2 \
                     GROUP BY DATE(TO_TIMESTAMP(Posts.CreateAt / 1000)) ORDER BY Name DESC LIMIT 30"
                );
                let mut qy = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(sql)).bind(start).bind(end);
                if !team.is_empty() {
                    qy = qy.bind(team);
                }
                let r = qy.fetch_all(&app.db).await?;
                Value::Array(r.into_iter().map(|(n, v)| json!({"name": n, "value": v})).collect())
            }
        }
        "extra_counts" => {
            let tf = if team.is_empty() { "" } else { " AND TeamId = $1" };
            let incoming =
                app.count(&format!("SELECT COUNT(*) FROM IncomingWebhooks WHERE DeleteAt = 0{tf}"), team).await?;
            // Go's AnalyticsOutgoingCount builds `WHERE ... AND TeamId` without a value for a team, an SQL error;
            // the team is compared here.
            let outgoing =
                app.count(&format!("SELECT COUNT(*) FROM OutgoingWebhooks WHERE DeleteAt = 0{tf}"), team).await?;
            let commands = app.count(&format!("SELECT COUNT(*) FROM Commands WHERE DeleteAt = 0{tf}"), team).await?;
            let sessions =
                app.count(&format!("SELECT COUNT(*) FROM Sessions WHERE ExpiresAt > {}", now_ms()), "").await?;
            let files = app.count("SELECT COALESCE((SELECT num FROM file_stats LIMIT 1), 0)::bigint", "").await?;
            let size = app.count("SELECT COALESCE((SELECT usage FROM file_stats LIMIT 1), 0)::bigint", "").await?;
            rows(&[
                ("incoming_webhook_count", incoming),
                ("outgoing_webhook_count", outgoing),
                ("command_count", commands),
                ("session_count", sessions),
                ("total_file_count", files),
                ("total_file_size", size),
            ])
        }
        _ => return Err(AppError::invalid_param("name")),
    };
    Ok(json_raw(&v))
}

// ---------- audits, caches, database, client metrics ----------

/// `getAudits`: the newest first.
async fn audits(St(app): St<State>, Auth(s): Auth, Query(q): Q) -> ApiResult<Response> {
    app.require(&s, "read_audits").await?;
    let (page, per_page) = page(&q);
    let r: Vec<(String, i64, String, String, String, String, String)> = sqlx::query_as(
        "SELECT Id, COALESCE(CreateAt, 0), COALESCE(UserId, ''), COALESCE(Action, ''), COALESCE(ExtraInfo, ''), \
         COALESCE(IpAddress, ''), COALESCE(SessionId, '') FROM Audits ORDER BY CreateAt DESC LIMIT $1 OFFSET $2",
    )
    .bind(per_page)
    .bind(page * per_page)
    .fetch_all(&app.db)
    .await?;
    let v: Vec<Value> = r
        .into_iter()
        .map(|(id, create_at, user_id, action, extra_info, ip_address, session_id)| {
            json!({"id": id, "create_at": create_at, "user_id": user_id, "action": action, "extra_info": extra_info,
                   "ip_address": ip_address, "session_id": session_id})
        })
        .collect();
    Ok(json_raw(&Value::Array(v)))
}

/// `invalidateCaches`.
async fn invalidate_caches(St(app): St<State>, Auth(s): Auth) -> ApiResult<Response> {
    app.require_unrestricted(&s, "invalidate_caches").await?;
    crate::license::invalidate_all_caches(&app).await;
    Ok(([NO_CACHE], ok()).into_response())
}

/// `databaseRecycle`: the pool's connections are replaced as they are next released.
async fn recycle_database(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    app.require_unrestricted(&s, "recycle_database_connections").await?;
    Ok(ok())
}

/// `submitPerformanceReport`: without metrics (an enterprise feature), nothing is recorded or checked.
async fn client_perf(_: crate::app::AuthTrusted) -> StatusCode {
    StatusCode::OK
}

// ---------- latest version, limits ----------

/// `GetLatestVersion`: the latest release on GitHub, kept a day.
async fn latest_version(St(app): St<State>, Auth(s): Auth) -> ApiResult<Response> {
    app.require_unrestricted(&s, crate::perm::MANAGE_SYSTEM).await?;
    static CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);
    if let Some((at, v)) = CACHE.lock().unwrap().clone()
        && at.elapsed() < Duration::from_secs(24 * 3600)
    {
        return Ok(json_raw(&v));
    }
    let fail = |e: String| AppError::internal("<untranslated>", e);
    let res = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("Go-http-client/1.1")
        .build()
        .map_err(|e| fail(e.to_string()))?
        .get("https://api.github.com/repos/mattermost/mattermost-server/releases/latest")
        .send()
        .await
        .map_err(|e| fail(e.to_string()))?;
    let body: Value = serde_json::from_slice(&res.bytes().await.map_err(|e| fail(e.to_string()))?)
        .map_err(|e| fail(e.to_string()))?;
    // GithubReleaseInfo, and its IsValid.
    let mut v = Map::new();
    for k in ["id", "tag_name", "name", "created_at", "published_at", "body", "html_url"] {
        let d = if k == "id" { json!(0) } else { json!("") };
        v.insert(k.into(), body.get(k).cloned().filter(|x| !x.is_null()).unwrap_or(d));
    }
    if v["id"].as_i64().unwrap_or(0) == 0 {
        return Err(fail("id is not set".into()));
    }
    for k in ["tag_name", "name", "created_at", "published_at", "html_url"] {
        if v[k].as_str().is_none_or(str::is_empty) {
            return Err(fail(format!("{k} is not set")));
        }
    }
    let v = Value::Object(v);
    *CACHE.lock().unwrap() = Some((Instant::now(), v.clone()));
    Ok(json_raw(&v))
}

/// `getServerLimits`. This server has no user limit (Go's unlicensed one is 200 and 250), so the limits are 0
/// (none); system admins see the active user count.
async fn server_limits(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    let admin = s.roles.split_whitespace().any(|r| r == crate::perm::SYSTEM_ADMIN)
        && app.has_perm(&s, "sysconsole_read_user_management_users").await;
    let active = if admin {
        app.count(
            "SELECT COUNT(*) FROM Users LEFT JOIN Bots ON Users.Id = Bots.UserId \
             WHERE Users.DeleteAt = 0 AND (Users.RemoteId = '' OR Users.RemoteId IS NULL) AND Bots.UserId IS NULL",
            "",
        )
        .await?
    } else {
        0
    };
    Ok(Json(
        json!({"maxUsersLimit": 0, "maxUsersHardLimit": 0, "activeUserCount": active, "singleChannelGuestCount": 0,
                   "singleChannelGuestLimit": 0, "postHistoryLimit": 0, "lastAccessiblePostTime": 0}),
    ))
}

// ---------- first admin visit, onboarding ----------

async fn first_admin_visit(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    crate::audit::log(&app, Some(&s), "attempt").await;
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    let v = app.admin_system_value("FirstAdminVisitMarketplace").await?.unwrap_or_else(|| "false".into());
    Ok(Json(json!({"name": "FirstAdminVisitMarketplace", "value": v})))
}

async fn set_first_admin_visit(St(app): St<State>, MaybeAuth(s): MaybeAuth) -> ApiResult<Json<Value>> {
    crate::audit::log(&app, s.as_deref(), "attempt").await;
    let Some(s) = s else { return Err(AppError::forbidden_perm(crate::perm::MANAGE_SYSTEM)) };
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    app.save_system("FirstAdminVisitMarketplace", "true")
        .await
        .map_err(|_| AppError::internal("api.error_set_first_admin_visit_marketplace_status", ""))?;
    app.hub.publish(
        "first_admin_visit_marketplace_status_received",
        json!({"firstAdminVisitMarketplaceStatus": "true"}),
        Default::default(),
    );
    Ok(ok())
}

/// `completeOnboarding`: the organization's name kept, the first admin's setup done (there are no plugins to
/// install).
async fn complete_onboarding(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    if !app.has_perm(&s, crate::perm::MANAGE_SYSTEM).await {
        return Err(AppError::new(StatusCode::FORBIDDEN, "app.system.complete_onboarding_request.no_first_user", ""));
    }
    let req: Value = serde_json::from_slice(&body)
        .ok()
        .filter(|v: &Value| v.is_object() || v.is_null())
        .ok_or_else(|| AppError::bad_request("app.system.complete_onboarding_request.app_error", ""))?;
    let org = req.get("organization").and_then(Value::as_str).unwrap_or("");
    if org.is_empty() {
        return Err(AppError::bad_request("api.error_no_organization_name_provided_for_self_hosted_onboarding", ""));
    }
    let _ = app.save_system("OrganizationName", org).await;
    app.save_system("FirstAdminSetupComplete", "true")
        .await
        .map_err(|_| AppError::internal("api.error_set_first_admin_complete_setup", ""))?;
    Ok(ok())
}

// ---------- server busy ----------

/// `Busy`: until when non-critical services are off (Unix milliseconds); 0 when not busy. An atomic, as Go's
/// `busy` flag, so that `is_busy` is one load on the hot paths; Go's timer that clears it at the expiry is the
/// comparison with the clock.
static BUSY: AtomicI64 = AtomicI64::new(0);

/// `Busy.IsBusy`.
pub fn is_busy() -> bool {
    let t = BUSY.load(Ordering::Relaxed);
    t != 0 && now_ms() < t
}

/// `Busy.Expires` in Unix seconds (`time.Time.Unix`), none when not busy.
fn busy_until() -> Option<i64> {
    let t = BUSY.load(Ordering::Relaxed);
    (t != 0 && now_ms() < t).then(|| t.div_euclid(1000))
}

/// `Busy.ToJSON`: the zero time when not busy, as Go prints it.
async fn get_busy(St(app): St<State>, Auth(s): Auth) -> ApiResult<Response> {
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    let v = match busy_until() {
        Some(t) => {
            let ts = chrono::DateTime::from_timestamp(t, 0).unwrap_or_default();
            json!({"busy": true, "expires": t, "expires_ts": ts.format("%a %b %-d %H:%M:%S +0000 UTC %Y").to_string()})
        }
        None => json!({"busy": false, "expires": -62135596800i64, "expires_ts": "Mon Jan 1 00:00:00 +0000 UTC 0001"}),
    };
    Ok(json_raw(&v))
}

async fn set_busy(St(app): St<State>, Auth(s): Auth, Query(q): Q) -> ApiResult<Json<Value>> {
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    const MAX: i64 = 86400;
    let secs = match query(&q, "seconds") {
        "" => 3600,
        v => v.parse::<i64>().unwrap_or(-1),
    };
    if secs <= 0 || secs > MAX {
        return Err(AppError::invalid_url_param(&format!("seconds must be 1 - {MAX}")));
    }
    BUSY.store(now_ms() + secs * 1000, Ordering::Relaxed);
    Ok(ok())
}

async fn clear_busy(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    BUSY.store(0, Ordering::Relaxed);
    Ok(ok())
}

// ---------- shared channels, schema, timezones ----------

/// `canUserDirectMessage`: without shared channels, anyone the user can see.
async fn can_dm(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, other)): Path<(String, String)>,
) -> ApiResult<Response> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    if !is_valid_id(&other) {
        return Err(AppError::invalid_url_param("other_user_id"));
    }
    let viewer = Session { user_id: user_id.clone(), ..(*s).clone() };
    let can = app.can_see_user(&viewer, &other).await?;
    Ok(json_raw(&json!({"can_dm": can})))
}

/// `getAppliedSchemaMigrations`.
async fn schema_version(St(app): St<State>, Auth(s): Auth) -> ApiResult<Response> {
    if !app.session_permissions(&s).await.iter().any(|p| p.starts_with("sysconsole_read_")) {
        return Err(AppError::forbidden_perm("sysconsole_read_*"));
    }
    let r: Vec<(i64, String)> = sqlx::query_as("SELECT Version::bigint, Name FROM db_migrations ORDER BY Version DESC")
        .fetch_all(&app.db)
        .await?;
    Ok(json_raw(&Value::Array(r.into_iter().map(|(v, n)| json!({"version": v, "name": n})).collect())))
}

/// `getSupportedTimezones` (`data/timezones.json`, Go's `DefaultSupportedTimezones`).
async fn timezones(_: Auth) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        include_str!("../data/timezones.json").trim().replace(['\n', ' '], ""),
    )
        .into_response()
}

// ---------- enterprise upgrade, license, restart ----------

/// `upgradeToEnterprise`: this server cannot be replaced by an enterprise binary.
async fn upgrade_to_enterprise(St(app): St<State>, Auth(s): Auth) -> ApiResult<Response> {
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    Err(AppError::new(StatusCode::FORBIDDEN, "api.upgrade_to_enterprise.system_not_supported.app_error", ""))
}

/// `upgradeToEnterpriseStatus`: no upgrade ever started.
async fn upgrade_status(St(app): St<State>, Auth(s): Auth) -> ApiResult<Response> {
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    Ok(json_raw(&json!({"error": null, "percentage": 0})))
}

/// `restart`: answered, then the server starts again in place (the same program, arguments and environment), as
/// Go's `Server.Restart` does within its process.
async fn restart(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        use std::os::unix::process::CommandExt;
        let Ok(exe) = std::env::current_exe() else { return };
        let err = std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec();
        eprintln!("restart: {err}");
    });
    Ok(ok())
}

// ---------- usage ----------

/// `RoundOffToZeroesResolution`.
fn round_off(n: i64, min_resolution: u32) -> i64 {
    if (-9..=9).contains(&n) {
        return if min_resolution == 0 { n } else { 0 };
    }
    let zeroes = (n.unsigned_abs() as f64).log10() as u32;
    let tens = 10i64.pow(zeroes.min(min_resolution));
    n / tens * tens
}

async fn usage_posts(St(app): St<State>, _: Auth) -> ApiResult<Response> {
    let n = app
        .count("SELECT COUNT(*) FROM Posts p WHERE p.Type = '' AND p.UserId NOT IN (SELECT UserId FROM Bots) AND p.DeleteAt = 0", "")
        .await
        .map_err(|_| AppError::internal("app.post.analytics_posts_count.app_error", ""))?;
    Ok(json_raw(&json!({"count": round_off(n, 3)})))
}

async fn usage_storage(St(app): St<State>, _: Auth) -> ApiResult<Response> {
    let n = app
        .count("SELECT COALESCE((SELECT usage FROM file_stats LIMIT 1), 0)::bigint", "")
        .await
        .map_err(|_| AppError::internal("app.usage.get_storage_usage.app_error", ""))?;
    Ok(json_raw(&json!({"bytes": round_off(n, 8)})))
}

async fn usage_teams(St(app): St<State>, _: Auth) -> ApiResult<Response> {
    let active = app.count("SELECT COUNT(*) FROM Teams WHERE DeleteAt = 0", "").await?;
    let archived = app.count("SELECT COUNT(*) FROM Teams WHERE DeleteAt > 0 AND CloudLimitsArchived", "").await?;
    Ok(json_raw(&json!({"active": active, "cloud_archived": archived})))
}

// ---------- product notices ----------

/// `updateViewedProductNotices`: each notice seen once more, now.
async fn view_notices(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    crate::audit::log(&app, Some(&s), "attempt").await;
    let ids: Vec<String> =
        serde_json::from_slice(&body).map_err(|_| AppError::bad_request("api.payload.parse.error", ""))?;
    view_product_notices(&app, &s.user_id, &ids)
        .await
        .map_err(|_| AppError::bad_request("api.system.update_viewed_notices.failed", ""))?;
    Ok(ok())
}

/// `ProductNoticesStore.View`: each notice seen once more by the user, now.
pub async fn view_product_notices(app: &App, user_id: &str, ids: &[String]) -> Result<(), sqlx::Error> {
    if ids.is_empty() {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp();
    let mut tx = app.db.begin().await?;
    let seen: Vec<String> =
        sqlx::query_scalar("SELECT NoticeId FROM ProductNoticeViewState WHERE UserId = $1 AND NoticeId = ANY($2)")
            .bind(user_id)
            .bind(ids)
            .fetch_all(&mut *tx)
            .await?;
    sqlx::query("UPDATE ProductNoticeViewState SET Viewed = Viewed + 1, Timestamp = $3 WHERE UserId = $1 AND NoticeId = ANY($2)")
        .bind(user_id)
        .bind(&seen)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    for id in ids.iter().filter(|i| !seen.contains(i)) {
        sqlx::query("INSERT INTO ProductNoticeViewState (UserId, NoticeId, Viewed, Timestamp) VALUES ($1, $2, 1, $3)")
            .bind(user_id)
            .bind(id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

/// `getProductNotices`: notices come from the notices URL, which this server does not fetch: none.
async fn notices(Auth(_): Auth, Path(team_id): Path<String>, Query(q): Q) -> ApiResult<Response> {
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    // NoticeClientTypeFromString
    if !matches!(query(&q, "client"), "web" | "desktop" | "mobile-ios" | "mobile-android") {
        return Err(AppError::invalid_param("client").detail("Invalid client type supplied"));
    }
    Ok(json_raw(&json!([])))
}

// ---------- system console tests ----------

/// `getRedirectLocation`: where a URL redirects to, kept an hour.
async fn redirect_location(St(app): St<State>, _: Auth, Query(q): Q) -> ApiResult<Response> {
    let mut m = Map::new();
    m.insert("location".into(), Value::String(String::new()));
    if !app.cfg.bool("ServiceSettings.EnableLinkPreviews") {
        return Ok(json_raw(&Value::Object(m)));
    }
    let url = query(&q, "url").to_string();
    if url.is_empty() {
        return Err(AppError::invalid_param("url"));
    }
    static CACHE: Mutex<Option<HashMap<String, (Instant, String)>>> = Mutex::new(None);
    let hit = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&url).cloned();
    let location = match hit {
        Some((at, l)) if at.elapsed() < Duration::from_secs(3600) => l,
        _ => {
            // An internal address is a failure like any other (Go's dialer refuses it): "" for an hour. The client
            // refuses it at connect time too; this spares the attempt.
            let internal = reqwest::Url::parse(&url).is_ok_and(|u| !app.untrusted_url_allowed(&u));
            let l = if internal {
                String::new()
            } else {
                match app.no_redirect_client().head(&url).send().await {
                    Ok(r) => r.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string(),
                    Err(_) => String::new(),
                }
            };
            let mut c = CACHE.lock().unwrap();
            let c = c.get_or_insert_with(HashMap::new);
            if c.len() > 10000 {
                c.clear();
            }
            c.insert(url, (Instant::now(), l.clone()));
            l
        }
    };
    m.insert("location".into(), Value::String(location));
    Ok(json_raw(&Value::Object(m)))
}

/// `testSiteURL`: the URL must answer its ping.
async fn test_site_url(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    app.require_unrestricted(&s, "test_site_url").await?;
    let props: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let site = props.get("site_url").and_then(Value::as_str).unwrap_or("");
    if site.is_empty() {
        return Err(AppError::invalid_param("site_url"));
    }
    let ok_ping = match reqwest::get(format!("{site}/api/v4/system/ping")).await {
        Ok(r) => r.status() == reqwest::StatusCode::OK,
        Err(_) => false,
    };
    if !ok_ping {
        return Err(AppError::bad_request("app.admin.test_site_url.failure", ""));
    }
    Ok(ok())
}

/// The config a test request sends, or the server's when it sends none (Go logs a bad body and goes on).
fn test_config(app: &App, body: &Bytes) -> Value {
    match serde_json::from_slice::<Value>(body) {
        Ok(v @ Value::Object(_)) => v,
        _ => app.cfg.value().clone(),
    }
}

/// `checkHasNilFields` over a section of a test request's config.
fn has_nil_fields(cfg: &Value, section: &str) -> bool {
    match (
        crate::config::defaults().get(section).and_then(Value::as_object),
        cfg.get(section).and_then(Value::as_object),
    ) {
        (Some(schema), Some(m)) => schema.keys().any(|k| m.get(k).is_none_or(Value::is_null)),
        _ => true,
    }
}

/// `testEmail` (`TestEmail`): a test email to the caller over the SMTP settings sent. Go checks the sent settings but
/// sends over the saved ones; the sent ones are used here, so the console tests what the admin typed.
async fn test_email(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    let mut cfg = test_config(&app, &body);
    if has_nil_fields(&cfg, "EmailSettings") {
        return Err(AppError::bad_request("api.file.test_connection_email_settings_nil.app_error", ""));
    }
    app.require_unrestricted(&s, "test_email").await?;
    let e = cfg["EmailSettings"].clone();
    let st = |k: &str| e[k].as_str().unwrap_or("").to_string();
    if st("SMTPServer").is_empty() {
        return Err(AppError::bad_request("api.admin.test_email.missing_server", "").detail(
            crate::i18n::tr("api.context.invalid_param.app_error", &[("Name", "SMTPServer")]).unwrap_or_default(),
        ));
    }
    if st("SMTPPassword") == "********************************" {
        let cur = |k: &str| app.cfg.str(&format!("EmailSettings.{k}")).to_string();
        if st("SMTPServer") == cur("SMTPServer")
            && st("SMTPPort") == cur("SMTPPort")
            && st("SMTPUsername") == cur("SMTPUsername")
        {
            cfg["EmailSettings"]["SMTPPassword"] = Value::String(cur("SMTPPassword"));
        } else {
            return Err(AppError::bad_request("api.admin.test_email.reenter_password", ""));
        }
    }
    let user = app.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;
    let smtp = crate::mail::SmtpConfig::from_config(&cfg["EmailSettings"], &crate::email::site_url(&app), "");
    let m = crate::mail::Mail {
        to: user.email.clone(),
        subject: crate::i18n::tl(&user.locale, "api.admin.test_email.subject", &[]),
        html: crate::i18n::tl(&user.locale, "api.admin.test_email.body", &[]),
        ..Default::default()
    };
    crate::mail::send(&m, &smtp)
        .await
        .map_err(|err| AppError::internal("app.admin.test_email.failure", "").with_param("Error", &err))?;
    Ok(ok())
}

/// `appendAncillaryPermissionsPost` (`data/ancillary_permissions.json`, Go's `SysconsoleAncillaryPermissions`).
async fn ancillary_permissions(_: Auth, body: Bytes) -> ApiResult<Response> {
    static MAP: std::sync::OnceLock<HashMap<String, Vec<String>>> = std::sync::OnceLock::new();
    let map = MAP
        .get_or_init(|| serde_json::from_str(include_str!("../data/ancillary_permissions.json")).unwrap_or_default());
    let perms: Vec<String> = serde_json::from_slice(&body)
        .ok()
        .filter(|p: &Vec<String>| !p.is_empty())
        .ok_or_else(|| AppError::bad_request("api.payload.parse.error", ""))?;
    let mut out = perms.clone();
    for p in &perms {
        if let Some(a) = map.get(p) {
            out.extend(a.iter().cloned());
        }
    }
    Ok(json_raw(&json!(out)))
}

// ---------- jobs ----------

/// `AllJobTypes`: what a listing without a type covers, and the types `IsValidJobType` accepts.
const ALL_JOB_TYPES: &[&str] = &[
    "data_retention",
    "message_export",
    "elasticsearch_post_indexing",
    "elasticsearch_post_aggregation",
    "ldap_sync",
    "migrations",
    "plugins",
    "expiry_notify",
    "product_notices",
    "active_users",
    "import_process",
    "import_delete",
    "export_process",
    "export_delete",
    "cloud",
    "extract_content",
    "last_accessible_post",
    "last_accessible_file",
    "cleanup_desktop_tokens",
    "cleanup_expired_access_tokens",
    "notify_expiring_access_tokens",
    "refresh_materialized_views",
    "mobile_session_metadata",
    "scheduled_recap",
];

/// `SessionHasPermissionToReadJob`: the permission a job type needs, if any.
async fn read_job_permission(app: &App, s: &Session, job_type: &str) -> Option<(bool, &'static str)> {
    let p = match job_type {
        "data_retention" => "read_data_retention_job",
        "message_export" => "read_compliance_export_job",
        "elasticsearch_post_indexing" => "read_elasticsearch_post_indexing_job",
        "elasticsearch_post_aggregation" => "read_elasticsearch_post_aggregation_job",
        "ldap_sync" => "read_ldap_sync_job",
        "migrations"
        | "plugins"
        | "product_notices"
        | "expiry_notify"
        | "active_users"
        | "import_process"
        | "import_delete"
        | "export_process"
        | "export_delete"
        | "cloud"
        | "mobile_session_metadata"
        | "extract_content"
        | "post_persistent_notifications"
        | "delete_expired_posts"
        | "cleanup_expired_access_tokens" => "read_jobs",
        "access_control_sync" => crate::perm::MANAGE_SYSTEM,
        "access_control_team_sync" => {
            if app.has_perm(s, crate::perm::MANAGE_SYSTEM).await {
                return Some((true, crate::perm::MANAGE_SYSTEM));
            }
            "manage_team_access_rules"
        }
        _ => return None,
    };
    Some((app.has_perm(s, p).await, p))
}

const JOB_COLS: &str = "Id, Type, Priority, CreateAt, StartAt, LastActivityAt, Status, Progress, Data::text";

type JobRow = (String, String, i64, i64, i64, i64, String, i64, Option<String>);

fn job_json(r: JobRow) -> Value {
    let (id, t, priority, create_at, start_at, last, status, progress, data) = r;
    let data: Value = data.and_then(|d| serde_json::from_str(&d).ok()).unwrap_or(Value::Null);
    json!({"id": id, "type": t, "priority": priority, "create_at": create_at, "start_at": start_at,
           "last_activity_at": last, "status": status, "progress": progress, "data": data})
}

async fn job_page(app: &App, types: &[&str], status: &str, page: i64, per_page: i64) -> ApiResult<Response> {
    let types: Vec<String> = types.iter().map(|t| t.to_string()).collect();
    let sql = format!(
        "SELECT {JOB_COLS} FROM Jobs WHERE Type = ANY($1){} ORDER BY CreateAt DESC LIMIT $2 OFFSET $3",
        if status.is_empty() { "" } else { " AND Status = $4" }
    );
    let mut q = sqlx::query_as::<_, JobRow>(sqlx::AssertSqlSafe(sql)).bind(types).bind(per_page).bind(page * per_page);
    if !status.is_empty() {
        q = q.bind(status);
    }
    let r = q.fetch_all(&app.db).await?;
    Ok(json_raw(&Value::Array(r.into_iter().map(job_json).collect())))
}

/// `getJobs`.
async fn jobs(St(app): St<State>, Auth(s): Auth, Query(q): Q) -> ApiResult<Response> {
    let job_type = query(&q, "job_type");
    let mut types = vec![];
    if !job_type.is_empty() {
        let Some(&t) = ALL_JOB_TYPES.iter().find(|t| **t == job_type) else {
            return Err(AppError::invalid_url_param("job_type"));
        };
        match read_job_permission(&app, &s, t).await {
            None => return Err(AppError::bad_request("api.job.retrieve.nopermissions", "")),
            Some((false, p)) => return Err(AppError::forbidden_perm(p)),
            Some((true, _)) => types.push(t),
        }
    } else {
        for t in ALL_JOB_TYPES {
            if let Some((true, _)) = read_job_permission(&app, &s, t).await {
                types.push(*t);
            }
        }
    }
    if types.is_empty() {
        return Err(AppError::forbidden());
    }
    let status = query(&q, "status");
    if !status.is_empty()
        && !matches!(
            status,
            "pending" | "in_progress" | "success" | "error" | "cancel_requested" | "canceled" | "warning"
        )
    {
        return Err(AppError::bad_request("api.job.status.invalid", ""));
    }
    let (page, per_page) = page(&q);
    job_page(&app, &types, status, page, per_page).await
}

/// `getJobsByType`.
async fn jobs_by_type(
    St(app): St<State>,
    Auth(s): Auth,
    Path(job_type): Path<String>,
    Query(q): Q,
) -> ApiResult<Response> {
    if job_type.is_empty() || job_type.len() > 32 {
        return Err(AppError::invalid_url_param("job_type"));
    }
    let Some((has, p)) = read_job_permission(&app, &s, &job_type).await else {
        return Err(AppError::bad_request("api.job.retrieve.nopermissions", ""));
    };
    let team_id = query(&q, "team_id");
    if !team_id.is_empty() && !is_valid_id(team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    let policy_id = query(&q, "policy_id");
    let access = job_type == "access_control_sync" || job_type == "access_control_team_sync";
    // Team-scoped access control sync requests: a team admin's own team, or a policy of it.
    let team_scoped = !has
        && job_type == "access_control_sync"
        && !team_id.is_empty()
        && app.has_team_perm(&s, team_id, "manage_team_access_rules").await;
    let policy_scoped = !has
        && job_type == "access_control_team_sync"
        && team_id.is_empty()
        && is_valid_id(policy_id)
        && app.has_team_perm(&s, policy_id, "manage_team_access_rules").await;
    if !has && !team_scoped && !policy_scoped {
        return Err(AppError::forbidden_perm(p));
    }
    let (page, per_page) = page(&q);
    let filter = if !team_id.is_empty() {
        Some(("team_id", team_id))
    } else if !policy_id.is_empty() && access {
        if !policy_scoped && !app.has_perm(&s, crate::perm::MANAGE_SYSTEM).await {
            return Err(AppError::forbidden_perm(crate::perm::MANAGE_SYSTEM));
        }
        Some(("policy_id", policy_id))
    } else {
        None
    };
    let Some(filter) = filter else {
        return job_page(&app, &[job_type.as_str()], "", page, per_page).await;
    };
    // GetJobsByTypeAndData, newest first, the page cut from them.
    let mut jobs = crate::jobs::get_by_type_and_data(&app, &job_type, &[filter])
        .await
        .map_err(|e| AppError::internal("app.job.get_all.app_error", e.detailed_error))?;
    jobs.sort_by_key(|j| std::cmp::Reverse(j.create_at));
    let page: Vec<_> = jobs.into_iter().skip((page * per_page) as usize).take(per_page as usize).collect();
    Ok(json_raw(&serde_json::to_value(page).unwrap_or_default()))
}

/// `getJob`.
async fn job(St(app): St<State>, Auth(s): Auth, Path(job_id): Path<String>) -> ApiResult<Response> {
    if !is_valid_id(&job_id) {
        return Err(AppError::invalid_url_param("job_id"));
    }
    let r: Option<JobRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {JOB_COLS} FROM Jobs WHERE Id = $1")))
        .bind(&job_id)
        .fetch_optional(&app.db)
        .await?;
    let r = r.ok_or_else(|| AppError::not_found("app.job.get.app_error", ""))?;
    match read_job_permission(&app, &s, &r.1).await {
        None => Err(AppError::bad_request("api.job.retrieve.nopermissions", "")),
        Some((false, p)) => Err(AppError::forbidden_perm(p)),
        Some((true, _)) => Ok(json_raw(&job_json(r))),
    }
}

// ---------- user reports ----------

/// `fillUserReportOptions` + `applyUserReportFilter`: the condition on `Users u` and its binds.
fn user_report_filter(q: &HashMap<String, String>) -> ApiResult<(String, Vec<String>)> {
    let team = query(q, "team_filter");
    if !(team.is_empty() || is_valid_id(team)) {
        return Err(AppError::bad_request("api.getUsersForReporting.invalid_team_filter", ""));
    }
    let (hide_active, hide_inactive) = (query(q, "hide_active") == "true", query(q, "hide_inactive") == "true");
    if hide_active && hide_inactive {
        return Err(AppError::bad_request("api.getUsersForReporting.invalid_active_filter", ""));
    }
    let mut cond = vec!["u.Id NOT IN (SELECT UserId FROM Bots)".to_string()];
    let mut args: Vec<String> = vec![];
    let bind = |v: String, args: &mut Vec<String>| {
        args.push(v);
        format!("${}", args.len())
    };
    let channels = "(SELECT COUNT(*) FROM ChannelMembers cm INNER JOIN Channels c ON c.Id = cm.ChannelId AND c.DeleteAt = 0 \
                    AND c.Type IN ('O','P') WHERE cm.UserId = u.Id)";
    let role = match query(q, "guest_filter") {
        "all" => "system_guest",
        "single_channel" => {
            cond.push(format!("{channels} = 1"));
            "system_guest"
        }
        "multiple_channel" => {
            cond.push(format!("{channels} > 1"));
            "system_guest"
        }
        _ => query(q, "role_filter"),
    };
    let escape = |s: &str| s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    if !role.is_empty() {
        let p = bind(format!("%{}%", escape(role)), &mut args);
        cond.push(format!("u.Roles LIKE LOWER({p})"));
    }
    if query(q, "has_no_team") == "true" {
        cond.push("u.Id NOT IN (SELECT UserId FROM TeamMembers WHERE DeleteAt = 0)".into());
    } else if !team.is_empty() {
        let p = bind(team.to_string(), &mut args);
        cond.push(format!("u.Id IN (SELECT UserId FROM TeamMembers WHERE DeleteAt = 0 AND TeamId = {p})"));
    }
    if hide_active {
        cond.push("u.DeleteAt > 0".into());
    }
    if hide_inactive {
        cond.push("u.DeleteAt = 0".into());
    }
    for term in query(q, "search_term").split_whitespace() {
        let p = bind(format!("{}%", escape(term.trim_start_matches('@')).to_lowercase()), &mut args);
        cond.push(format!(
            "(LOWER(u.Username) LIKE {p} OR LOWER(u.FirstName) LIKE {p} OR LOWER(u.LastName) LIKE {p} \
             OR LOWER(u.Nickname) LIKE {p} OR LOWER(u.Email) LIKE {p})"
        ));
    }
    Ok((cond.join(" AND "), args))
}

/// `getUserCountForReporting`.
async fn user_report_count(St(app): St<State>, Auth(s): Auth, Query(q): Q) -> ApiResult<Response> {
    app.require(&s, "sysconsole_read_user_management_users").await?;
    let (cond, args) = user_report_filter(&q)?;
    let mut qy =
        sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!("SELECT COUNT(u.Id) FROM Users u WHERE {cond}")));
    for a in args {
        qy = qy.bind(a);
    }
    Ok(json_raw(&json!(qy.fetch_one(&app.db).await?)))
}

/// What sorts a report. Go puts the requested column into its query as it comes; only these are taken here.
/// `model.UserReportSortColumns`, exactly: any other `sort_column` is refused, as `UserReportOptions.IsValid` does.
const REPORT_SORT: &[(&str, &str)] = &[
    ("CreateAt", "u.CreateAt"),
    ("Username", "u.Username"),
    ("FirstName", "u.FirstName"),
    ("LastName", "u.LastName"),
    ("Nickname", "u.Nickname"),
    ("Email", "u.Email"),
    ("Roles", "u.Roles"),
];

/// `GetReportDateRange` for `date_range` (`last_30_days`, `previous_month`, `last_6_months`, else all time) as the
/// `PostStats.Day` bounds Go joins on: local dates, the start inclusive, the end exclusive.
fn report_days(range: &str) -> String {
    use chrono::{Datelike, Duration, Local, NaiveDate};
    let now = Local::now().date_naive();
    // Go's `AddDate` normalizes an overflowing day into the next month (Aug 31 - 6 months = Mar 3).
    let add_months = |d: NaiveDate, months: i32| {
        let total = d.year() * 12 + d.month0() as i32 + months;
        let first = NaiveDate::from_ymd_opt(total.div_euclid(12), total.rem_euclid(12) as u32 + 1, 1).unwrap();
        first + Duration::days(d.day() as i64 - 1)
    };
    let (start, end) = match range {
        "last_30_days" => (Some(now - Duration::days(30)), None),
        "previous_month" => {
            let first = NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap();
            (Some(add_months(first, -1)), Some(first))
        }
        "last_6_months" => (Some(add_months(now, -6)), None),
        _ => (None, None),
    };
    let mut s = String::new();
    if let Some(d) = start {
        s.push_str(&format!(" AND ps.Day >= '{}'", d.format("%Y-%m-%d")));
    }
    if let Some(d) = end {
        s.push_str(&format!(" AND ps.Day < '{}'", d.format("%Y-%m-%d")));
    }
    s
}

/// `getUsersForReporting`: a page of people (not bots) with their activity over all time or `date_range`.
async fn user_report(St(app): St<State>, Auth(s): Auth, Query(q): Q) -> ApiResult<Response> {
    app.require(&s, "sysconsole_read_user_management_users").await?;
    let col = match query(&q, "sort_column") {
        "" => "Username",
        c => c,
    };
    let col = REPORT_SORT.iter().find(|(k, _)| *k == col).map(|(_, v)| *v);
    let prev = query(&q, "direction") == "prev";
    let desc = query(&q, "sort_direction") == "desc";
    let page_size = query(&q, "page_size").parse::<i64>().unwrap_or(50);
    let (cond, mut args) = user_report_filter(&q)?;
    if page_size <= 0 || page_size > 100 {
        return Err(AppError::bad_request("api.getUsersForReporting.invalid_page_size", ""));
    }
    let col = col.ok_or_else(|| {
        AppError::bad_request(
            "model.user_report_options.is_valid.invalid_sort_column",
            "Provided sort column is not valid.",
        )
    })?;
    let mut having = String::new();
    let mut dir = if desc { "DESC" } else { "ASC" };
    let (from_value, from_id) = (query(&q, "from_column_value"), query(&q, "from_id"));
    if !from_id.is_empty() && !from_value.is_empty() {
        args.push(from_value.to_string());
        let v = args.len();
        args.push(from_id.to_string());
        let i = args.len();
        let typed =
            if col.starts_with("u.") && col != "u.CreateAt" { format!("${v}") } else { format!("${v}::bigint") };
        let (cmp, d) = if prev != desc { ("<", "DESC") } else { (">", "ASC") };
        dir = d;
        having = format!("HAVING ({col} {cmp} {typed} OR ({col} = {typed} AND u.Id {cmp} ${i}))");
    }
    let days = report_days(query(&q, "date_range"));
    let sql = format!(
        "SELECT u.Id, MAX(s.LastActivityAt) AS LastStatusAt, MAX(ps.LastPostDate) AS LastPostDate, \
         COUNT(ps.Day) AS DaysActive, SUM(ps.NumPosts)::bigint AS TotalPosts, \
         (SELECT COUNT(*) FROM ChannelMembers cm INNER JOIN Channels c ON c.Id = cm.ChannelId AND c.DeleteAt = 0 \
          AND c.Type IN ('O','P') WHERE cm.UserId = u.Id) AS ChannelCount, \
         COALESCE((SELECT string_agg(t.DisplayName, ', ' ORDER BY t.DisplayName) FROM TeamMembers tm \
          INNER JOIN Teams t ON t.Id = tm.TeamId AND t.DeleteAt = 0 WHERE tm.UserId = u.Id AND tm.DeleteAt = 0), '') AS Teams \
         FROM Users u LEFT JOIN Status s ON s.UserId = u.Id LEFT JOIN PostStats ps ON ps.UserId = u.Id{days} \
         WHERE {cond} GROUP BY u.Id {having} ORDER BY {col} {dir}, u.Id {dir} LIMIT {page_size}"
    );
    type Row = (String, Option<i64>, Option<i64>, i64, Option<i64>, i64, String);
    let mut qy = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(sql));
    for a in args {
        qy = qy.bind(a);
    }
    let mut rows = qy.fetch_all(&app.db).await?;
    if prev {
        rows.reverse();
    }
    let ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
    let users = crate::users::query_users(&app, "", "u.Id = ANY($1)", "", vec![crate::users::Arg::V(ids)]).await?;
    // ClearNonProfileFields(true)
    const KEEP: &[&str] = &[
        "id",
        "create_at",
        "update_at",
        "delete_at",
        "username",
        "auth_service",
        "email",
        "nickname",
        "first_name",
        "last_name",
        "position",
        "roles",
        "notify_props",
        "locale",
        "timezone",
        "remote_id",
        "disable_welcome_email",
        "last_login",
    ];
    let out: Vec<Value> = rows
        .into_iter()
        .filter_map(|(id, status_at, post_date, days, posts, channels, teams)| {
            let u = users.iter().find(|u| u.id == id)?;
            let mut v = serde_json::to_value(u).ok()?;
            let m = v.as_object_mut()?;
            m.retain(|k, _| KEEP.contains(&k.as_str()));
            m.entry("remote_id").or_insert(json!(""));
            if let Some(t) = status_at {
                m.insert("last_status_at".into(), json!(t));
            }
            if let Some(t) = post_date {
                m.insert("last_post_date".into(), json!(t));
            }
            m.insert("days_active".into(), json!(days));
            if let Some(n) = posts {
                m.insert("total_posts".into(), json!(n));
            }
            m.insert("channel_count".into(), json!(channels));
            if !teams.is_empty() {
                m.insert("teams".into(), json!(teams));
            }
            Some(v)
        })
        .collect();
    Ok(json_raw(&Value::Array(out)))
}

// ---------- leftovers ----------

/// `/manualtest`: registered only with `EnableTesting` (otherwise the web client's page, as for any unknown path);
/// then Go's manual testing page, whose template a server build does not ship.
async fn manual_test(St(app): St<State>) -> Response {
    if !app.cfg.bool("ServiceSettings.EnableTesting") {
        return crate::web::root(&app).await;
    }
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "open root.html: no such file or directory\n",
    )
        .into_response()
}
