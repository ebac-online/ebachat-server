//! The workers and schedulers an unlicensed Go 12.1 server registers, the recurring tasks it runs outside the job
//! server (session, token, command webhook, job and config cleanup), and what it does at startup (guest
//! deactivation, the initial product notices fetch, the batch migrations' jobs).
//!
//! Every job type Go registers a worker for is known here, so `POST /jobs` accepts the same types. Types whose
//! worker needs a license (cloud, post history limits, persistent notifications, AI recaps behind a feature flag)
//! are registered disabled, as Go's `IsEnabled` answers without a license: their jobs stay pending.

// Based on Mattermost server/channels/app/server.go (runJobs, the recurring tasks, initJobs, guest deactivation),
// app/migrations.go (the batch migrations' jobs), app/expirynotify.go, app/expired_access_token_notify.go,
// app/notify_expiring_access_tokens.go, app/notify_admin.go, app/product_notices.go (UpdateProductNotices),
// app/user.go (DeactivateGuests), jobs/*/worker.go and scheduler.go, jobs/batch_migration_worker.go,
// jobs/migrations, and the sqlstore queries they run, Copyright (c) 2015-present Mattermost, Inc. Licensed under
// AGPL-3.0.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, State, now_ms};
use crate::db::Table;
use crate::jobs::{self, Data, Job, SchedKind, Scheduler};
use crate::model::{Post, Session};
use crate::posts::CreateFlags;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A registered worker: whether it runs with the settings in effect (`IsEnabled`).
pub struct Worker {
    pub enabled: fn(&App) -> bool,
}

fn always(_: &App) -> bool {
    true
}

fn never(_: &App) -> bool {
    false
}

fn import_retention(app: &App) -> bool {
    !app.cfg.str("ImportSettings.Directory").is_empty() && app.cfg.i64("ImportSettings.RetentionDays") > 0
}

fn export_retention(app: &App) -> bool {
    !app.cfg.str("ExportSettings.Directory").is_empty() && app.cfg.i64("ExportSettings.RetentionDays") > 0
}

fn notices_enabled(app: &App) -> bool {
    app.cfg.bool("AnnouncementSettings.AdminNoticesEnabled") || app.cfg.bool("AnnouncementSettings.UserNoticesEnabled")
}

/// `Config.AIRecapsEnabled`.
fn ai_recaps(app: &App) -> bool {
    app.cfg.feature("EnableAIRecaps") && app.cfg.get("AIRecapSettings.Enable").as_bool() != Some(false)
}

/// The worker of a job type, as `initJobs` registers them without a license.
pub fn worker(t: &str) -> Option<Worker> {
    let enabled: fn(&App) -> bool = match t {
        "migrations"
        | "plugins"
        | "import_process"
        | "export_process"
        | "delete_empty_drafts_migration"
        | "delete_orphan_drafts_migration"
        | "delete_dms_preferences_migration"
        | "extract_content"
        | "cleanup_desktop_tokens"
        | "cleanup_expired_access_tokens"
        | "notify_expiring_access_tokens"
        | "install_plugin_notify_admin"
        | "refresh_materialized_views" => always,
        // `!license.IsCloud()`: true without a license.
        "hosted_purchase_screening" => always,
        "expiry_notify" => |a: &App| a.cfg.bool("ServiceSettings.ExtendSessionLengthWithActivity"),
        "product_notices" => notices_enabled,
        "import_delete" => import_retention,
        "export_delete" => export_retention,
        "active_users" => |a: &App| a.cfg.bool("MetricsSettings.Enable"),
        "mobile_session_metadata" => |a: &App| a.cfg.bool("MetricsSettings.EnableClientMetrics"),
        "resend_invitation_email" => |a: &App| a.cfg.bool("ServiceSettings.EnableEmailInvitations"),
        "delete_expired_posts" => |a: &App| a.cfg.bool("ServiceSettings.EnableBurnOnRead"),
        "recap" | "scheduled_recap" => ai_recaps,
        // Cloud, a post history limit, a Professional license: never without a license. S3 path migration needs
        // MM_CLOUD_FILESTORE_BIFROST (Cloud's file store). The CSV user export needs a license to be created.
        "last_accessible_post"
        | "last_accessible_file"
        | "upgrade_notify_admin"
        | "trial_notify_admin"
        | "post_persistent_notifications"
        | "s3_path_migration"
        | "export_users_to_csv" => never,
        _ => return None,
    };
    Some(Worker { enabled })
}

