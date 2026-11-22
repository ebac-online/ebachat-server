//! The job server as an unlicensed Go server runs it: the `Jobs` table, the watcher that hands pending jobs to
//! their workers every 15 s, the schedulers that create jobs on their period (checked every minute), the status
//! changes and their `job_updated` events, and the `/jobs` routes. The workers themselves are in
//! `job_workers.rs`.
//!
//! The rows are Go's: the same types, statuses, progress and data, written by the same optimistic updates, so Go
//! and Rust can take turns on one database (one at a time: there is no cluster leader election, a single node is
//! always the leader).

// Based on Mattermost server/channels/jobs/jobs.go, server.go, schedulers.go, base_schedulers.go, workers.go,
// base_workers.go, jobs_watcher.go, batch_worker.go, store/sqlstore/job_store.go, app/job.go and api4/job.go,
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, AuthTrusted, State, is_valid_id, new_id, now_ms};
use crate::hub::Broadcast;
use crate::model::Session;

pub const PENDING: &str = "pending";
pub const IN_PROGRESS: &str = "in_progress";
pub const SUCCESS: &str = "success";
pub const ERROR: &str = "error";
pub const CANCEL_REQUESTED: &str = "cancel_requested";
pub const CANCELED: &str = "canceled";
pub const WARNING: &str = "warning";

/// `model.StringMap`: `null` when Go's map is nil.
pub type Data = BTreeMap<String, String>;

/// `model.Job`, in Go's field order.
#[derive(Serialize, Clone, Default, Debug)]
pub struct Job {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub priority: i64,
    pub create_at: i64,
    pub start_at: i64,
    pub last_activity_at: i64,
    pub status: String,
    pub progress: i64,
    pub data: Option<Data>,
}

impl Job {
    /// The data, made non-nil as Go's workers do before writing to it (`job.Data = make(model.StringMap)`).
    pub fn data_mut(&mut self) -> &mut Data {
        self.data.get_or_insert_with(Data::new)
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.data.as_ref().and_then(|d| d.get(k)).map(String::as_str)
    }
    /// `IsValidStatusChange`.
    fn is_valid_status_change(&self, new: &str) -> bool {
        match self.status.as_str() {
            IN_PROGRESS => new == PENDING || new == CANCEL_REQUESTED,
            PENDING => new == CANCEL_REQUESTED,
            CANCEL_REQUESTED => new == CANCELED,
            _ => false,
        }
    }
}

pub fn is_valid_status(s: &str) -> bool {
    matches!(s, PENDING | IN_PROGRESS | SUCCESS | ERROR | WARNING | CANCEL_REQUESTED | CANCELED)
}

// ---------- store (SqlJobStore) ----------

const COLS: &str = "Id, COALESCE(Type, ''), COALESCE(Priority, 0), COALESCE(CreateAt, 0), COALESCE(StartAt, 0),
    COALESCE(LastActivityAt, 0), COALESCE(Status, ''), COALESCE(Progress, 0), Data::text";

type Row = (String, String, i64, i64, i64, i64, String, i64, Option<String>);

fn from_row(r: Row) -> Job {
    let (id, t, priority, create_at, start_at, last_activity_at, status, progress, data) = r;
    let data = data.and_then(|d| serde_json::from_str::<Option<BTreeMap<String, Value>>>(&d).ok().flatten()).map(|m| {
        m.into_iter().map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect()
    });
    Job { id, r#type: t, priority, create_at, start_at, last_activity_at, status, progress, data }
}

/// `json.Marshal(job.Data)`: `null` for a nil map.
fn data_json(d: &Option<Data>) -> String {
    serde_json::to_string(d).unwrap_or_else(|_| "null".into())
}

async fn select(app: &App, cond: &str, binds: &[&str]) -> ApiResult<Vec<Job>> {
    let sql = format!("SELECT {COLS} FROM Jobs WHERE {cond}");
    let mut q = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(sql));
    for b in binds {
        q = q.bind(*b);
    }
    Ok(q.fetch_all(&app.db).await?.into_iter().map(from_row).collect())
}

async fn update_returning(app: &App, set: &str, cond: &str, binds: Vec<Value>) -> ApiResult<Vec<Job>> {
    let sql = format!("UPDATE Jobs SET {set} WHERE {cond} RETURNING {COLS}");
    let mut q = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(sql));
    for b in binds {
        q = match b {
            Value::Number(n) => q.bind(n.as_i64().unwrap_or_default()),
            Value::String(s) => q.bind(s),
            other => q.bind(other.to_string()),
        };
    }
    Ok(q.fetch_all(&app.db).await?.into_iter().map(from_row).collect())
}