/// The schedulers `initJobs` registers that can run without a license (the others are never enabled).
pub fn schedulers() -> Vec<Scheduler> {
    use SchedKind::*;
    vec![
        Scheduler { r#type: "migrations", enabled: always, kind: Migrations },
        Scheduler { r#type: "plugins", enabled: always, kind: Periodic(|_| Duration::from_secs(24 * 3600)) },
        Scheduler {
            r#type: "expiry_notify",
            enabled: |a| a.cfg.bool("ServiceSettings.ExtendSessionLengthWithActivity"),
            kind: Periodic(|_| Duration::from_secs(600)),
        },
        // Its own NextScheduleTime: now and NoticesFetchFrequency, without jitter.
        Scheduler { r#type: "product_notices", enabled: notices_enabled, kind: Notices },
        Scheduler {
            r#type: "import_delete",
            enabled: import_retention,
            kind: Periodic(|_| Duration::from_secs(24 * 3600)),
        },
        Scheduler {
            r#type: "export_delete",
            enabled: export_retention,
            kind: Periodic(|_| Duration::from_secs(24 * 3600)),
        },
        Scheduler {
            r#type: "active_users",
            enabled: |a| a.cfg.bool("MetricsSettings.Enable"),
            kind: Periodic(|_| Duration::from_secs(600)),
        },
        Scheduler {
            r#type: "mobile_session_metadata",
            enabled: |a| a.cfg.bool("MetricsSettings.EnableClientMetrics"),
            kind: Periodic(|_| Duration::from_secs(24 * 3600)),
        },
        Scheduler {
            r#type: "extract_content",
            enabled: |a| a.cfg.bool("FileSettings.ExtractContent"),
            kind: ExtractContent,
        },
        Scheduler {
            r#type: "install_plugin_notify_admin",
            enabled: always,
            kind: Periodic(|_| Duration::from_secs(24 * 3600)),
        },
        Scheduler { r#type: "cleanup_desktop_tokens", enabled: always, kind: Periodic(|_| Duration::from_secs(3600)) },
        Scheduler {
            r#type: "cleanup_expired_access_tokens",
            enabled: always,
            kind: Periodic(|_| Duration::from_secs(3600)),
        },
        Scheduler {
            r#type: "notify_expiring_access_tokens",
            enabled: always,
            kind: Periodic(|_| Duration::from_secs(3600)),
        },
        Scheduler {
            r#type: "refresh_materialized_views",
            enabled: always,
            kind: Daily("ServiceSettings.RefreshPostStatsRunTime"),
        },
        Scheduler {
            r#type: "delete_expired_posts",
            enabled: |a| a.cfg.feature("BurnOnRead") && a.cfg.bool("ServiceSettings.EnableBurnOnRead"),
            kind: Periodic(|a| {
                Duration::from_secs(a.cfg.i64("ServiceSettings.BurnOnReadSchedulerFrequencySeconds").max(0) as u64)
            }),
        },
    ]
}

// ---------- running a job ----------

/// `SimpleWorker.DoJob` and the other workers' `DoJob`: claim the job, run it, record the outcome.
pub async fn do_job(app: &State, job: Job) {
    match job.r#type.as_str() {
        "migrations" => migrations_job(app, job).await,
        "resend_invitation_email" => resend_invitation_job(app, job).await,
        "delete_empty_drafts_migration" | "delete_orphan_drafts_migration" | "delete_dms_preferences_migration" => {
            batch_migration_job(app, job).await
        }
        _ => simple_job(app, job).await,
    }
}

fn log(t: &str, what: &str, e: &AppError) {
    eprintln!("job {t}: {what}: {} {}", e.id, e.detailed_error);
}

/// `SimpleWorker.DoJob` (the plugins worker too, which skips the progress).
async fn simple_job(app: &State, job: Job) {
    let t = job.r#type.clone();
    let mut job = match jobs::claim(app, &job).await {
        Ok(Some(j)) => j,
        Ok(None) => return,
        Err(e) => return log(&t, "claim", &e),
    };
    if let Err(e) = execute(app, &mut job).await {
        // NewAppError("DoJob", "app.job.error", ...).Wrap(err), or the worker's own AppError (plugins).
        let msg = if t == "plugins" {
            e
        } else {
            format!("{} — {e}", crate::i18n::t("app.job.error").unwrap_or("app.job.error"))
        };
        if let Err(e) = jobs::set_error(app, &mut job, Some(msg)).await {
            log(&t, "set error", &e);
        }
        return;
    }
    if t != "plugins"
        && let Err(e) = jobs::set_progress(app, &mut job, 100).await
    {
        let _ = jobs::set_error(app, &mut job, Some(update_error_text(&e))).await;
    }
    if let Err(e) = jobs::set_success(app, &job).await {
        let _ = jobs::set_error(app, &mut job, Some(update_error_text(&e))).await;
    }
}

/// The `data.error` text of a failed status update (`app.job.update.app_error`).
fn update_error_text(e: &AppError) -> String {
    let mut s = e.message.clone();
    if !e.detailed_error.is_empty() {
        s += " — ";
        s += &e.detailed_error;
    }
    s
}

/// What a simple worker does; an error is the text of the error Go wraps.
async fn execute(app: &State, job: &mut Job) -> Result<(), String> {
    let db = |e: sqlx::Error| e.to_string();
    match job.r#type.as_str() {
        // cleanup_desktop_tokens: DesktopTokens.DeleteOlderThan, in seconds.
        "cleanup_desktop_tokens" => {
            let min = chrono::Utc::now().timestamp() - 5 * 60;
            sqlx::query("DELETE FROM DesktopTokens WHERE CreateAt < $1")
                .bind(min)
                .execute(&app.db)
                .await
                .map_err(|e| format!("failed to delete token row: {e}"))?;
            Ok(())
        }
        "cleanup_expired_access_tokens" => cleanup_expired_access_tokens(app).await,
        "notify_expiring_access_tokens" => notify_expiring_access_tokens(app).await,
        "plugins" => {
            // DeleteAllExpiredPluginKeys.
            sqlx::query("DELETE FROM PluginKeyValueStore WHERE ExpireAt <> 0 AND ExpireAt < $1")
                .bind(now_ms())
                .execute(&app.db)
                .await
                .map_err(|e| {
                    jobs::go_error(
                        "DeleteAllExpiredPluginKeys",
                        "app.plugin_store.delete.app_error",
                        &format!("failed to delete all expired PluginKeyValues: {e}"),
                    )
                })?;
            Ok(())
        }
        "refresh_materialized_views" => {
            for (view, what) in [
                ("posts_by_team_day", "error refreshing materialized view posts_by_team_day"),
                ("bot_posts_by_team_day", "error refreshing materialized view bot_posts_by_team_day"),
                ("file_stats", "error refreshing materialized view file_stats"),
                ("poststats", "users_refresh_post_stats_exec"),
            ] {
                sqlx::query(sqlx::AssertSqlSafe(format!("REFRESH MATERIALIZED VIEW {view}")))
                    .execute(&app.db)
                    .await
                    .map_err(|e| format!("{what}: {e}"))?;
            }
            Ok(())
        }
        // No metrics without a license: the count is read and dropped, as Go's worker does with no metrics.
        "active_users" => {
            sqlx::query("SELECT COUNT(*) FROM Users u LEFT JOIN Bots b ON b.UserId = u.Id WHERE u.DeleteAt = 0 AND b.UserId IS NULL")
                .execute(&app.db)
                .await
                .map_err(db)?;
            Ok(())
        }
        "mobile_session_metadata" => Ok(()),
        "expiry_notify" => notify_sessions_expired(app).await,
        "product_notices" => update_product_notices(app).await.map_err(|e| e.0),
        "import_delete" => retention_delete(app, false).await,
        "export_delete" => retention_delete(app, true).await,
        "install_plugin_notify_admin" => notify_admin_posts(app, false).await,
        "hosted_purchase_screening" => {
            let v: Option<String> =
                sqlx::query_scalar("SELECT Value FROM Systems WHERE Name = 'HostedPurchaseNeedsScreening'")
                    .fetch_optional(&app.db)
                    .await
                    .map_err(db)?;
            // System.GetByName's not-found error, which names an empty id.
            let v = v.ok_or_else(|| "resource \"System\" not found, id: name=".to_string())?;
            let at: i64 =
                v.parse().map_err(|e: std::num::ParseIntError| format!("strconv.ParseInt: parsing \"{v}\": {e}"))?;
            if now_ms() > at + 3 * DAY_MS {
                sqlx::query("DELETE FROM Systems WHERE Name = 'HostedPurchaseNeedsScreening'")
                    .execute(&app.db)
                    .await
                    .map_err(db)?;
            }
            Ok(())
        }
        "extract_content" => crate::extract::extract_content_job(app, job).await,
        "delete_expired_posts" => delete_expired_posts(app, job).await,
        "export_process" => crate::bulk_export::export_job(app, job).await,
        "import_process" => crate::bulk_import::import_job(app, job).await,
        // Licensed workers are never enabled here; nothing to do if one ever runs.
        _ => Ok(()),
    }
}

// ---------- the workers ----------

/// `GetSystemBot` as a session, and a direct message from it.
async fn system_bot_dm(app: &App, user_id: &str, mut post: Post, set_online: bool) -> ApiResult<()> {
    let bot = app.system_bot().await?;
    let bot_user = app.user(&bot).await?.ok_or_else(crate::users::missing_account)?;
    let s = Session { user_id: bot.clone(), roles: bot_user.roles.clone(), ..Default::default() };
    let ch = app.get_or_create_direct(&s, user_id, &bot).await?;
    post.channel_id = ch.id.clone();
    post.user_id = bot;
    app.create_post_in(post, &ch, CreateFlags { set_online, ..Default::default() }, None).await?;
    Ok(())
}

/// `resolveAccessTokenNotificationRecipient`: the owner of a user token, or the active owner of a user-owned bot,
/// with the bot's username.
async fn token_recipient(app: &App, user_id: &str) -> ApiResult<Option<(String, Option<String>)>> {
    let Some(u) = app.user(user_id).await? else {
        return Err(AppError::not_found("app.user.missing_account.const", ""));
    };
    if !u.is_bot {
        return Ok((u.delete_at == 0).then(|| (u.id.clone(), None)));
    }
    let owner: Option<(String,)> =
        sqlx::query_as("SELECT OwnerId FROM Bots WHERE UserId = $1").bind(user_id).fetch_optional(&app.db).await?;
    let Some((owner,)) = owner else { return Err(AppError::not_found("store.sql_bot.get.missing.app_error", "")) };
    match app.user(&owner).await? {
        Some(o) if o.delete_at == 0 => Ok(Some((o.id.clone(), Some(u.username.clone())))),
        _ => Ok(None),
    }
}

/// `cleanup_expired_access_tokens`: expired tokens (with their sessions) deleted in batches of 1000, owners told
/// by the system bot about tokens that expired in the last week.
async fn cleanup_expired_access_tokens(app: &App) -> Result<(), String> {
    let cutoff = now_ms();
    for _ in 0..1000 {
        let expired: Vec<(String, String, String, i64)> = sqlx::query_as(
            "SELECT Id, COALESCE(UserId, ''), COALESCE(Description, ''), ExpiresAt FROM UserAccessTokens
             WHERE ExpiresAt > 0 AND ExpiresAt <= $1 AND IsActive = true ORDER BY ExpiresAt ASC LIMIT 1000",
        )
        .bind(cutoff)
        .fetch_all(&app.db)
        .await
        .map_err(|e| format!("failed to find expired UserAccessTokens: {e}"))?;
        if expired.is_empty() {
            break;
        }
        notify_expired_tokens_deleted(app, &expired).await;
        let ids: Vec<String> = expired.iter().map(|t| t.0.clone()).collect();
        let mut tx = app.db.begin().await.map_err(|e| format!("begin_transaction: {e}"))?;
        sqlx::query("DELETE FROM Sessions WHERE Token IN (SELECT Token FROM UserAccessTokens WHERE Id = ANY($1))")
            .bind(&ids)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("failed to delete Sessions for UserAccessTokens: {e}"))?;
        sqlx::query("DELETE FROM UserAccessTokens WHERE Id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("failed to delete UserAccessTokens: {e}"))?;
        tx.commit().await.map_err(|e| e.to_string())?;
        let users: std::collections::BTreeSet<&String> = expired.iter().map(|t| &t.1).collect();
        for u in users {
            app.clear_sessions_for_user(u);
        }
        if expired.len() < 1000 {
            break;
        }
    }
    Ok(())
}

/// `NotifyExpiredAccessTokensDeleted`.
async fn notify_expired_tokens_deleted(app: &App, tokens: &[(String, String, String, i64)]) {
    if let Err(e) = app.system_bot().await {
        return log("cleanup_expired_access_tokens", "system bot", &e);
    }
    let floor = now_ms() - 7 * DAY_MS;
    for (_, user_id, description, expires_at) in tokens {
        if *expires_at < floor {
            continue;
        }
        let Ok(Some((recipient, bot))) = token_recipient(app, user_id).await else { continue };
        let message = match &bot {
            Some(b) => crate::i18n::tr(
                "app.user_access_token.bot_expired_deleted_notification",
                &[("BotUsername", b), ("Description", description)],
            ),
            None => {
                crate::i18n::tr("app.user_access_token.expired_deleted_notification", &[("Description", description)])
            }
        }
        .unwrap_or_default();
        let post = Post { message, ..Default::default() };
        if let Err(e) = system_bot_dm(app, &recipient, post, true).await {
            log("cleanup_expired_access_tokens", "notify", &e);
        }
    }
}

/// `NotifyExpiringAccessTokens`: a warning when a token enters its 7, 3 and 1 day buckets, once per bucket.
async fn notify_expiring_access_tokens(app: &App) -> Result<(), String> {
    let now = now_ms();
    let thresholds = [1i64, 3, 7];
    let actionable: Vec<String> = thresholds
        .iter()
        .map(|t| {
            let b = t * DAY_MS;
            format!(
                "(t.ExpiresAt <= {} AND (t.LastNotifiedAt IS NULL OR t.LastNotifiedAt < t.ExpiresAt - {b}))",
                now + b
            )
        })
        .collect();
    let owned_bot = "(b.UserId IS NOT NULL AND o.Id IS NOT NULL AND o.DeleteAt = 0)";
    let owner_filter = if app.cfg.bool("ServiceSettings.EnableUserAccessTokens") {
        format!("((b.UserId IS NULL AND u.DeleteAt = 0) OR {owned_bot})")
    } else {
        owned_bot.to_string()
    };
    let sql = format!(
        "SELECT t.Id, COALESCE(t.UserId, ''), COALESCE(t.Description, ''), t.ExpiresAt, t.LastNotifiedAt
         FROM UserAccessTokens t JOIN Users u ON u.Id = t.UserId LEFT JOIN Bots b ON b.UserId = t.UserId
         LEFT JOIN Users o ON o.Id = b.OwnerId
         WHERE t.IsActive = true AND t.ExpiresAt > $1 AND ({}) AND {owner_filter}
         ORDER BY t.ExpiresAt ASC LIMIT 1000",
        actionable.join(" OR ")
    );
    let tokens: Vec<(String, String, String, i64, Option<i64>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(now).fetch_all(&app.db).await.map_err(|e| {
            jobs::go_error(
                "NotifyExpiringAccessTokens",
                "app.user_access_token.get_expiring.app_error",
                &format!("failed to find expiring UserAccessTokens: {e}"),
            )
        })?;
    if tokens.is_empty() {
        return Ok(());
    }
    app.system_bot().await.map_err(|e| e.message)?;
    for (id, user_id, description, expires_at, last) in tokens {
        // accessTokenExpiryBucket
        let remaining = expires_at - now;
        let Some(bucket) = thresholds.iter().copied().find(|t| remaining > 0 && remaining <= t * DAY_MS) else {
            continue;
        };
        if last.is_some_and(|l| l >= expires_at - bucket * DAY_MS) {
            continue;
        }
        let Ok(Some((recipient, bot))) = token_recipient(app, &user_id).await else { continue };
        let unnamed = crate::i18n::t("app.notify_expiring_access_tokens.unnamed_token").unwrap_or("").to_string();
        let description = if description.is_empty() { unnamed } else { description };
        let days = bucket.to_string();
        let message = match (&bot, bucket <= 1) {
            (Some(b), true) => crate::i18n::tr(
                "app.notify_expiring_access_tokens.bot_dm_final",
                &[("BotUsername", b), ("Description", &description)],
            ),
            (Some(b), false) => crate::i18n::tr(
                "app.notify_expiring_access_tokens.bot_dm",
                &[("BotUsername", b), ("Description", &description), ("Days", &days)],
            ),
            (None, true) => {
                crate::i18n::tr("app.notify_expiring_access_tokens.dm_final", &[("Description", &description)])
            }
            (None, false) => crate::i18n::tr(
                "app.notify_expiring_access_tokens.dm",
                &[("Description", &description), ("Days", &days)],
            ),
        }
        .unwrap_or_default();
        if let Err(e) = system_bot_dm(app, &recipient, Post { message, ..Default::default() }, false).await {
            log("notify_expiring_access_tokens", "send", &e);
            continue;
        }
        let _ = sqlx::query("UPDATE UserAccessTokens SET LastNotifiedAt = $1 WHERE Id = $2")
            .bind(now)
            .bind(&id)
            .execute(&app.db)
            .await;
    }
    Ok(())
}

/// `NotifySessionsExpired`: a push to each mobile session that expired in the last hour, once, through the push
/// sender (`sendToPushProxy`: a `REMOVE` answer retires the device; after any failure the session is tried again).
async fn notify_sessions_expired(app: &App) -> Result<(), String> {
    if !crate::push::can_send(app) {
        return Ok(());
    }
    let now = now_ms();
    // GetSessionsExpired(OneHourMillis, mobileOnly, unnotifiedOnly)
    let sessions: Vec<Session> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{} WHERE ExpiresAt <> 0 AND ExpiresAt < $1 AND ExpiresAt > $2 AND DeviceId <> '' AND ExpiredNotify <> true",
        Session::SELECT
    )))
    .bind(now)
    .bind(now - 3_600_000)
    .fetch_all(&app.db)
    .await
    .map_err(|e| {
        jobs::go_error(
            "NotifySessionsExpired",
            "app.session.analytics_session_count.app_error",
            &format!("failed to find Sessions: {e}"),
        )
    })?;
    for s in sessions {
        let mut msg = crate::push::PushNotification {
            version: "v2".into(),
            kind: crate::push::SESSION.into(),
            ..Default::default()
        };
        msg.set_device(&s.device_id);
        msg.ack_id = crate::app::new_id();
        msg.message = session_expired_message(app, &s.user_id).await;
        if let Err(e) = crate::push::send_to_proxy(app, &mut msg, &s).await {
            eprintln!("Failed to send to push proxy: user_id={} session_id={} err={e}", s.user_id, s.id);
            continue;
        }
        if let Err(e) =
            sqlx::query("UPDATE Sessions SET ExpiredNotify = true WHERE Id = $1").bind(&s.id).execute(&app.db).await
        {
            eprintln!("Failed to update ExpiredNotify flag: sessionid={} err={e}", s.id);
        }
    }
    Ok(())
}

/// `getSessionExpiredPushMessage`: in the user's locale.
async fn session_expired_message(app: &App, user_id: &str) -> String {
    let locale = match app.user(user_id).await {
        Ok(Some(u)) => u.locale.clone(),
        _ => "en".to_string(),
    };
    let hours = app.cfg.i64("ServiceSettings.SessionLengthMobileInHours").to_string();
    crate::i18n::tl(
        &locale,
        "api.push_notifications.session.expired",
        &[("siteName", crate::brand::name()), ("hoursCount", &hours)],
    )
}

/// The ETag of the last notices fetch and its body (`noticesCache`, `utils.GetURLWithCache`).
static NOTICES_CACHE: std::sync::Mutex<Option<(String, bytes::Bytes)>> = std::sync::Mutex::new(None);

/// The ids of the notices of the last fetch (`cachedNotices`): a new user is marked as having seen them.
static NOTICE_IDS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub fn cached_notice_ids() -> Vec<String> {
    NOTICE_IDS.lock().unwrap().clone()
}

/// An error of `UpdateProductNotices`, as its text.
pub struct NoticesError(String);

/// `UpdateProductNotices`: the notices fetched (with the cached ETag), view states of notices no longer listed
/// dropped. The notices themselves are Mattermost's own product announcements, matched against Mattermost
/// versions; `GET /system/notices` keeps answering none.
pub async fn update_product_notices(app: &App) -> Result<(), NoticesError> {
    let url = app.cfg.str("AnnouncementSettings.NoticesURL").to_string();
    let skip = app.cfg.bool("AnnouncementSettings.NoticesSkipCache");
    let err = |id: &str, d: String| NoticesError(jobs::go_error("UpdateProductNotices", id, &d));
    let cached = NOTICES_CACHE.lock().unwrap().clone();
    let mut req = reqwest::Client::new().get(&url).timeout(Duration::from_secs(30));
    if let Some((etag, _)) = &cached
        && !skip
    {
        req = req.header("If-None-Match", etag.clone());
    }
    let r = req.send().await.map_err(|e| err("api.system.update_notices.fetch_failed", e.to_string()))?;
    let data = if r.status() == reqwest::StatusCode::NOT_MODIFIED && cached.is_some() {
        cached.map(|c| c.1).unwrap_or_default()
    } else {
        let etag = r.headers().get("etag").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = r.bytes().await.map_err(|e| err("api.system.update_notices.fetch_failed", e.to_string()))?;
        *NOTICES_CACHE.lock().unwrap() = Some((etag, body.clone()));
        body
    };
    let notices: Vec<serde_json::Map<String, Value>> =
        serde_json::from_slice(&data).map_err(|e| err("api.system.update_notices.parse_failed", e.to_string()))?;
    let ids: Vec<String> =
        notices.iter().filter_map(|n| n.get("id").and_then(Value::as_str).map(str::to_string)).collect();
    *NOTICE_IDS.lock().unwrap() = ids.clone();
    // ClearOldNotices: NotEq with no ids is "IS NOT NULL", every row.
    let q = if ids.is_empty() {
        sqlx::query("DELETE FROM ProductNoticeViewState WHERE NoticeId IS NOT NULL")
    } else {
        sqlx::query("DELETE FROM ProductNoticeViewState WHERE NoticeId <> ALL($1)").bind(ids)
    };
    q.execute(&app.db).await.map_err(|e| err("api.system.update_notices.clear_failed", e.to_string()))?;
    Ok(())
}