/// `Job.Save`.
async fn save(app: &App, j: &Job) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO Jobs (Id, Type, Priority, CreateAt, StartAt, LastActivityAt, Status, Progress, Data)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::jsonb)",
    )
    .bind(&j.id)
    .bind(&j.r#type)
    .bind(j.priority)
    .bind(j.create_at)
    .bind(j.start_at)
    .bind(j.last_activity_at)
    .bind(&j.status)
    .bind(j.progress)
    .bind(data_json(&j.data))
    .execute(&app.db)
    .await?;
    Ok(())
}

/// `Job.SaveOnce`: only when no job of the type is pending or in progress (serializable, as Go).
async fn save_once(app: &App, j: &Job) -> ApiResult<bool> {
    let mut tx = app.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").execute(&mut *tx).await?;
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM Jobs WHERE Status IN ('pending', 'in_progress') AND Type = $1")
            .bind(&j.r#type)
            .fetch_one(&mut *tx)
            .await?;
    if n > 0 {
        return Ok(false);
    }
    let r = sqlx::query(
        "INSERT INTO Jobs (Id, Type, Priority, CreateAt, StartAt, LastActivityAt, Status, Progress, Data)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::jsonb)",
    )
    .bind(&j.id)
    .bind(&j.r#type)
    .bind(j.priority)
    .bind(j.create_at)
    .bind(j.start_at)
    .bind(j.last_activity_at)
    .bind(&j.status)
    .bind(j.progress)
    .bind(data_json(&j.data))
    .execute(&mut *tx)
    .await;
    // isRepeatableError: a concurrent save won; nothing saved.
    let repeatable =
        |e: &sqlx::Error| e.as_database_error().and_then(|d| d.code()).is_some_and(|c| c == "40001" || c == "40P01");
    match r {
        Err(e) if repeatable(&e) => return Ok(false),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    match tx.commit().await {
        Err(e) if repeatable(&e) => Ok(false),
        Err(e) => Err(e.into()),
        Ok(()) => Ok(true),
    }
}

/// `Job.UpdateOptimistically`: status, data and progress, if the row still has `current`.
pub async fn update_optimistically(app: &App, j: &Job, current: &str) -> ApiResult<Option<Job>> {
    let r = update_returning(
        app,
        "LastActivityAt = $1, Status = $2, Data = $3::jsonb, Progress = $4",
        "Id = $5 AND Status = $6",
        vec![
            json!(now_ms()),
            json!(j.status),
            json!(data_json(&j.data)),
            json!(j.progress),
            json!(j.id),
            json!(current),
        ],
    )
    .await?;
    Ok(if r.len() == 1 { r.into_iter().next() } else { None })
}

/// `Job.UpdateStatus`.
async fn update_status(app: &App, id: &str, status: &str) -> ApiResult<Job> {
    let r = update_returning(
        app,
        "Status = $1, LastActivityAt = $2",
        "Id = $3",
        vec![json!(status), json!(now_ms()), json!(id)],
    )
    .await?;
    if r.len() != 1 {
        return Err(AppError::not_found("app.job.update.app_error", "").detail(format!("Job not found id={id}")));
    }
    Ok(r.into_iter().next().unwrap_or_default())
}

/// `Job.UpdateStatusOptimistically`: `StartAt` too when the job starts.
async fn update_status_optimistically(app: &App, id: &str, current: &str, new: &str) -> ApiResult<Option<Job>> {
    let now = now_ms();
    let set = if new == IN_PROGRESS {
        "LastActivityAt = $1, Status = $2, StartAt = $1"
    } else {
        "LastActivityAt = $1, Status = $2"
    };
    let r =
        update_returning(app, set, "Id = $3 AND Status = $4", vec![json!(now), json!(new), json!(id), json!(current)])
            .await?;
    Ok(if r.len() == 1 { r.into_iter().next() } else { None })
}

/// `Job.Get`.
pub async fn get_job(app: &App, id: &str) -> ApiResult<Option<Job>> {
    Ok(select(app, "Id = $1", &[id]).await?.into_iter().next())
}

async fn count_pending(app: &App, t: &str) -> ApiResult<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM Jobs WHERE Status = 'pending' AND Type = $1")
        .bind(t)
        .fetch_one(&app.db)
        .await?)
}

/// `GetLastSuccessfulJobByType` (`GetNewestJobByStatusesAndType`).
async fn last_successful(app: &App, t: &str) -> ApiResult<Option<Job>> {
    Ok(select(app, "Status = 'success' AND Type = $1 ORDER BY CreateAt DESC LIMIT 1", &[t]).await?.into_iter().next())
}

/// `Job.GetAllByType`.
pub async fn get_all_by_type(app: &App, t: &str) -> ApiResult<Vec<Job>> {
    select(app, "Type = $1 ORDER BY CreateAt DESC", &[t]).await
}

/// `Job.GetAllByTypeAndStatus`.
pub async fn get_all_by_type_and_status(app: &App, t: &str, status: &str) -> ApiResult<Vec<Job>> {
    select(app, "Type = $1 AND Status = $2 ORDER BY CreateAt DESC", &[t, status]).await
}

/// `Job.GetByTypeAndData` (`Data->key = '"value"'`), unordered as Go's.
pub async fn get_by_type_and_data(app: &App, t: &str, data: &[(&str, &str)]) -> ApiResult<Vec<Job>> {
    let mut cond = "Type = $1".to_string();
    let mut binds: Vec<String> = vec![t.to_string()];
    for (k, v) in data {
        binds.push(k.to_string());
        binds.push(format!("\"{v}\""));
        cond += &format!(" AND Data->${} = ${}::jsonb", binds.len() - 1, binds.len());
    }
    let b: Vec<&str> = binds.iter().map(String::as_str).collect();
    select(app, &cond, &b).await
}

/// `Job.Cleanup`: finished jobs older than `expiry`, in batches.
pub async fn cleanup(app: &App, expiry: i64, batch: i64) -> ApiResult<()> {
    loop {
        let r = sqlx::query(
            "DELETE FROM Jobs WHERE Id IN (SELECT Id FROM Jobs WHERE CreateAt < $1 AND (Status != 'in_progress' AND Status != 'pending')
             ORDER BY CreateAt ASC LIMIT $2)",
        )
        .bind(expiry)
        .bind(batch)
        .execute(&app.db)
        .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        if r.rows_affected() == 0 {
            return Ok(());
        }
    }
}