/// `import_delete` and `export_delete`: files of the directory older than the retention removed. Imports also
/// drop an incomplete upload's session, or a finished upload's FileInfo.
async fn retention_delete(app: &App, export: bool) -> Result<(), String> {
    let section = if export { "ExportSettings" } else { "ImportSettings" };
    let dir = app.cfg.str(&format!("{section}.Directory")).to_string();
    let retention = app.cfg.i64(&format!("{section}.RetentionDays")) * DAY_MS;
    let entries = app.files.list(&dir).await.map_err(|e| e.to_string())?;
    let now = now_ms();
    for (path, modified) in entries {
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        if export && !name.ends_with("_export.zip") {
            continue;
        }
        if now <= modified + retention {
            continue;
        }
        if !export {
            const INCOMPLETE: &str = ".tmp";
            if name.len() > 26 + 1 + INCOMPLETE.len()
                && name.ends_with(INCOMPLETE)
                && !name[..name.len() - 4].contains('.')
            {
                let upload_id = &name[..26];
                if sqlx::query("DELETE FROM UploadSessions WHERE Id = $1")
                    .bind(upload_id)
                    .execute(&app.db)
                    .await
                    .is_err()
                {
                    continue;
                }
            } else {
                let info: Option<(String,)> = match sqlx::query_as("SELECT Id FROM FileInfo WHERE Path = $1")
                    .bind(&path)
                    .fetch_optional(&app.db)
                    .await
                {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                if let Some((id,)) = info
                    && sqlx::query("DELETE FROM FileInfo WHERE Id = $1").bind(&id).execute(&app.db).await.is_err()
                {
                    continue;
                }
            }
        }
        let _ = app.files.remove(&path).await;
    }
    Ok(())
}

/// `delete_expired_posts`: burn-on-read posts past their expiry, scrubbed and deleted, their ids in the job's
/// data. Only a licensed server makes such posts; without one the table stays empty.
async fn delete_expired_posts(app: &App, job: &mut Job) -> Result<(), String> {
    let mut deleted: Vec<String> = vec![];
    let mut last = String::new();
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT PostId FROM TemporaryPosts WHERE ExpireAt <= $1 AND PostId > $2 ORDER BY PostId ASC LIMIT 100",
        )
        .bind(now_ms())
        .bind(&last)
        .fetch_all(&app.db)
        .await
        .map_err(|e| format!("failed to select expired TemporaryPosts: {e}"))?;
        let Some(l) = ids.last() else { break };
        last = l.clone();
        for id in ids {
            if app.permanent_delete_post_retain_stub(&id).await.is_ok() {
                deleted.push(id);
            }
        }
    }
    job.data_mut().insert("deleted_post_ids".into(), serde_json::to_string(&deleted).unwrap_or_default());
    Ok(())
}

impl App {
    /// `PermanentDeletePostDataRetainStub`: the post's files, edit history, persistent notification,
    /// acknowledgements, priority, reminders and thread data removed; the post kept as an empty, deleted stub.
    pub async fn permanent_delete_post_retain_stub(&self, post_id: &str) -> ApiResult<()> {
        let Some(post) = self.get_single_post(post_id, true).await? else { return Ok(()) };
        let files: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT Id, COALESCE(Path, ''), COALESCE(ThumbnailPath, ''), COALESCE(PreviewPath, '') FROM FileInfo WHERE PostId = $1",
        )
        .bind(post_id)
        .fetch_all(&self.db)
        .await?;
        for (id, path, thumb, preview) in &files {
            for p in [path, thumb, preview] {
                if !p.is_empty() {
                    let _ = self.files.remove(p).await;
                }
            }
            sqlx::query("DELETE FROM FileInfo WHERE Id = $1").bind(id).execute(&self.db).await?;
        }
        if !files.is_empty() {
            self.caches.file_infos.remove(post_id);
        }
        let history: Vec<String> =
            sqlx::query_scalar("SELECT Id FROM Posts WHERE OriginalId = $1").bind(post_id).fetch_all(&self.db).await?;
        for h in history {
            sqlx::query("DELETE FROM Posts WHERE Id = $1").bind(&h).execute(&self.db).await?;
        }
        for sql in [
            "DELETE FROM PersistentNotifications WHERE PostId = $1",
            "DELETE FROM PostAcknowledgements WHERE PostId = $1",
            "DELETE FROM PostsPriority WHERE PostId = $1",
            "DELETE FROM PostReminders WHERE PostId = $1",
            "DELETE FROM ThreadMemberships WHERE PostId = $1",
            "DELETE FROM Threads WHERE PostId = $1",
            "DELETE FROM Reactions WHERE PostId = $1",
            "DELETE FROM TemporaryPosts WHERE PostId = $1",
            "DELETE FROM ReadReceipts WHERE PostId = $1",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql)).bind(post_id).execute(&self.db).await?;
        }
        self.caches.reactions.remove(post_id);
        // scrubPost + Overwrite: no message, props, hashtags or files.
        sqlx::query(
            "UPDATE Posts SET Message = '', MessageSource = '', Hashtags = '', Props = '{}', FileIds = '[]', UpdateAt = $2,
             EditAt = $2 WHERE Id = $1",
        )
        .bind(post_id)
        .bind(now_ms())
        .execute(&self.db)
        .await?;
        if post.delete_at == 0 {
            self.delete_post(post_id, "").await?;
        }
        Ok(())
    }
}

/// `DoCheckForAdminNotifications` without a license (SKU "starter"): requests for paid features and plugins
/// sent to the system admins by the system bot, at most once per cool-off period.
async fn notify_admin_posts(app: &App, trial: bool) -> Result<(), String> {
    let key = if trial { "LAST_TRIAL_NOTIFICATION_TIMESTAMP" } else { "LAST_UPGRADE_NOTIFICATION_TIMESTAMP" };
    // CanNotifyAdmin
    let last: Option<String> = sqlx::query_scalar("SELECT Value FROM Systems WHERE Name = $1")
        .bind(key)
        .fetch_optional(&app.db)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(v) = last {
        let ok = match v.parse::<f64>() {
            Err(_) => false,
            Ok(t) => {
                let days = std::env::var("MM_NOTIFY_ADMIN_COOL_OFF_DAYS")
                    .ok()
                    .and_then(|d| d.parse::<f64>().ok())
                    .unwrap_or(14.0);
                (now_ms() - t as i64) >= (days * DAY_MS as f64) as i64
            }
        };
        if !ok {
            return Err(jobs::go_error(
                "SendNotifyAdminPosts",
                "app.notify_admin.send_notification_post.app_error",
                "Cannot notify yet",
            ));
        }
    }
    let admins: Vec<(String, String)> = sqlx::query_as(
        "SELECT Id, COALESCE(Locale, '') FROM Users WHERE Roles LIKE LOWER('%system\\_admin%') ORDER BY Username ASC LIMIT 100",
    )
    .fetch_all(&app.db)
    .await
    .map_err(|e| e.to_string())?;
    app.system_bot().await.map_err(|e| e.message)?;
    let now = now_ms();
    let rows: Vec<(String, i64, String, String, bool, Option<i64>)> = sqlx::query_as(
        "SELECT UserId, COALESCE(CreateAt, 0), COALESCE(RequiredPlan, ''), COALESCE(RequiredFeature, ''), COALESCE(Trial, false), SentAt
         FROM NotifyAdmin WHERE Trial = $1 AND (SentAt IS NULL)",
    )
    .bind(trial)
    .fetch_all(&app.db)
    .await
    .map_err(|e| jobs::go_error("SendNotifyAdminPosts", "app.notify_admin.send_notification_post.app_error", &e.to_string()))?;
    let rows: Vec<_> = rows.into_iter().filter(|r| r.2 != "starter").collect();
    if rows.is_empty() {
        return Ok(());
    }
    const PLUGIN: &str = "mattermost.feature.plugin";
    let as_json = |r: &(String, i64, String, String, bool, Option<i64>)| {
        let mut m = serde_json::Map::new();
        if r.1 != 0 {
            m.insert("create_at".into(), json!(r.1));
        }
        m.insert("user_id".into(), json!(r.0));
        m.insert("required_plan".into(), json!(r.2));
        m.insert("required_feature".into(), json!(r.3));
        m.insert("trial".into(), json!(r.4));
        m.insert("sent_at".into(), json!({"Int64": r.5.unwrap_or(0), "Valid": r.5.is_some()}));
        Value::Object(m)
    };
    let mut by_user: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_feature: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    type Row = (String, i64, String, String, bool, Option<i64>);
    let mut by_plugin: BTreeMap<String, Vec<&Row>> = BTreeMap::new();
    for r in &rows {
        *by_user.entry(&r.0).or_default() += 1;
        if r.3.starts_with(PLUGIN) {
            for p in r.2.split(',') {
                by_plugin.entry(p.to_string()).or_default().push(r);
            }
        } else {
            by_feature.entry(&r.3).or_default().push(as_json(r));
        }
    }
    if !by_feature.is_empty() {
        let n = by_user.len().to_string();
        let id = match (trial, by_user.len() == 1) {
            (false, false) => "app.cloud.upgrade_plan_bot_message",
            (false, true) => "app.cloud.upgrade_plan_bot_message_single",
            (true, false) => "app.cloud.trial_plan_bot_message",
            (true, true) => "app.cloud.trial_plan_bot_message_single",
        };
        let message = crate::i18n::tr(id, &[("UsersNum", &n), ("WorkspaceName", "")]).unwrap_or_default();
        for (admin, _) in &admins {
            let mut post =
                Post { message: message.clone(), r#type: "custom_up_notification".into(), ..Default::default() };
            post.props.0.insert("requested_features".into(), json!(by_feature));
            post.props.0.insert("trial".into(), json!(trial));
            if let Err(e) = system_bot_dm(app, admin, post, true).await {
                log("install_plugin_notify_admin", "post", &e);
            }
        }
    }
    // FinishSendAdminNotifyPost
    let _ =
        sqlx::query("INSERT INTO Systems (Name, Value) VALUES ($1, $2) ON CONFLICT (Name) DO UPDATE SET Value = $2")
            .bind(key)
            .bind(now_ms().to_string())
            .execute(&app.db)
            .await;
    for rows in by_plugin.values() {
        for r in rows {
            let _ = sqlx::query(
                "UPDATE NotifyAdmin SET SentAt = $1 WHERE UserId = $2 AND RequiredPlan = $3 AND RequiredFeature = $4",
            )
            .bind(now)
            .bind(&r.0)
            .bind(&r.2)
            .bind(&r.3)
            .execute(&app.db)
            .await;
        }
    }
    let _ = sqlx::query("DELETE FROM NotifyAdmin WHERE Trial = $1 AND CreateAt < $2 AND SentAt IS NULL")
        .bind(trial)
        .bind(now)
        .execute(&app.db)
        .await;
    Ok(())
}

// ---------- migrations (jobs/migrations) ----------

const PHASE_2: &str = "migration_advanced_permissions_phase_2";

/// The migrations scheduler's `ScheduleJob`: a job for the first migration not done; true once all are.
pub async fn schedule_migrations(app: &State) -> ApiResult<bool> {
    // GetMigrationState
    let done: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM Systems WHERE Name = $1)")
        .bind(PHASE_2)
        .fetch_one(&app.db)
        .await?;
    if done {
        return Ok(true);
    }
    let all = jobs::get_all_by_type(app, "migrations").await?;
    let last = all.into_iter().find(|j| j.get("migration_key") == Some(PHASE_2));
    if let Some(j) = &last
        && (j.status == jobs::IN_PROGRESS || j.status == jobs::PENDING)
    {
        // Wedged for an hour: failed, and scheduled again from where it was.
        let hour_ago = now_ms() - 3_600_000;
        if j.last_activity_at < hour_ago && j.create_at < hour_ago {
            let mut j = j.clone();
            let _ = jobs::set_error(app, &mut j, None).await;
            create_migration_job(app, Some(&j)).await?;
        }
        return Ok(false);
    }
    create_migration_job(app, last.as_ref()).await?;
    Ok(false)
}