// ---------- the job server ----------

/// `_createJob`: a pending job of a type some worker takes.
fn new_job(t: &str, data: Option<Data>) -> ApiResult<Job> {
    let id = new_id();
    if crate::job_workers::worker(t).is_none() {
        return Err(
            AppError::bad_request("model.job.is_valid.type.app_error", "Invalid job type").detail(format!("id={id}"))
        );
    }
    Ok(Job { id, r#type: t.into(), create_at: now_ms(), status: PENDING.into(), data, ..Default::default() })
}

/// `JobServer.CreateJob`.
pub async fn create_job(app: &App, t: &str, data: Option<Data>) -> ApiResult<Job> {
    let j = new_job(t, data)?;
    save(app, &j).await.map_err(|e| AppError::internal("app.job.save.app_error", e.detailed_error))?;
    Ok(j)
}

/// `JobServer.CreateJobOnce`: none if one of the type is already pending or in progress.
pub async fn create_job_once(app: &App, t: &str, data: Option<Data>) -> ApiResult<Option<Job>> {
    let j = new_job(t, data)?;
    let saved = save_once(app, &j).await.map_err(|e| AppError::internal("app.job.save.app_error", e.detailed_error))?;
    Ok(saved.then_some(j))
}

/// The permission a `job_updated` event of the type needs (`publishJobStatus`).
fn event_permission(t: &str) -> Option<&'static str> {
    Some(match t {
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
        | "cleanup_expired_access_tokens" => "read_jobs",
        "access_control_sync" => crate::perm::MANAGE_SYSTEM,
        "access_control_team_sync" => "manage_team_access_rules",
        _ => return None,
    })
}

/// `publishJobStatus`: `job_updated` with the job as JSON text, to sessions that may read it (sensitive data:
/// system admins when the type needs no particular permission).
fn publish(app: &App, j: &Job, status: &str) {
    let mut copy = j.clone();
    copy.status = status.into();
    let b = Broadcast {
        admin_only: true,
        required_permissions: event_permission(&j.r#type).into_iter().collect(),
        ..Default::default()
    };
    app.hub.publish("job_updated", json!({"job": serde_json::to_string(&copy).unwrap_or_default()}), b);
}

fn update_error(e: AppError) -> AppError {
    AppError::internal("app.job.update.app_error", e.detailed_error)
}

/// `ClaimJob`: pending → in progress, or none if another worker got it first.
pub async fn claim(app: &App, j: &Job) -> ApiResult<Option<Job>> {
    let r = update_status_optimistically(app, &j.id, PENDING, IN_PROGRESS).await.map_err(update_error)?;
    if let Some(j) = &r {
        publish(app, j, IN_PROGRESS);
    }
    Ok(r)
}

/// `SetJobProgress`.
pub async fn set_progress(app: &App, j: &mut Job, progress: i64) -> ApiResult<()> {
    j.status = IN_PROGRESS.into();
    j.progress = progress;
    if let Some(r) = update_optimistically(app, j, IN_PROGRESS).await.map_err(update_error)? {
        publish(app, &r, IN_PROGRESS);
    }
    Ok(())
}

async fn set_status(app: &App, j: &Job, status: &str) -> ApiResult<()> {
    let r = update_status(app, &j.id, status).await.map_err(update_error)?;
    publish(app, &r, status);
    Ok(())
}

/// `SetJobSuccess`.
pub async fn set_success(app: &App, j: &Job) -> ApiResult<()> {
    set_status(app, j, SUCCESS).await
}

/// `SetJobCanceled`.
pub async fn set_canceled(app: &App, j: &Job) -> ApiResult<()> {
    set_status(app, j, CANCELED).await
}

/// `SetJobPending`.
pub async fn set_pending(app: &App, j: &Job) -> ApiResult<()> {
    set_status(app, j, PENDING).await
}

/// `SetJobError`: with an error, its text in `data.error`, progress -1, from in progress (or cancel requested).
/// `error` is `AppError.Message`, then `DetailedError` and the wrapped error, joined with " — ".
pub async fn set_error(app: &App, j: &mut Job, error: Option<String>) -> ApiResult<()> {
    let Some(error) = error else {
        return set_status(app, j, ERROR).await;
    };
    j.status = ERROR.into();
    j.progress = -1;
    j.data_mut().insert("error".into(), error);
    let mut r = update_optimistically(app, j, IN_PROGRESS).await.map_err(update_error)?;
    if r.is_none() {
        r = update_optimistically(app, j, CANCEL_REQUESTED).await.map_err(update_error)?;
        if r.is_none() {
            return Err(AppError::internal("jobs.set_job_error.update.error", format!("id={}", j.id)));
        }
    }
    let r = r.unwrap_or_default();
    publish(app, &r, &r.status);
    Ok(())
}

/// `UpdateInProgressJobData`.
pub async fn update_in_progress_data(app: &App, j: &mut Job) -> ApiResult<()> {
    j.status = IN_PROGRESS.into();
    j.last_activity_at = now_ms();
    update_optimistically(app, j, IN_PROGRESS).await.map_err(update_error)?;
    Ok(())
}

/// `RequestCancellation`: a pending job is canceled at once, one in progress is asked to stop.
pub async fn request_cancellation(app: &App, id: &str) -> ApiResult<()> {
    if let Some(j) = update_status_optimistically(app, id, PENDING, CANCELED).await.map_err(update_error)? {
        publish(app, &j, CANCELED);
        return Ok(());
    }
    if let Some(j) = update_status_optimistically(app, id, IN_PROGRESS, CANCEL_REQUESTED).await.map_err(update_error)? {
        publish(app, &j, CANCEL_REQUESTED);
        return Ok(());
    }
    Err(AppError::internal("jobs.request_cancellation.status.error", format!("id={id}")))
}

/// `CancellationWatcher`'s check: the job was asked to stop.
pub async fn cancel_requested(app: &App, id: &str) -> bool {
    matches!(get_job(app, id).await, Ok(Some(j)) if j.status == CANCEL_REQUESTED)
}

/// `AppError.Error()` of `NewAppError(where, id, ...)` with a detail: what a wrapped error reads as.
pub fn go_error(where_: &str, id: &str, detail: &str) -> String {
    let msg = crate::i18n::t(id).unwrap_or(id);
    let mut s = format!("{where_}: {msg}");
    if !detail.is_empty() {
        s += ", ";
        s += detail;
    }
    s
}

/// `Job.Data["error"]` of `SetJobError(job, NewAppError(where, id, ...).Wrap(err))`: the message, then the wrapped
/// error after " — " (`AppError.Message`, not `Error()`, so `where` does not appear).
pub fn job_error(id: &str, wrapped: &str) -> String {
    let mut s = crate::i18n::t(id).unwrap_or(id).to_string();
    if !wrapped.is_empty() {
        s += " — ";
        s += wrapped;
    }
    s
}

// ---------- watcher, workers and schedulers ----------

/// Job types whose worker is busy with a job (a Go worker takes one job at a time from an unbuffered channel).
static BUSY: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn take(t: &str) -> bool {
    BUSY.lock().unwrap().get_or_insert_with(HashSet::new).insert(t.to_string())
}

fn release(t: &str) {
    if let Some(b) = BUSY.lock().unwrap().as_mut() {
        b.remove(t);
    }
}

/// `Watcher.PollAndNotify`: every pending job, oldest first, to its worker if the worker is enabled and idle.
async fn poll_and_notify(app: &State) {
    let Ok(jobs) = select(app, "Status = 'pending' ORDER BY CreateAt ASC", &[]).await else { return };
    for j in jobs {
        let Some(w) = crate::job_workers::worker(&j.r#type) else { continue };
        if !(w.enabled)(app) || !take(&j.r#type) {
            continue;
        }
        let app = app.clone();
        tokio::spawn(async move {
            let t = j.r#type.clone();
            let id = j.id.clone();
            crate::job_workers::do_job(&app, j).await;
            if std::env::var("LOG_JOBS").is_ok_and(|v| v == "1") {
                eprintln!("job {t} {id} done");
            }
            release(&t);
        });
    }
}

/// `Scheduler.NextScheduleTime`'s answer: when to create the next job, or never.
pub type Next = Option<chrono::DateTime<chrono::Local>>;

/// The scheduler of a type: `Enabled`, `NextScheduleTime` and `ScheduleJob`.
pub struct Scheduler {
    pub r#type: &'static str,
    pub enabled: fn(&App) -> bool,
    pub kind: SchedKind,
}

pub enum SchedKind {
    /// `PeriodicScheduler`: now, a random jitter under 2 s and the period; a job without data.
    Periodic(fn(&App) -> Duration),
    /// `DailyScheduler`: the next local time of day, `HH:MM` from the setting.
    Daily(&'static str),
    /// The migrations scheduler (`migrations/scheduler.go`).
    Migrations,
    /// `extract_content`: periodic, with `catchup: true`.
    ExtractContent,
    /// `product_notices`: `NoticesFetchFrequency` seconds from now, without jitter.
    Notices,
}

/// `getRandomDelay(jitterRange)`.
fn jitter() -> Duration {
    Duration::from_millis(rand::random_range(0..2000))
}

/// `GenerateNextStartDateTime`: today at the time, or tomorrow if that is not after now.
fn next_start(now: chrono::DateTime<chrono::Local>, hh_mm: &str) -> Next {
    use chrono::{Duration as D, TimeZone};
    let (h, m) = hh_mm.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    let t = chrono::NaiveTime::from_hms_opt(h, m, 0)?;
    let mut next = chrono::Local.from_local_datetime(&now.date_naive().and_time(t)).earliest()?;
    if now >= next {
        next += D::days(1);
    }
    Some(next)
}

struct SchedState {
    next: HashMap<&'static str, Next>,
    migrations_done: bool,
}

impl Scheduler {
    async fn next_time(&self, app: &App, st: &SchedState, now: chrono::DateTime<chrono::Local>) -> Next {
        let after = |d: Duration| Some(chrono::Local::now() + jitter() + d);
        match &self.kind {
            SchedKind::Periodic(p) => after(p(app)),
            SchedKind::ExtractContent => after(Duration::from_secs(3600)),
            SchedKind::Notices => {
                let secs = app.cfg.i64("AnnouncementSettings.NoticesFetchFrequency").max(0) as u64;
                Some(chrono::Local::now() + Duration::from_secs(secs))
            }
            SchedKind::Daily(setting) => next_start(now, app.cfg.str(setting)),
            SchedKind::Migrations => {
                if st.migrations_done {
                    None
                } else {
                    Some(chrono::Local::now() + Duration::from_secs(60))
                }
            }
        }
    }

    async fn schedule(&self, app: &State, st: &mut SchedState) -> ApiResult<()> {
        match &self.kind {
            SchedKind::Periodic(_) | SchedKind::Daily(_) | SchedKind::Notices => {
                create_job(app, self.r#type, None).await?;
            }
            SchedKind::ExtractContent => {
                create_job(app, self.r#type, Some(Data::from([("catchup".to_string(), "true".to_string())]))).await?;
            }
            SchedKind::Migrations => {
                if crate::job_workers::schedule_migrations(app).await? {
                    st.migrations_done = true;
                }
            }
        }
        Ok(())
    }
}

/// `setNextRunTime` (the pending count and last success are read as Go does, though no scheduler here uses them).
async fn set_next(app: &State, st: &mut SchedState, s: &Scheduler, now: chrono::DateTime<chrono::Local>) {
    if count_pending(app, s.r#type).await.is_err() || last_successful(app, s.r#type).await.is_err() {
        st.next.insert(s.r#type, None);
        return;
    }
    let n = s.next_time(app, st, now).await;
    st.next.insert(s.r#type, n);
}

/// `Schedulers.Start`: every minute, the due schedulers create their jobs; a config change sets the times again.
async fn run_schedulers(app: State) {
    let scheds = crate::job_workers::schedulers();
    let mut st = SchedState { next: HashMap::new(), migrations_done: false };
    let mut now = chrono::Local::now();
    for s in &scheds {
        if (s.enabled)(&app) {
            set_next(&app, &mut st, s, now).await;
        } else {
            st.next.insert(s.r#type, None);
        }
    }
    let mut cfg = app.cfg.value() as *const Value as usize;
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let current = app.cfg.value() as *const Value as usize;
        if current != cfg {
            // handleConfigChange: Go uses the `now` of the last tick.
            cfg = current;
            for s in &scheds {
                if (s.enabled)(&app) {
                    set_next(&app, &mut st, s, now).await;
                } else {
                    st.next.insert(s.r#type, None);
                }
            }
        }
        now = chrono::Local::now();
        for s in &scheds {
            let Some(Some(next)) = st.next.get(s.r#type).copied() else { continue };
            if chrono::Local::now() <= next || !(s.enabled)(&app) {
                continue;
            }
            // scheduleJob: Go reads the pending count and the last success first.
            if count_pending(&app, s.r#type).await.is_err() || last_successful(&app, s.r#type).await.is_err() {
                continue;
            }
            if let Err(e) = s.schedule(&app, &mut st).await {
                eprintln!("scheduler {}: failed to schedule job: {} {}", s.r#type, e.id, e.detailed_error);
                continue;
            }
            set_next(&app, &mut st, s, now).await;
        }
    }
}

/// `runJobs`: the recurring tasks, then the workers (`JobSettings.RunJobs`) and schedulers (`RunScheduler`).
pub fn spawn(app: State) {
    crate::job_workers::spawn_recurring_tasks(app.clone());
    if app.cfg.bool("JobSettings.RunJobs") {
        let a = app.clone();
        tokio::spawn(async move {
            // The watcher starts after a random delay under its 15 s interval, then polls every 15 s.
            tokio::time::sleep(Duration::from_millis(rand::random_range(0..15_000))).await;
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                poll_and_notify(&a).await;
            }
        });
    }
    if app.cfg.bool("JobSettings.RunScheduler") {
        tokio::spawn(run_schedulers(app));
    }
}

// ---------- permissions (app/job.go) ----------

/// `SessionHasPermissionToCreateJob` (types without access control policies: those need a license).
async fn create_permission(app: &App, s: &Session, j: &Job) -> Option<(bool, &'static str)> {
    let p = match j.r#type.as_str() {
        "data_retention" => "create_data_retention_job",
        "message_export" => "create_compliance_export_job",
        "elasticsearch_post_indexing" => "create_elasticsearch_post_indexing_job",
        "elasticsearch_post_aggregation" => "create_elasticsearch_post_aggregation_job",
        "ldap_sync" => "create_ldap_sync_job",
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
        | "extract_content"
        | "cleanup_expired_access_tokens"
        | "notify_expiring_access_tokens" => "manage_jobs",
        "access_control_team_sync" => {
            if app.has_perm(s, crate::perm::MANAGE_SYSTEM).await {
                return Some((true, crate::perm::MANAGE_SYSTEM));
            }
            if let Some(policy) = j.get("policy_id").filter(|p| is_valid_id(p))
                && app.has_team_perm(s, policy, "manage_team_access_rules").await
            {
                return Some((true, "manage_team_access_rules"));
            }
            return Some((false, crate::perm::MANAGE_SYSTEM));
        }
        "access_control_sync" => {
            if app.has_perm(s, crate::perm::MANAGE_SYSTEM).await {
                return Some((true, crate::perm::MANAGE_SYSTEM));
            }
            // The channel, team and policy checks of Go need access control policies, which need a license: no
            // policy exists on this server, so none of them grants anything.
            return Some((false, crate::perm::MANAGE_SYSTEM));
        }
        _ => return None,
    };
    Some((app.has_perm(s, p).await, p))
}

/// `SessionHasPermissionToManageJob`.
async fn manage_permission(app: &App, s: &Session, j: &Job) -> Option<(bool, &'static str)> {
    let p = match j.r#type.as_str() {
        "data_retention" => "manage_data_retention_job",
        "message_export" => "manage_compliance_export_job",
        "elasticsearch_post_indexing" => "manage_elasticsearch_post_indexing_job",
        "elasticsearch_post_aggregation" => "manage_elasticsearch_post_aggregation_job",
        "ldap_sync" => "manage_ldap_sync_job",
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
        | "extract_content"
        | "cleanup_expired_access_tokens" => "manage_jobs",
        "access_control_sync" => crate::perm::MANAGE_SYSTEM,
        "access_control_team_sync" => {
            if app.has_perm(s, crate::perm::MANAGE_SYSTEM).await {
                return Some((true, crate::perm::MANAGE_SYSTEM));
            }
            if let Some(policy) = j.get("policy_id").filter(|p| is_valid_id(p))
                && app.has_team_perm(s, policy, "manage_team_access_rules").await
            {
                return Some((true, "manage_team_access_rules"));
            }
            return Some((false, crate::perm::MANAGE_SYSTEM));
        }
        _ => return None,
    };
    Some((app.has_perm(s, p).await, p))
}

// ---------- routes (api4/job.go) ----------

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/jobs", post(create))
        .route("/api/v4/jobs/{job_id}/cancel", post(cancel))
        .route("/api/v4/jobs/{job_id}/download", get(download))
        .route("/api/v4/jobs/{job_id}/status", patch(update_job_status))
}

/// `[A-Za-z0-9]+` of the route: anything else is the router's 404.
fn route_id(path: &str, id: &str) -> ApiResult<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(crate::api::route_not_found(path));
    }
    Ok(())
}

/// `RequireJobId`.
fn require_job_id(id: &str) -> ApiResult<()> {
    if !is_valid_id(id) {
        return Err(AppError::invalid_url_param("job_id"));
    }
    Ok(())
}

/// `GetJob`.
async fn job_or_404(app: &App, id: &str) -> ApiResult<Job> {
    get_job(app, id).await.map_err(|e| AppError::internal("app.job.get.app_error", e.detailed_error))?.ok_or_else(
        || {
            AppError::not_found("app.job.get.app_error", "Unable to get the job.")
                .detail(format!("Job not found id={id}"))
        },
    )
}

/// A `model.Job` decoded as Go's `json.NewDecoder(r.Body).Decode(&job)`: wrong types are errors, `null` and
/// missing fields are zero values, unknown fields are ignored.
fn decode_job(body: &[u8]) -> Result<Job, String> {
    let v = serde_json::Deserializer::from_slice(body)
        .into_iter::<Value>()
        .next()
        .ok_or_else(|| "EOF".to_string())?
        .map_err(|e| e.to_string())?;
    let mut j = Job::default();
    let Value::Object(m) = v else {
        return if v.is_null() { Ok(j) } else { Err("json: cannot unmarshal into Go value of type model.Job".into()) };
    };
    let s = |k: &str| -> Result<String, String> {
        match m.get(k) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(format!("json: cannot unmarshal into Go struct field Job.{k} of type string")),
        }
    };
    let n = |k: &str| -> Result<i64, String> {
        match m.get(k) {
            None | Some(Value::Null) => Ok(0),
            Some(Value::Number(n)) => n
                .as_i64()
                .ok_or_else(|| format!("json: cannot unmarshal number into Go struct field Job.{k} of type int64")),
            Some(_) => Err(format!("json: cannot unmarshal into Go struct field Job.{k} of type int64")),
        }
    };
    j.id = s("id")?;
    j.r#type = s("type")?;
    j.priority = n("priority")?;
    j.create_at = n("create_at")?;
    j.start_at = n("start_at")?;
    j.last_activity_at = n("last_activity_at")?;
    j.status = s("status")?;
    j.progress = n("progress")?;
    j.data = match m.get("data") {
        None | Some(Value::Null) => None,
        Some(Value::Object(d)) => Some(
            d.iter()
                .map(|(k, v)| match v {
                    Value::String(s) => Ok((k.clone(), s.clone())),
                    Value::Null => Ok((k.clone(), String::new())),
                    _ => Err("json: cannot unmarshal into Go struct field Job.data of type string".to_string()),
                })
                .collect::<Result<Data, String>>()?,
        ),
        Some(_) => return Err("json: cannot unmarshal into Go struct field Job.data of type model.StringMap".into()),
    };
    Ok(j)
}

/// `createJob`.
async fn create(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Response> {
    let job = decode_job(&body).map_err(|e| AppError::invalid_param("job").detail(e))?;
    match create_permission(&app, &s, &job).await {
        None => {
            return Err(AppError::bad_request(
                "api.job.unable_to_create_job.incorrect_job_type",
                "The job type of the job you are trying to create is invalid",
            ));
        }
        Some((false, p)) => return Err(AppError::forbidden_perm(p)),
        Some((true, _)) => {}
    }
    // App.CreateJob: Elasticsearch and access control types need a license, so no worker takes them here and
    // the job server refuses them before their own steps would matter.
    let j = create_job(&app, &job.r#type, job.data).await?;
    Ok((StatusCode::CREATED, Json(j)).into_response())
}

/// `cancelJob`.
async fn cancel(St(app): St<State>, Auth(s): Auth, Path(job_id): Path<String>) -> ApiResult<Json<Value>> {
    route_id(&format!("/api/v4/jobs/{job_id}/cancel"), &job_id)?;
    require_job_id(&job_id)?;
    let j = job_or_404(&app, &job_id).await?;
    match create_permission(&app, &s, &j).await {
        None => {
            return Err(AppError::bad_request(
                "api.job.unable_to_create_job.incorrect_job_type",
                "The job type of the job you are trying to create is invalid",
            ));
        }
        Some((false, p)) => return Err(AppError::forbidden_perm(p)),
        Some((true, _)) => {}
    }
    request_cancellation(&app, &job_id).await?;
    Ok(Json(json!({"status": "OK"})))
}

/// `updateJobStatus`.
async fn update_job_status(
    St(app): St<State>,
    Auth(s): Auth,
    Path(job_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    route_id(&format!("/api/v4/jobs/{job_id}/status"), &job_id)?;
    require_job_id(&job_id)?;
    // StringInterfaceFromJSON: anything but an object is empty.
    let props: serde_json::Map<String, Value> = serde_json::Deserializer::from_slice(&body)
        .into_iter::<serde_json::Map<String, Value>>()
        .next()
        .and_then(Result::ok)
        .unwrap_or_default();
    let Some(status) = props.get("status").and_then(Value::as_str) else {
        return Err(AppError::invalid_param("status"));
    };
    let force = props.get("force").and_then(Value::as_bool).unwrap_or(false);
    let j = job_or_404(&app, &job_id).await?;
    match manage_permission(&app, &s, &j).await {
        None => {
            return Err(AppError::bad_request(
                "api.job.unable_to_manage_job.incorrect_job_type",
                "The job type of the job you are trying to manage is invalid",
            ));
        }
        Some((false, p)) => return Err(AppError::forbidden_perm(p)),
        Some((true, _)) => {}
    }
    if !force && !j.is_valid_status_change(status) {
        return Err(AppError::bad_request("api.job.status.invalid", "Invalid job status"));
    }
    // App.UpdateJobStatus.
    match status {
        PENDING => set_pending(&app, &j).await?,
        CANCEL_REQUESTED => request_cancellation(&app, &j.id).await?,
        CANCELED => set_canceled(&app, &j).await?,
        _ => return Err(AppError::internal("app.job.update_status.app_error", "")),
    }
    Ok(Json(json!({"status": "OK"})))
}

/// `downloadJob`: only compliance exports are downloadable, and they need a license; without one no such job
/// is ever made, so every job that exists is the wrong type.
async fn download(St(app): St<State>, AuthTrusted(s): AuthTrusted, Path(job_id): Path<String>) -> ApiResult<Response> {
    route_id(&format!("/api/v4/jobs/{job_id}/download"), &job_id)?;
    require_job_id(&job_id)?;
    if !app.cfg.bool("MessageExportSettings.DownloadExportResults") {
        return Err(AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "app.job.download_export_results_not_enabled",
            "DownloadExportResults in config.json is false. Please set this to true to download the results of this job.",
        ));
    }
    let j = job_or_404(&app, &job_id).await?;
    if j.r#type == "message_export" {
        if !app.has_perm(&s, "download_compliance_export_result").await {
            return Err(AppError::forbidden_perm("download_compliance_export_result"));
        }
        // A message export made by a licensed server that took a turn on the database: its file is not kept
        // in a form this server reads.
        return Err(AppError::not_found("api.job.unable_to_download_job", "Unable to download this job")
            .detail("job.Data did not include export_dir, export_dir was malformed, or jobId.zip wasn't found"));
    }
    Err(AppError::bad_request(
        "api.job.unable_to_download_job.incorrect_job_type",
        "The job type you are trying to download is not currently supported",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_start_is_today_or_tomorrow() {
        use chrono::TimeZone;
        let now = chrono::Local.with_ymd_and_hms(2026, 11, 20, 10, 30, 0).unwrap();
        assert_eq!(next_start(now, "11:00").unwrap(), chrono::Local.with_ymd_and_hms(2026, 11, 20, 11, 0, 0).unwrap());
        assert_eq!(next_start(now, "00:00").unwrap(), chrono::Local.with_ymd_and_hms(2026, 11, 21, 0, 0, 0).unwrap());
        assert_eq!(next_start(now, "10:30").unwrap(), chrono::Local.with_ymd_and_hms(2026, 11, 21, 10, 30, 0).unwrap());
        assert!(next_start(now, "1:00").is_none());
    }

    #[test]
    fn decodes_jobs_as_go() {
        let j = decode_job(br#"{"type":"plugins","data":{"a":"b"},"x":1}"#).unwrap();
        assert_eq!(j.r#type, "plugins");
        assert_eq!(j.get("a"), Some("b"));
        assert!(decode_job(br#"{"type":1}"#).is_err());
        assert!(decode_job(b"").is_err());
        assert_eq!(decode_job(b"null").unwrap().r#type, "");
    }

    #[test]
    fn data_is_null_when_nil() {
        assert_eq!(data_json(&None), "null");
        assert_eq!(data_json(&Some(Data::new())), "{}");
    }
}