async fn create_migration_job(app: &App, last: Option<&Job>) -> ApiResult<()> {
    let last_done = last.and_then(|j| j.get("last_done")).unwrap_or("").to_string();
    let data = Data::from([("migration_key".to_string(), PHASE_2.to_string()), ("last_done".to_string(), last_done)]);
    jobs::create_job(app, "migrations", Some(data)).await?;
    Ok(())
}

/// The migrations worker: a batch every second, its progress in `last_done`, until the migration is done (then
/// success, with progress left at 0) or the job is canceled.
async fn migrations_job(app: &State, job: Job) {
    let mut job = match jobs::claim(app, &job).await {
        Ok(Some(j)) => j,
        _ => return,
    };
    let mut last_watch = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if last_watch.elapsed() >= Duration::from_secs(5) {
            last_watch = std::time::Instant::now();
            if jobs::cancel_requested(app, &job.id).await {
                let _ = jobs::set_canceled(app, &job).await;
                return;
            }
        }
        let key = job.get("migration_key").unwrap_or("").to_string();
        let last_done = job.get("last_done").unwrap_or("").to_string();
        let r = if key == PHASE_2 {
            phase_2_batch(app, &last_done).await
        } else {
            Err(jobs::job_error("migrations.worker.run_migration.unknown_key", ""))
        };
        match r {
            Err(e) => {
                let _ = jobs::set_error(app, &mut job, Some(e)).await;
                return;
            }
            Ok((true, _)) => {
                if let Err(e) = sqlx::query("INSERT INTO Systems (Name, Value) VALUES ($1, 'true')")
                    .bind(&key)
                    .execute(&app.db)
                    .await
                {
                    let _ = jobs::set_error(
                        app,
                        &mut job,
                        Some(jobs::job_error("migrations.system.save.app_error", &e.to_string())),
                    )
                    .await;
                    return;
                }
                if let Err(e) = jobs::set_success(app, &job).await {
                    let _ = jobs::set_error(app, &mut job, Some(update_error_text(&e))).await;
                }
                return;
            }
            Ok((false, progress)) => {
                job.data_mut().insert("last_done".into(), progress);
                if let Err(e) = jobs::update_in_progress_data(app, &mut job).await {
                    let _ = jobs::set_error(app, &mut job, Some(update_error_text(&e))).await;
                    return;
                }
            }
        }
    }
}

/// `AdvancedPermissionsPhase2Progress`, in Go's field order.
#[derive(serde::Serialize, serde::Deserialize)]
struct Phase2 {
    current_table: String,
    last_team_id: String,
    last_channel_id: String,
    last_user: String,
}

/// `runAdvancedPermissionsPhase2Migration`: 100 team members, then 100 channel members at a time, their scheme
/// roles moved out of `Roles` into the `Scheme*` columns.
async fn phase_2_batch(app: &App, last_done: &str) -> Result<(bool, String), String> {
    let zero = "0".repeat(26);
    let invalid =
        |p: &str| jobs::job_error("migrations.worker.run_advanced_permissions_phase_2_migration.invalid_progress", p);
    let mut p = if last_done.is_empty() {
        Phase2 {
            current_table: "TeamMembers".into(),
            last_team_id: zero.clone(),
            last_channel_id: zero.clone(),
            last_user: zero.clone(),
        }
    } else {
        let p: Phase2 = serde_json::from_str(last_done).map_err(|e| invalid(&e.to_string()))?;
        let ok = [&p.last_channel_id, &p.last_team_id, &p.last_user].iter().all(|i| crate::app::is_valid_id(i))
            && (p.current_table == "TeamMembers" || p.current_table == "ChannelMembers");
        if !ok {
            return Err(invalid(""));
        }
        p
    };
    let json = |p: &Phase2| serde_json::to_string(p).unwrap_or_default();
    let (table, key, last_key) = if p.current_table == "TeamMembers" {
        ("TeamMembers", "TeamId", p.last_team_id.clone())
    } else {
        ("ChannelMembers", "ChannelId", p.last_channel_id.clone())
    };
    let (admin, user, guest) = if table == "TeamMembers" {
        ("team_admin", "team_user", "team_guest")
    } else {
        ("channel_admin", "channel_user", "channel_guest")
    };
    let err_id = if table == "TeamMembers" {
        "app.team.migrate_team_members.update.app_error"
    } else {
        "app.channel.migrate_channel_members.select.app_error"
    };
    let fail = |e: sqlx::Error| jobs::job_error(err_id, &e.to_string());
    let mut tx = app.db.begin().await.map_err(fail)?;
    type Member = (String, String, String, Option<bool>, Option<bool>, Option<bool>);
    let members: Vec<Member> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {key}, UserId, COALESCE(Roles, ''), SchemeAdmin, SchemeUser, SchemeGuest FROM {table}
         WHERE ({key}, UserId) > ($1, $2) ORDER BY {key}, UserId LIMIT 100"
    )))
    .bind(&last_key)
    .bind(&p.last_user)
    .fetch_all(&mut *tx)
    .await
    .map_err(fail)?;
    let Some(last) = members.last().cloned() else {
        tx.commit().await.map_err(fail)?;
        if table == "TeamMembers" {
            p.last_user = zero;
            p.current_table = "ChannelMembers".into();
            return Ok((false, json(&p)));
        }
        return Ok((true, json(&p)));
    };
    for (k, u, roles, sa, su, sg) in members {
        let (mut sa, mut su, mut sg) = (sa.unwrap_or(false), su.unwrap_or(false), sg.unwrap_or(false));
        let mut rest = vec![];
        for r in roles.split_whitespace() {
            if r == admin {
                sa = true;
            } else if r == user {
                su = true;
            } else if r == guest {
                sg = true;
            } else {
                rest.push(r);
            }
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET Roles = $1, SchemeAdmin = $2, SchemeUser = $3, SchemeGuest = $4 WHERE {key} = $5 AND UserId = $6"
        )))
        .bind(rest.join(" "))
        .bind(sa)
        .bind(su)
        .bind(sg)
        .bind(&k)
        .bind(&u)
        .execute(&mut *tx)
        .await
        .map_err(fail)?;
    }
    tx.commit().await.map_err(fail)?;
    if table == "TeamMembers" {
        p.last_team_id = last.0;
    } else {
        p.last_channel_id = last.0;
    }
    p.last_user = last.1;
    Ok((false, json(&p)))
}

// ---------- batch migrations (jobs/batch_migration_worker.go) ----------

/// `BatchWorker.DoJob` with `BatchMigrationWorker.doBatch`: a batch a second until done, the data carrying the
/// position (progress 0 meanwhile), then success with progress 100 and the migration's key in `Systems`.
async fn batch_migration_job(app: &State, job: Job) {
    let mut job = match jobs::claim(app, &job).await {
        Ok(Some(j)) => j,
        _ => return,
    };
    job.data_mut();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let data = job.data.clone().unwrap_or_default();
        match migration_batch(app, &job.r#type, &data).await {
            Err(e) => {
                // NewAppError("doMigrationBatch", NoTranslation, ...).Wrap(err).
                let _ = jobs::set_error(app, &mut job, Some(format!("<untranslated> — {e}"))).await;
                return;
            }
            Ok((true, _)) => {
                if let Err(e) = jobs::set_progress(app, &mut job, 100).await {
                    let _ = jobs::set_error(app, &mut job, Some(update_error_text(&e))).await;
                }
                if let Err(e) = jobs::set_success(app, &job).await {
                    let _ = jobs::set_error(app, &mut job, Some(update_error_text(&e))).await;
                }
                // markAsComplete
                let _ =
                    sqlx::query("INSERT INTO Systems (Name, Value) VALUES ($1, 'true') ON CONFLICT (Name) DO NOTHING")
                        .bind(&job.r#type)
                        .execute(&app.db)
                        .await;
                return;
            }
            Ok((false, next)) => {
                job.data = next;
                let _ = jobs::set_progress(app, &mut job, 0).await;
            }
        }
    }
}

/// One batch of a migration: done, or the job's next data (Go's nil for the preferences migration).
async fn migration_batch(app: &App, t: &str, data: &Data) -> Result<(bool, Option<Data>), String> {
    if t == "delete_dms_preferences_migration" {
        // DeleteInvalidVisibleDmsGms: limit_visible_dms_gms outside 1..40, 100 at a time.
        let r = sqlx::query(
            "DELETE FROM Preferences WHERE (userid, category, name) IN (SELECT UserId, Category, Name FROM Preferences
             WHERE Category = 'sidebar_settings' AND Name = 'limit_visible_dms_gms'
             AND (SUBSTRING(CONCAT('000000000000000', Value), LENGTH(Value) + 1, 15) > '000000000000040'
                  OR SUBSTRING(CONCAT('000000000000000', Value), LENGTH(Value) + 1, 15) < '000000000000001') LIMIT 100)",
        )
        .execute(&app.db)
        .await
        .map_err(|e| format!("failed to delete invalid limit_visible_dms_gms: failed to delete Preference: {e}"))?;
        return Ok((r.rows_affected() == 0, None));
    }
    let create_at: i64 = match data.get("create_at").map(String::as_str).unwrap_or("") {
        "" => 0,
        s => s.parse().map_err(|e| format!("failed to parse job metadata: failed to parse create_at: {e}"))?,
    };
    let user_id = data.get("user_id").cloned().unwrap_or_default();
    let next: Option<(i64, String)> = sqlx::query_as(
        "SELECT CreateAt, UserId FROM (SELECT CreateAt, UserId FROM Drafts WHERE CreateAt > $1 OR (CreateAt = $1 AND UserId > $2)
         ORDER BY CreateAt, UserId ASC LIMIT 100) d ORDER BY CreateAt DESC, UserId DESC LIMIT 1",
    )
    .bind(create_at)
    .bind(&user_id)
    .fetch_optional(&app.db)
    .await
    .map_err(|e| format!("failed to get the next batch (create_at={create_at}, user_id={user_id}): failed to get the list of drafts: {e}"))?;
    let Some((next_at, next_user)) = next else { return Ok((true, None)) };
    let cond = if t == "delete_empty_drafts_migration" {
        "d.Message = ''"
    } else {
        "(d.RootId IN (SELECT Id FROM Posts WHERE DeleteAt <> 0) OR NOT EXISTS (SELECT 1 FROM Posts WHERE Posts.Id = d.RootId))"
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "WITH dd AS (SELECT UserId, ChannelId, RootId FROM Drafts WHERE (CreateAt > $1 OR (CreateAt = $1 AND UserId > $2))
         ORDER BY CreateAt, UserId LIMIT 100)
         DELETE FROM Drafts d USING dd WHERE d.UserId = dd.UserId AND d.ChannelId = dd.ChannelId AND d.RootId = dd.RootId AND {cond}"
    )))
    .bind(create_at)
    .bind(&user_id)
    .execute(&app.db)
    .await
    .map_err(|e| format!("failed to delete drafts (create_at={create_at}, user_id={user_id}): {e}"))?;
    Ok((false, Some(Data::from([("create_at".to_string(), next_at.to_string()), ("user_id".to_string(), next_user)]))))
}

// ---------- resend_invitation_email ----------

/// The resend worker: the job is never claimed; once its 48 hours (`MM_RESEND_INVITATION_EMAIL_JOB_DURATION`)
/// are up, the invitations of emails that did not join yet are sent again, the job's `Systems` row is dropped
/// and the job succeeds.
async fn resend_invitation_job(app: &State, mut job: Job) {
    let scheduled: i64 = job.get("scheduledAt").and_then(|s| s.parse().ok()).unwrap_or(0);
    let duration = std::env::var("MM_RESEND_INVITATION_EMAIL_JOB_DURATION")
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(2 * DAY_MS);
    if now_ms() - scheduled <= duration {
        return;
    }
    // An error is recorded as Go's `SetJobError` does, which needs the job in progress: this one never is, so it
    // stays pending and is tried again at the next poll, as in Go.
    if let Err(e) = resend_emails(app, &job).await {
        let _ = jobs::set_error(app, &mut job, Some(e)).await;
        return;
    }
    // TearDown
    let _ = sqlx::query("DELETE FROM Systems WHERE Name = $1").bind(&job.id).execute(&app.db).await;
    if let Err(e) = jobs::set_success(app, &job).await {
        let _ = jobs::set_error(app, &mut job, Some(update_error_text(&e))).await;
    }
}

/// `ResendEmails`: the emails and channels of the job, less the addresses that joined the team since, invited
/// again with the "48" hours reminder (`InviteNewUsersToTeamGracefully`). The error is `data.error`'s text.
async fn resend_emails(app: &App, job: &Job) -> Result<(), String> {
    let werr = |e: String| format!("job_id: {} — {e}", job.id);
    let emails: Vec<String> =
        serde_json::from_str(job.get("emailList").unwrap_or("")).map_err(|e| werr(e.to_string()))?;
    let mut channels: Vec<String> = vec![];
    if let Some(c) = job.get("channelList").filter(|c| !c.is_empty()) {
        channels = serde_json::from_str(c).map_err(|e| werr(e.to_string()))?;
    }
    let team_id = job.get("teamID").unwrap_or("");
    // removeAlreadyJoined: GetUserByEmail, then GetTeamMembersByIds (active memberships).
    let mut pending = vec![];
    for email in emails {
        let joined: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM Users u JOIN TeamMembers tm ON tm.UserId = u.Id
             WHERE u.Email = LOWER(TRIM($1)) AND tm.TeamId = $2 AND tm.DeleteAt = 0)",
        )
        .bind(&email)
        .bind(team_id)
        .fetch_one(&app.db)
        .await
        .unwrap_or(false);
        if !joined {
            pending.push(email);
        }
    }
    if pending.is_empty() {
        return Ok(());
    }
    // Profiles kept for the remaining addresses need an Enterprise license (`validateAndNormalizeMemberInviteProfiles`).
    let mut profiles = false;
    if let Some(p) = job.get("profilesList").filter(|p| !p.is_empty()) {
        let list: Vec<Option<Value>> = serde_json::from_str(p).map_err(|e| werr(e.to_string()))?;
        let remaining: Vec<String> = pending.iter().map(|e| e.trim().to_lowercase()).collect();
        profiles = list.iter().any(|p| match p {
            None => true,
            Some(p) => remaining.contains(&p.get("email").and_then(Value::as_str).unwrap_or("").trim().to_lowercase()),
        });
    }
    let sender = job.get("senderID").unwrap_or("");
    let res = if profiles {
        resend_with_profiles(app, &pending, team_id, sender).await
    } else {
        crate::teams::invite_new_users_gracefully(app, pending, &channels, String::new(), team_id, sender, "48").await
    };
    let invites = res.map_err(|e| update_error_text(&e))?;
    for i in invites {
        if let Some(e) = i.error {
            return Err(update_error_text(&e));
        }
    }
    Ok(())
}

/// `InviteNewUsersToTeamGracefully` with invitee profiles: its checks, then the license error.
async fn resend_with_profiles(
    app: &App,
    emails: &[String],
    team_id: &str,
    sender: &str,
) -> ApiResult<Vec<crate::teams::EmailInviteWithError>> {
    if !app.cfg.bool("ServiceSettings.EnableEmailInvitations") {
        return Err(AppError::new(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "api.team.invite_members.disabled.app_error",
            "Email invitations are disabled.",
        ));
    }
    if emails.is_empty() {
        return Err(AppError::bad_request("api.team.invite_members.no_one.app_error", ""));
    }
    app.team(team_id).await?;
    app.user(sender).await?.ok_or_else(crate::users::missing_account)?;
    Err(AppError::bad_request("api.team.invite_members.profiles_license.app_error", ""))
}

// ---------- recurring tasks outside the job server (server.go) ----------

/// `model.CreateRecurringTask`: once now, then every period.
fn recurring<F, Fut>(app: &State, period: Duration, f: F)
where
    F: Fn(State) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let app = app.clone();
    tokio::spawn(async move {
        loop {
            f(app.clone()).await;
            tokio::time::sleep(period).await;
        }
    });
}

/// `runJobs`' recurring tasks: sessions (daily), tokens (hourly), command webhooks (hourly), finished jobs and
/// old configurations (daily, when their thresholds are set). The security bulletin check is not run (see
/// docs/JOBS.md); the license expiry check and Cloud's tasks do nothing without a license.
pub fn spawn_recurring_tasks(app: State) {
    recurring(&app, Duration::from_secs(24 * 3600), |app| async move {
        // Session.Cleanup: expired sessions, 1000 at a time.
        loop {
            let r = sqlx::query("DELETE FROM Sessions WHERE Id IN (SELECT Id FROM Sessions WHERE ExpiresAt != 0 AND $1 > ExpiresAt LIMIT 1000)")
                .bind(now_ms())
                .execute(&app.db)
                .await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !matches!(r, Ok(r) if r.rows_affected() > 0) {
                break;
            }
        }
    });
    recurring(&app, Duration::from_secs(3600), |app| async move {
        // Token.Cleanup: every token older than 48 hours but CWS access tokens.
        let _ = sqlx::query("DELETE FROM Tokens WHERE CreateAt < $1 AND Type <> 'cws_access_token'")
            .bind(now_ms() - 48 * 3_600_000)
            .execute(&app.db)
            .await;
    });
    recurring(&app, Duration::from_secs(3600), |app| async move {
        // CommandWebhook.Cleanup: older than 30 minutes.
        let _ = sqlx::query("DELETE FROM CommandWebhooks WHERE CreateAt < $1")
            .bind(now_ms() - 30 * 60_000)
            .execute(&app.db)
            .await;
    });
    recurring(&app, Duration::from_secs(24 * 3600), |app| async move {
        let days = app.cfg.i64("JobSettings.CleanupJobsThresholdDays");
        if days >= 0
            && let Err(e) = jobs::cleanup(&app, now_ms() - days * DAY_MS, 1000).await
        {
            log("cleanup", "jobs", &e);
        }
    });
    recurring(&app, Duration::from_secs(24 * 3600), |app| async move {
        // Configuration cleanup: only for a config in the database.
        let days = app.cfg.i64("JobSettings.CleanupConfigThresholdDays");
        let src = std::env::var("MM_CONFIG").unwrap_or_default();
        if days < 0 || !src.starts_with("postgres") {
            return;
        }
        let _ = sqlx::query(
            "DELETE FROM Configurations WHERE CreateAt < $1 AND (Active IS NULL OR Active = false)
             AND ID NOT IN (SELECT ID FROM Configurations ORDER BY CreateAt DESC LIMIT 5)",
        )
        .bind(now_ms() - days * DAY_MS)
        .execute(&app.db)
        .await;
    });
}

// ---------- startup ----------

/// What Go's `NewServer` does around `runJobs`: the first product notices fetch, guest accounts deactivated
/// when guests (or their magic links) are off, the batch migrations' jobs (`doAppMigrations`).
pub async fn startup(app: &State) {
    if notices_enabled(app) {
        let a = app.clone();
        tokio::spawn(async move {
            if let Err(e) = update_product_notices(&a).await {
                eprintln!("initial product notices fetch: {}", e.0);
            }
        });
    }
    if !app.cfg.bool("GuestAccountsSettings.Enable") {
        if let Err(e) = app.deactivate_guests(false).await {
            log("startup", "deactivate guests", &e);
        }
    } else if !app.cfg.bool("GuestAccountsSettings.EnableGuestMagicLink")
        && let Err(e) = app.deactivate_guests(true).await
    {
        log("startup", "deactivate magic link guests", &e);
    }
    for t in ["delete_empty_drafts_migration", "delete_orphan_drafts_migration", "delete_dms_preferences_migration"] {
        // doDelete*Migration: unless done, or a job is pending.
        let done: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM Systems WHERE Name = $1)")
            .bind(t)
            .fetch_one(&app.db)
            .await
            .unwrap_or(true);
        if done {
            continue;
        }
        match jobs::get_all_by_type_and_status(app, t, jobs::PENDING).await {
            Ok(p) if p.is_empty() => {
                if let Err(e) = jobs::create_job_once(app, t, None).await {
                    log(t, "create", &e);
                }
            }
            _ => {}
        }
    }
}

/// The config listener: guests deactivated when guest accounts (or their magic links) are turned off.
pub async fn config_changed(app: &App, old: &Value, new: &Value) {
    let b = |v: &Value, k: &str| {
        v.pointer(&format!("/GuestAccountsSettings/{k}")).and_then(Value::as_bool).unwrap_or(false)
    };
    if b(old, "Enable") && !b(new, "Enable") {
        if let Err(e) = app.deactivate_guests(false).await {
            log("config", "deactivate guests", &e);
        }
    } else if b(old, "EnableGuestMagicLink")
        && !b(new, "EnableGuestMagicLink")
        && let Err(e) = app.deactivate_guests(true).await
    {
        log("config", "deactivate magic link guests", &e);
    }
}

impl App {
    /// `DeactivateGuests` (`DeactivateMagicLinkGuests` with `magic_link`): the accounts deactivated in one update,
    /// their sessions revoked, `userDeactivated` for each, the caches cleared and `guests_deactivated` sent.
    pub async fn deactivate_guests(&self, magic_link: bool) -> ApiResult<()> {
        let now = now_ms();
        let err = |e: sqlx::Error| {
            AppError::internal("app.user.update_active_for_multiple_users.updating.app_error", e.to_string())
        };
        if magic_link {
            sqlx::query("UPDATE Users SET UpdateAt = $1, DeleteAt = $1, Roles = 'system_user' WHERE DeleteAt = 0 AND AuthService = 'magic_link'")
                .bind(now)
                .execute(&self.db)
                .await
                .map_err(err)?;
        } else {
            sqlx::query("UPDATE Users SET UpdateAt = $1, DeleteAt = $1 WHERE Roles = 'system_guest' AND DeleteAt = 0")
                .bind(now)
                .execute(&self.db)
                .await
                .map_err(err)?;
        }
        let ids: Vec<String> = sqlx::query_scalar("SELECT Id FROM Users WHERE DeleteAt = $1")
            .bind(now)
            .fetch_all(&self.db)
            .await
            .map_err(err)?;
        for id in &ids {
            self.caches.invalidate_user(id);
            self.revoke_all_sessions(id).await?;
        }
        for id in &ids {
            self.user_deactivated(id).await?;
        }
        if magic_link {
            let _ =
                sqlx::query("DELETE FROM Tokens WHERE Type = 'guest_magic_link_invitation'").execute(&self.db).await;
            let _ = sqlx::query("DELETE FROM Tokens WHERE Type = 'guest_magic_link'").execute(&self.db).await;
        }
        // Channel().ClearCaches() and User().ClearCaches().
        let c = &self.caches;
        c.channel.clear();
        c.channel_by_name.clear();
        c.member_count.clear();
        c.guest_count.clear();
        c.pinned_count.clear();
        c.members_for_user.clear();
        c.members_notify_props.clear();
        c.user.clear();
        c.all_profiles.clear();
        c.profiles_in_channel.clear();
        self.hub.publish("guests_deactivated", json!({}), Default::default());
        Ok(())
    }
}
